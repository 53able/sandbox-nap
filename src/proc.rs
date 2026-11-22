//! プロセスの生存確認。PID の再利用を誤認しないよう、開始時刻と組で照合する。

/// プロセスの開始時刻(マイクロ秒)。プロセスが無ければ None。
pub fn start_time(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info は proc_bsdinfo の大きさを持つ有効なバッファ。
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    if n != size {
        return None;
    }
    // 終了済みで回収待ちのプロセス(ゾンビ)は生存とみなさない。
    if info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// `pid` が `start` に開始したプロセスとして今も存在するか。
pub fn is_alive(pid: i32, start: u64) -> bool {
    start_time(pid) == Some(start)
}

pub fn self_identity() -> (i32, u64) {
    let pid = std::process::id() as i32;
    (pid, start_time(pid).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_is_alive() {
        let (pid, start) = self_identity();
        assert!(start > 0);
        assert!(is_alive(pid, start));
        assert!(!is_alive(pid, start + 1), "different start time must not match");
    }

    #[test]
    fn exited_child_is_not_alive() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let start = start_time(pid);
        child.wait().unwrap();
        if let Some(s) = start {
            assert!(!is_alive(pid, s));
        }
    }

    #[test]
    fn zombie_is_not_alive() {
        let mut child = std::process::Command::new("sleep").arg("0.2").spawn().unwrap();
        let pid = child.id() as i32;
        let start = start_time(pid).expect("running child has start time");
        assert!(is_alive(pid, start));
        std::thread::sleep(std::time::Duration::from_millis(600));
        // 終了済み・未回収(ゾンビ)
        assert!(!is_alive(pid, start));
        child.wait().unwrap();
    }

    #[test]
    fn invalid_pid() {
        assert_eq!(start_time(0), None);
        assert_eq!(start_time(-5), None);
    }
}
