//! CLI 側の処理。デーモンへ依頼し、exec / cp は `container` を直接起動する(リース方式)。

use crate::config::Config;
use crate::model::validate_name;
use crate::proc;
use crate::protocol::{code, Request, Response, SandboxView};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const EXIT_FAILURE: i32 = 1;
/// デーモンに接続できない(sysexits EX_UNAVAILABLE)。
pub const EXIT_NO_DAEMON: i32 = 69;
/// 受付判定で拒否された(sysexits EX_TEMPFAIL)。
pub const EXIT_REJECTED: i32 = 75;

const RECONNECT_TRIES: u32 = 60;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// CLI のエラー。終了コードと表示文を持つ。
#[derive(Debug)]
pub struct CliError {
    pub exit: i32,
    pub message: String,
}

type CliResult<T> = Result<T, CliError>;

fn from_response(r: &Response) -> CliError {
    let exit = match r.code.as_deref() {
        Some(code::REJECT) => EXIT_REJECTED,
        _ => EXIT_FAILURE,
    };
    let c = r.code.as_deref().unwrap_or("error");
    let msg = r.message.as_deref().unwrap_or("request failed");
    let text = if c == code::NOT_MANAGED { format!("asb: not managed: {msg}") } else { format!("asb: {c}: {msg}") };
    CliError { exit, message: text }
}

struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Conn {
    fn open(cfg: &Config) -> std::io::Result<Self> {
        let s = UnixStream::connect(&cfg.socket_path)?;
        Ok(Conn { writer: s.try_clone()?, reader: BufReader::new(s) })
    }

    fn call(&mut self, req: &Request) -> std::io::Result<Response> {
        let mut line = serde_json::to_string(req).map_err(std::io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon closed the connection"));
        }
        serde_json::from_str(&resp).map_err(std::io::Error::other)
    }
}

fn no_daemon(cfg: &Config, e: &std::io::Error) -> CliError {
    CliError {
        exit: EXIT_NO_DAEMON,
        message: format!(
            "asb: cannot connect to the daemon at {} ({e}). Start it with `asb daemon`.",
            cfg.socket_path.display()
        ),
    }
}

fn connect(cfg: &Config) -> CliResult<Conn> {
    Conn::open(cfg).map_err(|e| no_daemon(cfg, &e))
}

fn call_once(cfg: &Config, req: &Request) -> CliResult<Response> {
    let mut c = connect(cfg)?;
    let r = c.call(req).map_err(|e| no_daemon(cfg, &e))?;
    if r.ok {
        Ok(r)
    } else {
        Err(from_response(&r))
    }
}

/// 再接続しながら 1 回の依頼を届ける(最大 60 回、1 秒間隔)。
fn call_with_retry(cfg: &Config, req: &Request) -> Option<Response> {
    for i in 0..RECONNECT_TRIES {
        if i > 0 {
            std::thread::sleep(RECONNECT_INTERVAL);
        }
        if let Ok(mut c) = Conn::open(cfg) {
            if let Ok(r) = c.call(req) {
                return Some(r);
            }
        }
    }
    None
}

pub fn create(cfg: &Config, name: &str, image: &str, cpus: Option<u32>, memory_mb: Option<u64>, internet: bool) -> CliResult<()> {
    call_once(cfg, &Request::Create { name: name.into(), image: image.into(), cpus, memory_mb, internet }).map(|_| ())
}

pub fn remove(cfg: &Config, name: &str) -> CliResult<()> {
    call_once(cfg, &Request::Remove { name: name.into() }).map(|_| ())
}

pub fn list(cfg: &Config) -> CliResult<Vec<SandboxView>> {
    Ok(call_once(cfg, &Request::List)?.sandboxes.unwrap_or_default())
}

pub fn print_list(views: &[SandboxView], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(views).unwrap_or_else(|_| "[]".into()));
        return;
    }
    println!("{:<24} {:<10} {:>6} {:>8} {:<20} ARCHIVE", "NAME", "TIER", "LEASES", "MEM_MB", "LAST_ACCESS");
    for v in views {
        let last = v.last_access_ms.map(format_ts).unwrap_or_else(|| "-".into());
        println!(
            "{:<24} {:<10} {:>6} {:>8} {:<20} {}",
            v.name,
            v.tier,
            v.leases,
            v.memory_mb.map(|m| m.to_string()).unwrap_or_else(|| "-".into()),
            last,
            v.archive_ref.as_deref().unwrap_or("-")
        );
    }
}

fn format_ts(ms: i64) -> String {
    let secs = (crate::model::now_ms() - ms).max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// リースを取得し、`container` の子プロセスを動かし、解放する。
/// 子プロセスの終了状態を返す。
fn run_leased(cfg: &Config, name: &str, mut command: Command) -> CliResult<ExitStatus> {
    let (cli_pid, cli_start) = proc::self_identity();
    let mut conn = connect(cfg)?;
    let resp = conn
        .call(&Request::Acquire { name: name.into(), cli_pid, cli_start })
        .map_err(|e| no_daemon(cfg, &e))?;
    if !resp.ok {
        return Err(from_response(&resp));
    }
    let lease_id = resp.lease_id.ok_or_else(|| CliError { exit: EXIT_FAILURE, message: "asb: daemon returned no lease".into() })?;

    let mut child = command.spawn().map_err(|e| CliError { exit: EXIT_FAILURE, message: format!("asb: cannot run container: {e}") })?;
    // 子の実行中は CLI 自身が SIGINT / SIGQUIT で落ちないようにする(子には届く)。
    // SAFETY: シグナル処理を無視に設定するだけ。
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let exec_pid = child.id() as i32;
    let exec_start = proc::start_time(exec_pid).unwrap_or(0);
    let attach = Request::Attach { lease_id, exec_pid, exec_start };
    if !matches!(conn.call(&attach), Ok(r) if r.ok) {
        // デーモン再起動などで届かなかった場合は再接続して送り直す。
        let _ = call_with_retry(cfg, &attach);
    }

    // 接続を見張り、切れたら再接続して Attach を送り直す。
    let done = Arc::new(AtomicBool::new(false));
    {
        let cfg = cfg.clone();
        let done = Arc::clone(&done);
        let attach = attach.clone();
        let mut current = conn;
        std::thread::spawn(move || loop {
            let mut buf = String::new();
            // デーモンは要求が無い限り何も送らないので、read が返るのは切断時のみ。
            let _ = current.reader.read_line(&mut buf);
            if done.load(Ordering::SeqCst) {
                return;
            }
            let mut reconnected = None;
            for _ in 0..RECONNECT_TRIES {
                std::thread::sleep(RECONNECT_INTERVAL);
                if done.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(mut c) = Conn::open(&cfg) {
                    if matches!(c.call(&attach), Ok(r) if r.ok) {
                        reconnected = Some(c);
                        break;
                    }
                }
            }
            match reconnected {
                Some(c) => current = c,
                None => return,
            }
        });
    }

    let status = child.wait().map_err(|e| CliError { exit: EXIT_FAILURE, message: format!("asb: wait failed: {e}") })?;
    done.store(true, Ordering::SeqCst);
    if call_with_retry(cfg, &Request::Release { lease_id }).is_none() {
        eprintln!("asb: warning: could not release lease {lease_id}; the daemon will release it once this process exits");
    }
    Ok(status)
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

pub struct ExecOptions {
    pub interactive: bool,
    pub tty: bool,
    pub workdir: Option<String>,
    pub env: Vec<String>,
}

pub fn exec(cfg: &Config, name: &str, opts: &ExecOptions, cmd: &[String]) -> CliResult<i32> {
    if cmd.is_empty() {
        return Err(CliError { exit: EXIT_FAILURE, message: "asb: exec needs a command after --".into() });
    }
    let mut c = Command::new("container");
    c.arg("exec");
    if opts.interactive {
        c.arg("-i");
    }
    if opts.tty {
        c.arg("-t");
    }
    if let Some(w) = &opts.workdir {
        c.args(["-w", w]);
    }
    for e in &opts.env {
        c.args(["-e", e]);
    }
    c.arg(name).args(cmd);
    run_leased(cfg, name, c).map(exit_code)
}

/// `name:path` 形式なら sandbox 名を返す。
pub fn sandbox_of(arg: &str) -> Option<&str> {
    let (head, _) = arg.split_once(':')?;
    validate_name(head).ok().map(|_| head)
}

pub fn cp(cfg: &Config, src: &str, dst: &str) -> CliResult<i32> {
    let name = match (sandbox_of(src), sandbox_of(dst)) {
        (Some(n), None) | (None, Some(n)) => n.to_string(),
        (Some(_), Some(_)) => {
            return Err(CliError { exit: EXIT_FAILURE, message: "asb: cp between two sandboxes is not supported".into() })
        }
        (None, None) => {
            return Err(CliError { exit: EXIT_FAILURE, message: "asb: one side of cp must be <sandbox>:<path>".into() })
        }
    };
    let mut c = Command::new("container");
    c.args(["cp", src, dst]);
    run_leased(cfg, &name, c).map(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_side_of_cp() {
        assert_eq!(sandbox_of("box:/root/f"), Some("box"));
        assert_eq!(sandbox_of("./local"), None);
        assert_eq!(sandbox_of("/abs/path"), None);
        assert_eq!(sandbox_of("Box:/x"), None);
        assert_eq!(sandbox_of("dir/x:y"), None);
    }

    #[test]
    fn reject_maps_to_exit_75() {
        let e = from_response(&Response::error(code::REJECT, "x"));
        assert_eq!(e.exit, EXIT_REJECTED);
        let e = from_response(&Response::error(code::NOT_MANAGED, "x"));
        assert_eq!(e.exit, EXIT_FAILURE);
        assert!(e.message.contains("not managed"));
    }
}
