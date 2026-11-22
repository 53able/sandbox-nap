//! 実機の Apple Container に対する e2e テスト(計画 5 章)。
//!
//! 実行: `sh scripts/e2e.sh`(管理対象外コンテナの前後比較つき)
//! または `cargo test --features e2e --test e2e -- --test-threads=1`
//!
//! - サンドボックス名は `asb-e2e-<乱数>-<役割>`。
//! - 後始末は `asb.managed=true` かつ自分の接頭辞を持つコンテナ、
//!   `asb-archive/<自分の接頭辞>` のイメージ、`asb.e2e-decoy=1` かつ
//!   `asb-e2e-unmanaged-` 接頭辞の囮コンテナだけを対象にする。
//! - 状態の判定は期限つきポーリングで行う。期限は
//!   `しきい値 + 2 × tick + OP_MARGIN`。

use serde_json::Value;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const IMAGE: &str = "alpine:3.22";
const TICK_SECS: u64 = 1;
/// Phase 0 で測った最大の操作時間(archived からの復元 707 ms)の 2 倍を切り上げ、
/// 計測していない commit + rm の分を足した余裕。
const OP_MARGIN: Duration = Duration::from_secs(3);
const EXT: &str = "1.1.1.1";
const EXT_PORT: &str = "443";

fn deadline(threshold_secs: u64) -> Duration {
    Duration::from_secs(threshold_secs + 2 * TICK_SECS) + OP_MARGIN
}

fn unique_prefix() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    format!("asb-e2e-{:x}{:x}", std::process::id() % 0xfff, nanos % 0xffff)
}

// ---------- container CLI の読み取り ----------

fn container_json(args: &[&str]) -> Value {
    let out = Command::new("container").args(args).output().expect("run container");
    assert!(out.status.success(), "container {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("json")
}

fn container_entry(name: &str) -> Option<Value> {
    container_json(&["ls", "-a", "--format", "json"])
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["configuration"]["id"] == name)
        .cloned()
}

fn container_state(name: &str) -> Option<String> {
    container_entry(name).and_then(|c| c["status"]["state"].as_str().map(String::from))
}

fn in_running_list(name: &str) -> bool {
    container_json(&["ls", "--format", "json"]).as_array().unwrap().iter().any(|c| c["configuration"]["id"] == name)
}

fn images() -> Vec<String> {
    container_json(&["image", "ls", "--format", "json"])
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["configuration"]["name"].as_str())
        .map(|n| n.strip_prefix("docker.io/").unwrap_or(n).to_string())
        .collect()
}

fn wait_until(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if f() {
            return;
        }
        if start.elapsed() > within {
            panic!("timed out after {within:?} waiting for: {what}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------- デーモンと CLI ----------

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    prefix: String,
    daemon: Option<Child>,
}

impl Env {
    fn new() -> Self {
        assert!(Command::new("container").arg("--version").output().is_ok(), "container CLI is required");
        let dir = tempfile::Builder::new().prefix("asbe2e").tempdir_in("/tmp").unwrap();
        let home = dir.path().to_path_buf();
        Env { _dir: dir, home, prefix: unique_prefix(), daemon: None }
    }

    fn name(&self, role: &str) -> String {
        format!("{}-{role}", self.prefix)
    }

    fn log_path(&self) -> PathBuf {
        self.home.join("daemon.log")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    /// デーモンを起動し、応答するまで待つ。`settings` は環境変数(ASB_*)。
    fn start_daemon(&mut self, settings: &[(&str, &str)]) {
        assert!(self.daemon.is_none(), "daemon already running");
        let log = OpenOptions::new().create(true).append(true).open(self.log_path()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("daemon")
            .env("ASB_HOME", &self.home)
            .env("ASB_TICK_SECS", TICK_SECS.to_string())
            .envs(settings.iter().copied())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        wait_until("daemon answers", Duration::from_secs(15), || self.asb(&["ls", "--json"]).status.success());
    }

    fn kill_daemon(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_asb"));
        c.args(args).env("ASB_HOME", &self.home);
        c
    }

    fn asb(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.asb(args);
        assert!(
            o.status.success(),
            "asb {args:?} failed ({:?}): {}\n--- daemon log ---\n{}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr),
            self.log()
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn spawn_exec(&self, name: &str, cmd: &[&str]) -> Child {
        let mut args = vec!["exec", name, "--"];
        args.extend_from_slice(cmd);
        self.cmd(&args).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap()
    }

    fn create(&self, role: &str, extra: &[&str]) -> String {
        let n = self.name(role);
        let mut args = vec!["create", n.as_str(), "--image", IMAGE];
        args.extend_from_slice(extra);
        self.ok(&args);
        n
    }

    fn view(&self, name: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(&self.ok(&["ls", "--json"])).unwrap();
        v.as_array().unwrap().iter().find(|s| s["name"] == name).cloned()
    }

    fn tier(&self, name: &str) -> String {
        self.view(name).and_then(|v| v["tier"].as_str().map(String::from)).unwrap_or_default()
    }

    fn write_marker(&self, name: &str) -> String {
        let m = format!("marker-{}", unique_prefix());
        self.ok(&["exec", name, "--", "sh", "-c", &format!("echo {m} > /root/marker && sync")]);
        m
    }

    fn read_marker(&self, name: &str) -> String {
        self.ok(&["exec", name, "--", "cat", "/root/marker"]).trim().to_string()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_daemon();
        cleanup(&self.prefix);
    }
}

/// 自分の接頭辞を持つものだけを削除する。
fn cleanup(prefix: &str) {
    let Ok(out) = Command::new("container").args(["ls", "-a", "--format", "json"]).output() else { return };
    let list: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Array(vec![]));
    for c in list.as_array().into_iter().flatten() {
        let id = c["configuration"]["id"].as_str().unwrap_or_default();
        let labels = &c["configuration"]["labels"];
        let managed_ours = id.starts_with(prefix) && labels["asb.managed"] == "true";
        let decoy_ours = id.starts_with(&format!("asb-e2e-unmanaged-{}", &prefix["asb-e2e-".len()..]))
            && labels["asb.e2e-decoy"] == "1";
        if managed_ours || decoy_ours {
            let _ = Command::new("container").args(["rm", "-f", id]).output();
        }
    }
    let repo = format!("asb-archive/{prefix}");
    for img in images() {
        if img.starts_with(&repo) {
            let _ = Command::new("container").args(["image", "rm", &img]).output();
        }
    }
}

// ---------- AC-1 ----------

#[test]
fn auto_stop() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    let n = e.create("a", &["--memory", "256"]);
    assert_eq!(container_state(&n).as_deref(), Some("running"));
    wait_until("stopped", deadline(3), || container_state(&n).as_deref() == Some("stopped"));
    assert!(!in_running_list(&n));
    assert_eq!(e.tier(&n), "stopped");
}

// ---------- AC-2 ----------

#[test]
fn wake_on_exec() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    let n = e.create("a", &["--memory", "256"]);
    let m = e.write_marker(&n);
    wait_until("stopped", deadline(3), || container_state(&n).as_deref() == Some("stopped"));
    let out = e.asb(&["exec", &n, "--", "cat", "/root/marker"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), m);
    assert_eq!(container_state(&n).as_deref(), Some("running"));
    let out = e.asb(&["exec", &n, "--", "sh", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7));
}

// ---------- AC-3 ----------

#[test]
fn archive_restore() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "2"), ("ASB_IDLE_ARCHIVE_SECS", "4")]);
    let n = e.create("a", &["--memory", "256"]);
    let m = e.write_marker(&n);
    wait_until("container removed (archived)", deadline(4), || container_entry(&n).is_none());
    assert_eq!(e.tier(&n), "archived");
    assert!(images().iter().any(|i| i.starts_with(&format!("asb-archive/{n}:"))));
    assert_eq!(e.read_marker(&n), m);
    assert_eq!(container_state(&n).as_deref(), Some("running"));
}

// ---------- AC-4 ----------

#[test]
fn no_demote_while_exec() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    let n = e.create("a", &["--memory", "256"]);
    let t0 = Instant::now();
    let mut child = e.spawn_exec(&n, &["sleep", "15"]);
    for at in [6u64, 9, 12] {
        let target = Duration::from_secs(at);
        if let Some(rest) = target.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
        assert_eq!(container_state(&n).as_deref(), Some("running"), "demoted at {at}s while exec was running");
    }
    assert!(
        e.log().contains(&format!("event=skip_demote name={n} reason=lease")),
        "daemon never evaluated the leased sandbox:\n{}",
        e.log()
    );
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    // 対照: exec の終了後は降格される
    wait_until("stopped after exec finished", deadline(3), || container_state(&n).as_deref() == Some("stopped"));
}

#[test]
fn refuses_unmanaged() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "2"), ("ASB_IDLE_ARCHIVE_SECS", "4")]);
    let d = format!("asb-e2e-unmanaged-{}", &e.prefix["asb-e2e-".len()..]);
    // asb で作って DB 行を持たせた後、同名の管理対象外コンテナに置き換える
    e.ok(&["create", &d, "--image", IMAGE, "--memory", "256"]);
    let rm = Command::new("container").args(["rm", "-f", &d]).output().unwrap();
    assert!(rm.status.success());
    let mk = Command::new("container")
        .args(["create", "--name", &d, "--label", "asb.e2e-decoy=1", "--init", "--entrypoint", "sleep", IMAGE, "infinity"])
        .output()
        .unwrap();
    assert!(mk.status.success(), "{}", String::from_utf8_lossy(&mk.stderr));
    assert!(Command::new("container").args(["start", &d]).output().unwrap().status.success());
    let started = container_entry(&d).unwrap()["status"]["startedDate"].clone();

    for args in [vec!["exec", d.as_str(), "--", "true"], vec!["rm", d.as_str()]] {
        let o = e.asb(&args);
        assert_ne!(o.status.code(), Some(0), "asb {args:?} must fail");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("not managed"), "asb {args:?} stderr: {err}");
    }
    std::thread::sleep(Duration::from_secs(12));
    let after = container_entry(&d).unwrap();
    assert_eq!(after["status"]["state"], "running");
    assert_eq!(after["status"]["startedDate"], started);
    // 囮の後始末(ラベルを確認してから)
    if after["configuration"]["labels"]["asb.e2e-decoy"] == "1" {
        let _ = Command::new("container").args(["rm", "-f", &d]).output();
    }
}

// ---------- AC-5 ----------

fn setup_three(e: &mut Env) -> (String, String, String) {
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3600"), ("ASB_IDLE_ARCHIVE_SECS", "7200"), ("ASB_MAX_HOT_MEMORY_MB", "1024")]);
    let a = e.create("a", &["--memory", "512"]);
    std::thread::sleep(Duration::from_millis(1100));
    let b = e.create("b", &["--memory", "512"]);
    std::thread::sleep(Duration::from_millis(1100));
    let c = e.create("c", &["--memory", "512"]);
    (a, b, c)
}

#[test]
fn admission_evict() {
    let mut e = Env::new();
    let (a, b, c) = setup_three(&mut e);
    assert_eq!(container_state(&a).as_deref(), Some("stopped"));
    assert_eq!(container_state(&b).as_deref(), Some("running"));
    assert_eq!(container_state(&c).as_deref(), Some("running"));
}

#[test]
fn admission_wake() {
    let mut e = Env::new();
    let (a, b, c) = setup_three(&mut e);
    e.ok(&["exec", &a, "--", "true"]);
    assert_eq!(container_state(&a).as_deref(), Some("running"));
    assert_eq!(container_state(&b).as_deref(), Some("stopped"));
    assert_eq!(container_state(&c).as_deref(), Some("running"));
}

#[test]
fn admission_reject() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3600"), ("ASB_IDLE_ARCHIVE_SECS", "7200"), ("ASB_MAX_HOT_MEMORY_MB", "1024")]);
    let a = e.create("a", &["--memory", "512"]);
    let b = e.create("b", &["--memory", "512"]);
    let mut ca = e.spawn_exec(&a, &["sleep", "10"]);
    let mut cb = e.spawn_exec(&b, &["sleep", "10"]);
    wait_until("both leases recorded", Duration::from_secs(10), || {
        e.view(&a).is_some_and(|v| v["leases"] == 1) && e.view(&b).is_some_and(|v| v["leases"] == 1)
    });
    let c = e.name("c");
    let o = e.asb(&["create", &c, "--image", IMAGE, "--memory", "512"]);
    assert_eq!(o.status.code(), Some(75));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("1024") && err.contains("512"), "stderr: {err}");
    assert_eq!(container_state(&a).as_deref(), Some("running"));
    assert_eq!(container_state(&b).as_deref(), Some("running"));
    assert!(container_entry(&c).is_none());
    assert_eq!(ca.wait().unwrap().code(), Some(0));
    assert_eq!(cb.wait().unwrap().code(), Some(0));
}

// ---------- AC-6 ----------

#[test]
fn network_modes() {
    let host = Command::new("nc").args(["-z", "-G", "5", EXT, EXT_PORT]).output().unwrap();
    assert!(host.status.success(), "precondition failed: host cannot reach {EXT}:{EXT_PORT}");
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3600"), ("ASB_IDLE_ARCHIVE_SECS", "7200")]);
    let internal = e.create("int", &["--memory", "256"]);
    let internet = e.create("net", &["--memory", "256", "--internet"]);
    let probe = format!("nc -w 5 {EXT} {EXT_PORT} </dev/null >/dev/null 2>&1");
    let o = e.asb(&["exec", &internal, "--", "sh", "-c", &probe]);
    assert_ne!(o.status.code(), Some(0), "internal sandbox reached {EXT}");
    let o = e.asb(&["exec", &internet, "--", "sh", "-c", &probe]);
    assert_eq!(o.status.code(), Some(0), "internet sandbox could not reach {EXT}");
}

// ---------- AC-7 ----------

#[test]
fn daemon_restart() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "6")]);
    let a = e.create("arch", &["--memory", "256"]);
    let ma = e.write_marker(&a);
    wait_until("A archived", deadline(6), || container_entry(&a).is_none());
    let s = e.create("stop", &["--memory", "256"]);
    let ms = e.write_marker(&s);
    let h = e.create("hot", &["--memory", "256"]);
    let mut hold = e.spawn_exec(&h, &["sleep", "30"]);
    wait_until("S stopped", deadline(3), || container_state(&s).as_deref() == Some("stopped"));

    // 再起動(stopped が確認前に archived へ進まないよう archive を長くする)
    e.kill_daemon();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    assert_eq!(e.tier(&h), "hot");
    assert_eq!(container_state(&h).as_deref(), Some("running"));
    assert_eq!(e.tier(&s), "stopped");
    assert_eq!(container_state(&s).as_deref(), Some("stopped"));
    assert_eq!(e.tier(&a), "archived");
    assert!(container_entry(&a).is_none());

    // AC-2: stopped からの起床、AC-3: archived からの復元
    assert_eq!(e.read_marker(&s), ms);
    assert_eq!(e.read_marker(&a), ma);
    // AC-1: 再び自動停止する
    wait_until("S stopped again", deadline(3), || container_state(&s).as_deref() == Some("stopped"));
    wait_until("A stopped again", deadline(3), || container_state(&a).as_deref() == Some("stopped"));
    assert_eq!(hold.wait().unwrap().code(), Some(0));

    // AC-3: 再起動後のデーモンでも退避が成立する
    e.kill_daemon();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "6")]);
    wait_until("S archived after restart", deadline(6), || container_entry(&s).is_none());
    assert_eq!(e.tier(&s), "archived");
    assert_eq!(e.read_marker(&s), ms);
}

#[test]
fn restart_during_exec() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    let n = e.create("a", &["--memory", "256"]);
    let mut child = e.spawn_exec(&n, &["sleep", "20"]);
    wait_until("lease recorded", Duration::from_secs(10), || e.view(&n).is_some_and(|v| v["leases"] == 1));
    std::thread::sleep(Duration::from_secs(1));
    e.kill_daemon();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "3"), ("ASB_IDLE_ARCHIVE_SECS", "3600")]);
    let t0 = Instant::now();
    for at in [6u64, 9] {
        if let Some(rest) = Duration::from_secs(at).checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
        assert_eq!(container_state(&n).as_deref(), Some("running"), "demoted {at}s after restart while exec was running");
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    wait_until("stopped after exec finished", deadline(3), || container_state(&n).as_deref() == Some("stopped"));
}

// ---------- AC-8 ----------

#[test]
fn cli_surface() {
    let mut e = Env::new();
    e.start_daemon(&[("ASB_IDLE_STOP_SECS", "2"), ("ASB_IDLE_ARCHIVE_SECS", "4")]);
    let n = e.create("a", &["--memory", "256"]);
    let host_in = e.home.join("in.txt");
    let host_out = e.home.join("out.txt");
    let content = format!("payload-{}\n", unique_prefix());
    std::fs::write(&host_in, &content).unwrap();
    e.ok(&["cp", host_in.to_str().unwrap(), &format!("{n}:/root/in.txt")]);
    assert_eq!(e.ok(&["exec", &n, "--", "cat", "/root/in.txt"]), content);
    e.ok(&["cp", &format!("{n}:/root/in.txt"), host_out.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&host_out).unwrap(), content);

    let v = e.view(&n).expect("ls --json lists the sandbox");
    assert!(v["tier"].is_string() && v["name"] == n.as_str());
    let table = e.ok(&["ls"]);
    assert!(table.contains(&n));

    // 退避イメージを作ってから削除する
    wait_until("archived", deadline(4), || container_entry(&n).is_none());
    e.ok(&["exec", &n, "--", "true"]);
    assert!(images().iter().any(|i| i.starts_with(&format!("asb-archive/{n}:"))));
    e.ok(&["rm", &n]);
    assert!(container_entry(&n).is_none());
    assert!(!images().iter().any(|i| i.starts_with(&format!("asb-archive/{n}:"))));
    assert!(e.view(&n).is_none());
    // 存在しない名前の rm は失敗する
    assert_ne!(e.asb(&["rm", &n]).status.code(), Some(0));
}
