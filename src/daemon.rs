//! 常駐デーモン。計画 3.1〜3.7 の規則を実装する。
//!
//! ロック順(待ちが循環しないための規則):
//!   対象のサンドボックスロック(待つ)→ 受付ロック(待つ)→ 退去対象のサンドボックスロック(try_lock)
//! - 受付ロックの保持者は、他のサンドボックスロックを待たない。
//! - 定期処理は対象を try_lock で取り、取れなければその周期は見送る。
//! - hot を増やす操作(作成・起床)は必ず受付ロックの中で行う。

use crate::admission::{self, Decision, HotEntry};
use crate::config::Config;
use crate::lifecycle::{next_action, Action};
use crate::model::{now_ms, validate_name, NetworkMode, Sandbox, Tier};
use crate::protocol::{code, Request, Response, SandboxView};
use crate::reconcile::{resolve_all, resolve_one, Resolved};
use crate::runtime::{archive_ref, is_archive_of, ContainerRuntime, ContainerState, CreateSpec, RuntimeError};
use crate::store::Store;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub const DEFAULT_MEMORY_MB: u64 = 1024;

/// API エラー。`code` は protocol::code の値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        ApiError { code, message: message.into() }
    }
}

impl From<RuntimeError> for ApiError {
    fn from(e: RuntimeError) -> Self {
        match e {
            RuntimeError::NotManaged(_) => ApiError::new(code::NOT_MANAGED, e.to_string()),
            RuntimeError::NotFound(_) => ApiError::new(code::NOT_FOUND, e.to_string()),
            _ => ApiError::new(code::RUNTIME, e.to_string()),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::new(code::INTERNAL, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// 名前単位のロック。`lock` は待ち、`try_lock` は待たない。
#[derive(Default)]
pub struct NameLocks {
    busy: Mutex<HashSet<String>>,
    cv: Condvar,
}

pub struct NameGuard<'a> {
    locks: &'a NameLocks,
    name: String,
}

impl NameLocks {
    pub fn lock(&self, name: &str) -> NameGuard<'_> {
        let mut busy = self.busy.lock().unwrap();
        while busy.contains(name) {
            busy = self.cv.wait(busy).unwrap();
        }
        busy.insert(name.to_string());
        NameGuard { locks: self, name: name.to_string() }
    }

    pub fn try_lock(&self, name: &str) -> Option<NameGuard<'_>> {
        let mut busy = self.busy.lock().unwrap();
        if busy.contains(name) {
            return None;
        }
        busy.insert(name.to_string());
        Some(NameGuard { locks: self, name: name.to_string() })
    }
}

impl Drop for NameGuard<'_> {
    fn drop(&mut self) {
        self.locks.busy.lock().unwrap().remove(&self.name);
        self.locks.cv.notify_all();
    }
}

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;
type Liveness = Box<dyn Fn(i32, u64) -> bool + Send + Sync>;
type Logger = Box<dyn Fn(&str) + Send + Sync>;

pub struct Daemon {
    cfg: Config,
    rt: Arc<dyn ContainerRuntime>,
    store: Mutex<Store>,
    locks: NameLocks,
    admission: Mutex<()>,
    clock: Clock,
    alive: Liveness,
    logger: Logger,
    /// 過渡状態の読み直し間隔。
    transient_wait: Duration,
}

impl Daemon {
    pub fn new(cfg: Config, rt: Arc<dyn ContainerRuntime>, store: Store) -> Self {
        Daemon {
            cfg,
            rt,
            store: Mutex::new(store),
            locks: NameLocks::default(),
            admission: Mutex::new(()),
            clock: Box::new(now_ms),
            alive: Box::new(crate::proc::is_alive),
            logger: Box::new(|line| {
                let mut e = std::io::stderr().lock();
                let _ = writeln!(e, "{line}");
            }),
            transient_wait: Duration::from_secs(1),
        }
    }

    #[cfg(test)]
    fn with_hooks(mut self, clock: Clock, alive: Liveness, logger: Logger) -> Self {
        self.clock = clock;
        self.alive = alive;
        self.logger = logger;
        self.transient_wait = Duration::from_millis(1);
        self
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn log(&self, event: &str, fields: &[(&str, &str)]) {
        let mut line = format!("ts={} event={event}", self.now());
        for (k, v) in fields {
            line.push_str(&format!(" {k}={v}"));
        }
        (self.logger)(&line);
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    pub fn handle(&self, req: Request, conn_leases: &mut Vec<i64>) -> Response {
        let result = match req {
            Request::Create { name, image, cpus, memory_mb, internet } => {
                let net = if internet { NetworkMode::Internet } else { NetworkMode::Internal };
                self.create(&name, &image, cpus, memory_mb.unwrap_or(DEFAULT_MEMORY_MB), net).map(|_| Response::ok())
            }
            Request::Acquire { name, cli_pid, cli_start } => self.acquire(&name, cli_pid, cli_start).map(|id| {
                conn_leases.push(id);
                Response { lease_id: Some(id), ..Response::ok() }
            }),
            Request::Attach { lease_id, exec_pid, exec_start } => {
                self.attach(lease_id, exec_pid, exec_start).map(|_| {
                    if !conn_leases.contains(&lease_id) {
                        conn_leases.push(lease_id);
                    }
                    Response::ok()
                })
            }
            Request::Release { lease_id } => self.release(lease_id).map(|_| {
                conn_leases.retain(|l| *l != lease_id);
                Response::ok()
            }),
            Request::List => self.list().map(|v| Response { sandboxes: Some(v), ..Response::ok() }),
            Request::Remove { name } => self.remove(&name).map(|_| Response::ok()),
        };
        result.unwrap_or_else(|e| Response::error(e.code, e.message))
    }

    fn current(&self, name: &str) -> ApiResult<Option<Resolved>> {
        let container = self.rt.find(name)?;
        let row = self.store().get_sandbox(name)?;
        Ok(resolve_one(container.as_ref(), row.as_ref()))
    }

    /// 過渡状態なら読み直す。
    fn current_settled(&self, name: &str) -> ApiResult<Option<Resolved>> {
        for _ in 0..10 {
            match self.current(name)? {
                Some(Resolved::Managed { tier: Tier::Transient, .. }) => std::thread::sleep(self.transient_wait),
                other => return Ok(other),
            }
        }
        Err(ApiError::new(code::TRANSIENT, format!("{name} stayed in a transitional state; try again")))
    }

    // ---------- 作成 ----------

    pub fn create(&self, name: &str, image: &str, cpus: Option<u32>, memory_mb: u64, network: NetworkMode) -> ApiResult<()> {
        validate_name(name).map_err(|m| ApiError::new(code::INVALID, m))?;
        if memory_mb == 0 {
            return Err(ApiError::new(code::INVALID, "memory must be > 0"));
        }
        let _g = self.locks.lock(name);
        if self.store().get_sandbox(name)?.is_some() || self.rt.find(name)?.is_some() {
            return Err(ApiError::new(code::EXISTS, format!("{name} already exists")));
        }
        let _adm = self.admission.lock().unwrap();
        self.ensure_capacity(name, memory_mb)?;
        let spec = CreateSpec { name: name.into(), image: image.into(), cpus, memory_mb, network };
        self.rt.create(&spec)?;
        if let Err(e) = self.start_and_verify(name, true) {
            let _ = self.rt.remove(name);
            return Err(e);
        }
        let now = self.now();
        self.store().insert_sandbox(&Sandbox {
            name: name.into(),
            image: image.into(),
            cpus,
            memory_mb,
            network,
            last_access_ms: now,
            archive_ref: None,
            archive_gen: 0,
            created_at_ms: now,
        })?;
        self.log("create", &[("name", name), ("network", network.as_str())]);
        Ok(())
    }

    /// 起動し、稼働中であることを確かめる。`settle` なら少し待って再確認する
    /// (イメージに `sleep infinity` が無いと init がすぐ終了するため)。
    fn start_and_verify(&self, name: &str, settle: bool) -> ApiResult<()> {
        self.rt.start(name)?;
        let checks = if settle { 5 } else { 1 };
        for i in 0..checks {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            match self.rt.find(name)? {
                Some(c) if c.state == ContainerState::Running => {}
                _ => {
                    return Err(ApiError::new(
                        code::RUNTIME,
                        format!("{name} exited right after start; the image must provide `sleep infinity`"),
                    ))
                }
            }
        }
        Ok(())
    }

    // ---------- 受付判定 ----------

    /// 受付ロックを保持した状態で呼ぶ。必要なら退去し、足りなければ Reject。
    fn ensure_capacity(&self, target: &str, request_mb: u64) -> ApiResult<()> {
        let containers = self.rt.list()?;
        let rows = self.store().list_sandboxes()?;
        let resolved = resolve_all(&containers, &rows);
        let mut entries = Vec::new();
        let mut guards = Vec::new();
        for (name, r) in &resolved {
            if name == target {
                continue;
            }
            // 過渡状態のものは保守的に hot として数え、退去候補にはしない。
            let Resolved::Managed { tier: tier @ (Tier::Hot | Tier::Transient), row: Some(row) } = r else { continue };
            let mut evictable = false;
            if *tier == Tier::Hot && self.store().lease_count(name)? == 0 {
                if let Some(g) = self.locks.try_lock(name) {
                    // ロック取得後にリース数を再確認する(リースの記録は対象ロックの中で行われる)。
                    if self.store().lease_count(name)? == 0 {
                        evictable = true;
                        guards.push(g);
                    }
                }
            }
            entries.push(HotEntry {
                name: name.clone(),
                memory_mb: row.memory_mb,
                last_access_ms: row.last_access_ms,
                evictable,
            });
        }
        match admission::decide(&entries, request_mb, self.cfg.max_hot_memory_mb) {
            Decision::Admit => Ok(()),
            Decision::Evict(names) => {
                for n in names {
                    // 一覧取得の後に定期処理が停止を終えている場合がある。ロック保持中に読み直す。
                    if self.rt.find(&n)?.map(|c| c.state) == Some(ContainerState::Running) {
                        self.rt.stop(&n, self.cfg.stop_timeout_secs)?;
                    }
                    self.log("evict", &[("name", &n), ("for", target)]);
                }
                Ok(())
            }
            Decision::Reject { limit_mb, current_mb, request_mb } => Err(ApiError::new(
                code::REJECT,
                format!(
                    "admission rejected: limit {limit_mb} MB, hot total {current_mb} MB, requested {request_mb} MB \
                     (no idle hot sandbox can be stopped)"
                ),
            )),
        }
    }

    // ---------- リース ----------

    pub fn acquire(&self, name: &str, cli_pid: i32, cli_start: u64) -> ApiResult<i64> {
        let _g = self.locks.lock(name);
        match self.current_settled(name)? {
            None => Err(ApiError::new(code::NOT_FOUND, format!("{name} not found"))),
            Some(Resolved::Unmanaged) => Err(ApiError::new(
                code::NOT_MANAGED,
                format!("{name} is not managed by asb (no asb.managed=true label)"),
            )),
            Some(Resolved::Managed { tier: Tier::Orphan, .. }) => Err(ApiError::new(
                code::ORPHAN,
                format!("{name} has the asb label but no record; remove it with `asb rm {name}`"),
            )),
            Some(Resolved::Managed { tier: Tier::Lost, .. }) => Err(ApiError::new(
                code::LOST,
                format!("{name} has neither a container nor an archive; remove it with `asb rm {name}`"),
            )),
            Some(Resolved::Managed { tier: Tier::Hot, .. }) => self.record_lease(name, cli_pid, cli_start),
            Some(Resolved::Managed { tier, row: Some(row) }) if tier == Tier::Stopped || tier == Tier::Archived => {
                let _adm = self.admission.lock().unwrap();
                self.ensure_capacity(name, row.memory_mb)?;
                if tier == Tier::Stopped {
                    self.start_and_verify(name, false)?;
                } else {
                    self.restore(&row)?;
                }
                self.log("wake", &[("name", name), ("from", tier.as_str())]);
                self.record_lease(name, cli_pid, cli_start)
            }
            Some(Resolved::Managed { tier, .. }) => {
                Err(ApiError::new(code::INTERNAL, format!("{name}: unexpected state {}", tier.as_str())))
            }
        }
    }

    fn record_lease(&self, name: &str, cli_pid: i32, cli_start: u64) -> ApiResult<i64> {
        let now = self.now();
        let store = self.store();
        let id = store.insert_lease(name, cli_pid, cli_start, now)?;
        store.touch(name, now)?;
        Ok(id)
    }

    pub fn attach(&self, lease_id: i64, exec_pid: i32, exec_start: u64) -> ApiResult<()> {
        if self.store().attach_lease(lease_id, exec_pid, exec_start)? {
            Ok(())
        } else {
            Err(ApiError::new(code::NOT_FOUND, format!("lease {lease_id} not found")))
        }
    }

    pub fn release(&self, lease_id: i64) -> ApiResult<()> {
        let store = self.store();
        if let Some(l) = store.get_lease(lease_id)? {
            store.delete_lease(lease_id)?;
            store.touch(&l.sandbox, self.now())?;
        }
        Ok(())
    }

    fn lease_alive(&self, l: &crate::model::Lease) -> bool {
        let cli = (self.alive)(l.cli_pid, l.cli_start);
        let exec = match (l.exec_pid, l.exec_start) {
            (Some(p), Some(s)) => (self.alive)(p, s),
            _ => false,
        };
        cli || exec
    }

    /// リースのプロセスが両方とも終了していれば解放し、最終アクセスを更新する。
    fn release_if_dead(&self, l: &crate::model::Lease, reason: &str) -> ApiResult<()> {
        if self.lease_alive(l) {
            return Ok(());
        }
        {
            let store = self.store();
            store.delete_lease(l.id)?;
            store.touch(&l.sandbox, self.now())?;
        }
        self.log("lease_released", &[("name", &l.sandbox), ("id", &l.id.to_string()), ("reason", reason)]);
        Ok(())
    }

    /// CLI と exec 子プロセスの両方が終了しているリースを解放する。
    pub fn prune_leases(&self) -> ApiResult<()> {
        let leases = self.store().list_leases()?;
        for l in &leases {
            self.release_if_dead(l, "dead")?;
        }
        Ok(())
    }

    /// 接続が切れたときに呼ぶ。切断だけでは解放せず、生存確認を即時に行う。
    pub fn on_disconnect(&self, lease_ids: &[i64]) {
        for id in lease_ids {
            // ストアのロックを先に手放す(if let の条件式の一時値は本体の終わりまで生きる)。
            let lease = self.store().get_lease(*id);
            if let Ok(Some(l)) = lease {
                let _ = self.release_if_dead(&l, "disconnect");
            }
        }
    }

    // ---------- 一覧・削除 ----------

    pub fn list(&self) -> ApiResult<Vec<SandboxView>> {
        let containers = self.rt.list()?;
        let rows = self.store().list_sandboxes()?;
        let mut out = Vec::new();
        for (name, r) in resolve_all(&containers, &rows) {
            if let Resolved::Managed { tier, row } = r {
                let leases = self.store().lease_count(&name)?;
                out.push(SandboxView {
                    tier: tier.as_str().into(),
                    last_access_ms: row.as_ref().map(|r| r.last_access_ms),
                    leases,
                    archive_ref: row.as_ref().and_then(|r| r.archive_ref.clone()),
                    image: row.as_ref().map(|r| r.image.clone()),
                    memory_mb: row.as_ref().map(|r| r.memory_mb),
                    cpus: row.as_ref().and_then(|r| r.cpus),
                    network: row.as_ref().map(|r| r.network.as_str().to_string()),
                    name,
                });
            }
        }
        Ok(out)
    }

    pub fn remove(&self, name: &str) -> ApiResult<()> {
        let _g = self.locks.lock(name);
        match self.current(name)? {
            None => Err(ApiError::new(code::NOT_FOUND, format!("{name} not found"))),
            Some(Resolved::Unmanaged) => Err(ApiError::new(
                code::NOT_MANAGED,
                format!("{name} is not managed by asb (no asb.managed=true label)"),
            )),
            Some(Resolved::Managed { tier, .. }) => {
                let leases = self.store().lease_count(name)?;
                if leases > 0 {
                    return Err(ApiError::new(code::BUSY, format!("{name} has {leases} running exec(s)")));
                }
                if matches!(tier, Tier::Hot | Tier::Stopped | Tier::Transient | Tier::Orphan) {
                    self.rt.remove(name)?;
                }
                self.remove_archives(name, None);
                self.store().delete_sandbox(name)?;
                self.log("remove", &[("name", name)]);
                Ok(())
            }
        }
    }

    // ---------- 退避 ----------

    /// `keep` 以外の `asb-archive/<name>` イメージをすべて削除する。失敗はログのみ。
    fn remove_archives(&self, name: &str, keep: Option<&str>) {
        let images = match self.rt.image_list() {
            Ok(i) => i,
            Err(e) => {
                self.log("archive_cleanup_failed", &[("name", name), ("error", &quote(&e.to_string()))]);
                return;
            }
        };
        for img in images {
            if is_archive_of(name, &img) && Some(img.as_str()) != keep {
                if let Err(e) = self.rt.image_remove(name, &img) {
                    self.log("archive_cleanup_failed", &[("name", name), ("image", &img), ("error", &quote(&e.to_string()))]);
                }
            }
        }
    }

    /// 対象ロックを保持した状態で呼ぶ。commit → 確認 → DB 確定 → rm → 旧世代削除。
    fn archive(&self, row: &Sandbox) -> ApiResult<()> {
        let generation = row.archive_gen + 1;
        let reference = archive_ref(&row.name, generation);
        if self.rt.image_exists(&reference)? {
            self.rt.image_remove(&row.name, &reference)?;
        }
        self.rt.commit(&row.name, &reference)?;
        if !self.rt.image_exists(&reference)? {
            return Err(ApiError::new(code::RUNTIME, format!("commit of {} did not produce {reference}", row.name)));
        }
        self.store().set_archive(&row.name, &reference, generation)?;
        self.rt.remove(&row.name)?;
        self.remove_archives(&row.name, Some(&reference));
        Ok(())
    }

    fn restore(&self, row: &Sandbox) -> ApiResult<()> {
        let image = row
            .archive_ref
            .clone()
            .ok_or_else(|| ApiError::new(code::LOST, format!("{} has no archive", row.name)))?;
        let spec = CreateSpec { name: row.name.clone(), image, cpus: row.cpus, memory_mb: row.memory_mb, network: row.network };
        self.rt.create(&spec)?;
        if let Err(e) = self.start_and_verify(&row.name, false) {
            let _ = self.rt.remove(&row.name);
            return Err(e);
        }
        Ok(())
    }

    // ---------- 定期処理 ----------

    pub fn tick(&self) -> ApiResult<()> {
        self.prune_leases()?;
        let containers = self.rt.list()?;
        let rows = self.store().list_sandboxes()?;
        let now = self.now();
        for (name, r) in resolve_all(&containers, &rows) {
            let Resolved::Managed { tier, row: Some(row) } = r else { continue };
            let leases = self.store().lease_count(&name)?;
            match next_action(tier, row.last_access_ms, now, leases, &self.cfg) {
                Action::NotDue => {}
                Action::SkipLease => self.log("skip_demote", &[("name", &name), ("reason", "lease")]),
                Action::Stop | Action::Archive => {
                    let Some(_g) = self.locks.try_lock(&name) else { continue };
                    if let Err(e) = self.demote_locked(&name) {
                        self.log("demote_failed", &[("name", &name), ("error", &quote(&e.message))]);
                    }
                }
            }
        }
        Ok(())
    }

    /// 対象ロックを保持した状態で、状態とリース数を読み直してから降格する。
    fn demote_locked(&self, name: &str) -> ApiResult<()> {
        let Some(Resolved::Managed { tier, row: Some(row) }) = self.current(name)? else { return Ok(()) };
        let leases = self.store().lease_count(name)?;
        match next_action(tier, row.last_access_ms, self.now(), leases, &self.cfg) {
            Action::NotDue => Ok(()),
            Action::SkipLease => {
                self.log("skip_demote", &[("name", name), ("reason", "lease")]);
                Ok(())
            }
            Action::Stop => {
                self.rt.stop(name, self.cfg.stop_timeout_secs)?;
                self.log("demote", &[("name", name), ("to", "stopped")]);
                Ok(())
            }
            Action::Archive => {
                self.archive(&row)?;
                self.log("demote", &[("name", name), ("to", "archived")]);
                Ok(())
            }
        }
    }
}

fn quote(s: &str) -> String {
    format!("{s:?}")
}

// ---------- サーバー ----------

fn handle_conn(daemon: Arc<Daemon>, stream: UnixStream) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    let mut conn_leases: Vec<i64> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => daemon.handle(req, &mut conn_leases),
            Err(e) => Response::error(code::INVALID, format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"ok":false}"#.into());
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
    daemon.on_disconnect(&conn_leases);
}

/// 二重起動を防ぐ排他ロック。戻り値の File を保持している間有効。
fn acquire_instance_lock(cfg: &Config) -> anyhow::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = cfg.db_path.with_extension("lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    // SAFETY: 有効なファイル記述子。
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    anyhow::ensure!(rc == 0, "another asb daemon is running (lock {} is held)", path.display());
    Ok(f)
}

pub fn run(cfg: Config) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::PermissionsExt;

    let _instance = acquire_instance_lock(&cfg)?;
    let rt = crate::runtime_cli::CliRuntime::system();
    if let Some(w) = rt.version_warning() {
        eprintln!("{w}");
    }
    rt.ensure_network().context("prepare asb-internal network")?;
    let store = Store::open(&cfg.db_path)?;
    let daemon = Arc::new(Daemon::new(cfg.clone(), Arc::new(rt), store));
    daemon.prune_leases().map_err(|e| anyhow::anyhow!(e.message))?;

    if cfg.socket_path.exists() {
        std::fs::remove_file(&cfg.socket_path).context("remove stale socket")?;
    }
    if let Some(dir) = cfg.socket_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(&cfg.socket_path).with_context(|| format!("bind {}", cfg.socket_path.display()))?;
    std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(0o600))?;
    daemon.log(
        "start",
        &[
            ("socket", &cfg.socket_path.display().to_string()),
            ("idle_stop_secs", &cfg.idle_stop_secs.to_string()),
            ("idle_archive_secs", &cfg.idle_archive_secs.to_string()),
            ("tick_secs", &cfg.tick_secs.to_string()),
            ("max_hot_memory_mb", &cfg.max_hot_memory_mb.to_string()),
        ],
    );

    let ticker = Arc::clone(&daemon);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(ticker.cfg.tick_secs));
        if let Err(e) = ticker.tick() {
            ticker.log("tick_failed", &[("error", &quote(&e.message))]);
        }
    });

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let d = Arc::clone(&daemon);
                std::thread::spawn(move || handle_conn(d, s));
            }
            Err(e) => daemon.log("accept_failed", &[("error", &quote(&e.to_string()))]),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_fake::FakeRuntime;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct Harness {
        d: Daemon,
        rt: Arc<FakeRuntime>,
        clock: Arc<AtomicI64>,
        dead: Arc<Mutex<HashSet<i32>>>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    fn cfg(limit: u64) -> Config {
        Config {
            idle_stop_secs: 10,
            idle_archive_secs: 100,
            tick_secs: 1,
            stop_timeout_secs: None,
            max_hot_memory_mb: limit,
            socket_path: PathBuf::from("/tmp/unused.sock"),
            db_path: PathBuf::from("/tmp/unused.db"),
        }
    }

    fn harness(limit: u64) -> Harness {
        let rt = Arc::new(FakeRuntime::new());
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let dead: Arc<Mutex<HashSet<i32>>> = Arc::default();
        let logs: Arc<Mutex<Vec<String>>> = Arc::default();
        let (c, dd, l) = (clock.clone(), dead.clone(), logs.clone());
        let d = Daemon::new(cfg(limit), rt.clone(), Store::in_memory().unwrap()).with_hooks(
            Box::new(move || c.load(Ordering::SeqCst)),
            Box::new(move |pid, _| !dd.lock().unwrap().contains(&pid)),
            Box::new(move |line| l.lock().unwrap().push(line.to_string())),
        );
        Harness { d, rt, clock, dead, logs }
    }

    impl Harness {
        fn advance(&self, secs: i64) {
            self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
        }
        fn kill(&self, pid: i32) {
            self.dead.lock().unwrap().insert(pid);
        }
        fn create(&self, name: &str, mb: u64) {
            self.d.create(name, "alpine:3.22", None, mb, NetworkMode::Internal).unwrap();
        }
        fn tier(&self, name: &str) -> String {
            self.d.list().unwrap().into_iter().find(|v| v.name == name).map(|v| v.tier).unwrap_or_default()
        }
        fn logged(&self, needle: &str) -> bool {
            self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
        }
    }

    #[test]
    fn create_makes_hot_and_rejects_duplicates_and_bad_names() {
        let h = harness(4096);
        h.create("a", 512);
        assert_eq!(h.tier("a"), "hot");
        assert_eq!(h.d.create("a", "alpine:3.22", None, 512, NetworkMode::Internal).unwrap_err().code, code::EXISTS);
        assert_eq!(h.d.create("Bad", "alpine:3.22", None, 512, NetworkMode::Internal).unwrap_err().code, code::INVALID);
    }

    #[test]
    fn idle_hot_stops_then_archives_then_wakes_from_archive() {
        let h = harness(4096);
        h.create("a", 512);
        h.advance(10);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "stopped");
        assert!(h.logged("event=demote name=a to=stopped"));
        h.advance(100);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "archived");
        assert!(h.rt.state_of("a").is_none(), "container must be removed");
        assert!(h.rt.image_list().unwrap().contains(&"asb-archive/a:g1".to_string()));
        // 起床
        let id = h.d.acquire("a", 100, 1).unwrap();
        assert_eq!(h.tier("a"), "hot");
        assert!(h.logged("event=wake name=a from=archived"));
        assert!(h.rt.ops().contains(&"create a asb-archive/a:g1".to_string()));
        h.d.release(id).unwrap();
        // 2 回目の退避で世代が進み、旧世代は消える
        h.advance(10);
        h.d.tick().unwrap();
        h.advance(100);
        h.d.tick().unwrap();
        let images = h.rt.image_list().unwrap();
        assert_eq!(images, vec!["asb-archive/a:g2".to_string()]);
    }

    #[test]
    fn lease_blocks_demotion_and_logs_skip() {
        let h = harness(4096);
        h.create("a", 512);
        let id = h.d.acquire("a", 100, 1).unwrap();
        h.advance(60);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "hot");
        assert!(h.logged("event=skip_demote name=a reason=lease"));
        h.d.release(id).unwrap();
        // 解放時に最終アクセスが更新されるので、すぐには止まらない
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "hot");
        h.advance(10);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "stopped");
    }

    #[test]
    fn lease_survives_while_exec_alive_even_if_cli_died() {
        let h = harness(4096);
        h.create("a", 512);
        let id = h.d.acquire("a", 100, 1).unwrap();
        h.d.attach(id, 200, 2).unwrap();
        h.kill(100);
        h.advance(60);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "hot", "exec child still alive");
        h.kill(200);
        h.d.tick().unwrap(); // リース解放 + 最終アクセス更新
        assert!(h.logged("event=lease_released name=a"));
        assert_eq!(h.tier("a"), "hot");
        h.advance(10);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "stopped");
    }

    #[test]
    fn disconnect_alone_does_not_release_live_lease() {
        let h = harness(4096);
        h.create("a", 512);
        let id = h.d.acquire("a", 100, 1).unwrap();
        h.d.on_disconnect(&[id]);
        assert_eq!(h.d.list().unwrap()[0].leases, 1);
        h.kill(100);
        h.d.on_disconnect(&[id]);
        assert_eq!(h.d.list().unwrap()[0].leases, 0);
    }

    #[test]
    fn admission_evicts_oldest_idle_hot() {
        let h = harness(1024);
        h.create("a", 512);
        h.advance(1);
        h.create("b", 512);
        h.advance(1);
        h.create("c", 512);
        assert_eq!((h.tier("a").as_str(), h.tier("b").as_str(), h.tier("c").as_str()), ("stopped", "hot", "hot"));
        assert!(h.logged("event=evict name=a for=c"));
        // 起床でも退去する
        let id = h.d.acquire("a", 100, 1).unwrap();
        assert_eq!((h.tier("a").as_str(), h.tier("b").as_str(), h.tier("c").as_str()), ("hot", "stopped", "hot"));
        h.d.release(id).unwrap();
    }

    #[test]
    fn admission_rejects_when_all_hot_are_leased() {
        let h = harness(1024);
        h.create("a", 512);
        h.create("b", 512);
        h.d.acquire("a", 100, 1).unwrap();
        h.d.acquire("b", 101, 1).unwrap();
        let e = h.d.create("c", "alpine:3.22", None, 512, NetworkMode::Internal).unwrap_err();
        assert_eq!(e.code, code::REJECT);
        assert!(e.message.contains("1024") && e.message.contains("512"), "{}", e.message);
        assert_eq!((h.tier("a").as_str(), h.tier("b").as_str()), ("hot", "hot"));
        assert!(h.rt.state_of("c").is_none());
    }

    #[test]
    fn unmanaged_containers_are_never_touched() {
        let h = harness(1024);
        h.rt.add_unmanaged("buildkit", ContainerState::Running);
        h.create("a", 512);
        // 同名の DB 行を持つ管理対象外コンテナ
        h.rt.state.lock().unwrap().containers.remove("a");
        h.rt.add_unmanaged("a", ContainerState::Running);
        assert_eq!(h.d.acquire("a", 1, 1).unwrap_err().code, code::NOT_MANAGED);
        assert_eq!(h.d.remove("a").unwrap_err().code, code::NOT_MANAGED);
        assert_eq!(h.d.remove("buildkit").unwrap_err().code, code::NOT_MANAGED);
        h.advance(1000);
        h.d.tick().unwrap();
        assert_eq!(h.rt.state_of("a"), Some(ContainerState::Running));
        assert_eq!(h.rt.state_of("buildkit"), Some(ContainerState::Running));
        assert!(h.d.list().unwrap().iter().all(|v| v.name != "buildkit" && v.name != "a"));
        assert!(!h.rt.ops().iter().any(|o| o.ends_with(" buildkit") || o == "stop a" || o == "rm a"));
    }

    #[test]
    fn failed_commit_keeps_container() {
        let h = harness(4096);
        h.create("a", 512);
        h.advance(10);
        h.d.tick().unwrap();
        h.rt.state.lock().unwrap().fail_commit = true;
        h.advance(100);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "stopped");
        assert!(h.logged("event=demote_failed name=a"));
        assert!(h.d.store().get_sandbox("a").unwrap().unwrap().archive_ref.is_none());
    }

    #[test]
    fn remove_deletes_container_archives_and_row_but_refuses_busy() {
        let h = harness(4096);
        h.create("a", 512);
        let id = h.d.acquire("a", 100, 1).unwrap();
        assert_eq!(h.d.remove("a").unwrap_err().code, code::BUSY);
        h.d.release(id).unwrap();
        h.advance(10);
        h.d.tick().unwrap();
        h.advance(100);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "archived");
        h.d.remove("a").unwrap();
        assert!(h.rt.image_list().unwrap().is_empty());
        assert!(h.d.list().unwrap().is_empty());
        assert_eq!(h.d.remove("a").unwrap_err().code, code::NOT_FOUND);
    }

    #[test]
    fn lost_and_orphan_are_reported() {
        let h = harness(4096);
        h.create("a", 512);
        h.rt.state.lock().unwrap().containers.remove("a");
        assert_eq!(h.tier("a"), "lost");
        assert_eq!(h.d.acquire("a", 1, 1).unwrap_err().code, code::LOST);
        h.d.remove("a").unwrap();
        // DB 行の無い管理コンテナ
        h.rt.create(&CreateSpec { name: "o".into(), image: "x".into(), cpus: None, memory_mb: 1, network: NetworkMode::Internal })
            .unwrap();
        assert_eq!(h.tier("o"), "orphan");
        assert_eq!(h.d.acquire("o", 1, 1).unwrap_err().code, code::ORPHAN);
        h.d.remove("o").unwrap();
        assert!(h.rt.state_of("o").is_none());
    }

    #[test]
    fn tick_skips_sandbox_locked_by_another_operation() {
        let h = harness(4096);
        h.create("a", 512);
        h.advance(10);
        let g = h.d.locks.try_lock("a").unwrap();
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "hot");
        drop(g);
        h.d.tick().unwrap();
        assert_eq!(h.tier("a"), "stopped");
    }

    #[test]
    fn concurrent_acquires_do_not_exceed_limit() {
        let h = Arc::new(harness(1024));
        h.create("a", 512);
        h.create("b", 512);
        h.create("c", 512); // a を退去
        h.d.acquire("b", 1, 1).unwrap();
        h.d.acquire("c", 2, 1).unwrap();
        // b, c がリース中: a の起床は Reject されるはず。並行に 4 本。
        let mut handles = vec![];
        for i in 0..4 {
            let h2 = Arc::clone(&h);
            handles.push(std::thread::spawn(move || h2.d.acquire("a", 10 + i, 1)));
        }
        let results: Vec<_> = handles.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(results.iter().all(|r| r.as_ref().unwrap_err().code == code::REJECT));
        let hot_mb: u64 = h.d.list().unwrap().iter().filter(|v| v.tier == "hot").map(|v| v.memory_mb.unwrap()).sum();
        assert!(hot_mb <= 1024);
    }

    #[test]
    fn name_locks_try_lock() {
        let l = NameLocks::default();
        let g = l.lock("a");
        assert!(l.try_lock("a").is_none());
        assert!(l.try_lock("b").is_some());
        drop(g);
        assert!(l.try_lock("a").is_some());
    }
}
