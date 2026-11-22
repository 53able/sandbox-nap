//! 設定。既定値 → 設定ファイル(JSON)→ 環境変数 の順で上書きする。
//!
//! - 設定ディレクトリ: `ASB_HOME`(既定 `~/.asb`)
//! - 設定ファイル: `ASB_CONFIG`(既定 `<ASB_HOME>/config.json`)。無ければ読まない。
//! - 環境変数: `ASB_IDLE_STOP_SECS` `ASB_IDLE_ARCHIVE_SECS` `ASB_TICK_SECS`
//!   `ASB_STOP_TIMEOUT_SECS` `ASB_MAX_HOT_MEMORY_MB` `ASB_SOCKET` `ASB_DB`

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub idle_stop_secs: u64,
    pub idle_archive_secs: u64,
    pub tick_secs: u64,
    /// `container stop --time` に渡す秒数。None なら `container` の既定に任せる。
    pub stop_timeout_secs: Option<u64>,
    pub max_hot_memory_mb: u64,
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    idle_stop_secs: Option<u64>,
    idle_archive_secs: Option<u64>,
    tick_secs: Option<u64>,
    stop_timeout_secs: Option<u64>,
    max_hot_memory_mb: Option<u64>,
    socket_path: Option<PathBuf>,
    db_path: Option<PathBuf>,
}

pub const DEFAULT_IDLE_STOP_SECS: u64 = 300;
pub const DEFAULT_IDLE_ARCHIVE_SECS: u64 = 86_400;
pub const DEFAULT_TICK_SECS: u64 = 5;

impl Config {
    /// 実際の環境(環境変数・ホームディレクトリ・物理メモリ)から読み込む。
    pub fn load() -> Result<Self> {
        let env = |k: &str| std::env::var(k).ok();
        Self::resolve(&env, physical_memory_mb())
    }

    /// テスト可能な読み込み処理。`env` は環境変数の参照関数。
    pub fn resolve(env: &dyn Fn(&str) -> Option<String>, physical_mb: u64) -> Result<Self> {
        let home = match env("ASB_HOME") {
            Some(h) => PathBuf::from(h),
            None => {
                let h = env("HOME").context("HOME is not set")?;
                Path::new(&h).join(".asb")
            }
        };
        let file_path = env("ASB_CONFIG").map(PathBuf::from).unwrap_or_else(|| home.join("config.json"));
        let file: FileConfig = if file_path.exists() {
            let text = std::fs::read_to_string(&file_path)
                .with_context(|| format!("read {}", file_path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("parse {}", file_path.display()))?
        } else {
            FileConfig::default()
        };

        let num = |k: &str| -> Result<Option<u64>> {
            match env(k) {
                Some(v) => Ok(Some(v.trim().parse::<u64>().with_context(|| format!("{k} must be an integer"))?)),
                None => Ok(None),
            }
        };

        let cfg = Config {
            idle_stop_secs: num("ASB_IDLE_STOP_SECS")?.or(file.idle_stop_secs).unwrap_or(DEFAULT_IDLE_STOP_SECS),
            idle_archive_secs: num("ASB_IDLE_ARCHIVE_SECS")?
                .or(file.idle_archive_secs)
                .unwrap_or(DEFAULT_IDLE_ARCHIVE_SECS),
            tick_secs: num("ASB_TICK_SECS")?.or(file.tick_secs).unwrap_or(DEFAULT_TICK_SECS).max(1),
            stop_timeout_secs: num("ASB_STOP_TIMEOUT_SECS")?.or(file.stop_timeout_secs),
            max_hot_memory_mb: num("ASB_MAX_HOT_MEMORY_MB")?.or(file.max_hot_memory_mb).unwrap_or(physical_mb / 2),
            socket_path: env("ASB_SOCKET")
                .map(PathBuf::from)
                .or(file.socket_path)
                .unwrap_or_else(|| home.join("asb.sock")),
            db_path: env("ASB_DB").map(PathBuf::from).or(file.db_path).unwrap_or_else(|| home.join("asb.db")),
        };
        anyhow::ensure!(
            cfg.idle_archive_secs >= cfg.idle_stop_secs,
            "idle_archive_secs ({}) must be >= idle_stop_secs ({})",
            cfg.idle_archive_secs,
            cfg.idle_stop_secs
        );
        Ok(cfg)
    }
}

/// 物理メモリ量(MB)。取得できなければ 8192 とみなす。
pub fn physical_memory_mb() -> u64 {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    // SAFETY: name は NUL 終端、value と size は有効なポインタ。
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && value > 0 {
        value / (1024 * 1024)
    } else {
        8192
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn defaults() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let cfg = Config::resolve(&env_of(&[("ASB_HOME", &home)]), 32768).unwrap();
        assert_eq!(cfg.idle_stop_secs, 300);
        assert_eq!(cfg.idle_archive_secs, 86_400);
        assert_eq!(cfg.tick_secs, 5);
        assert_eq!(cfg.stop_timeout_secs, None);
        assert_eq!(cfg.max_hot_memory_mb, 16384);
        assert_eq!(cfg.socket_path, dir.path().join("asb.sock"));
        assert_eq!(cfg.db_path, dir.path().join("asb.db"));
    }

    #[test]
    fn file_then_env_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"idle_stop_secs": 10, "idle_archive_secs": 100, "max_hot_memory_mb": 2048}"#,
        )
        .unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let cfg = Config::resolve(&env_of(&[("ASB_HOME", &home), ("ASB_IDLE_STOP_SECS", "3")]), 32768).unwrap();
        assert_eq!(cfg.idle_stop_secs, 3);
        assert_eq!(cfg.idle_archive_secs, 100);
        assert_eq!(cfg.max_hot_memory_mb, 2048);
    }

    #[test]
    fn rejects_archive_shorter_than_stop() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let r = Config::resolve(
            &env_of(&[("ASB_HOME", &home), ("ASB_IDLE_STOP_SECS", "10"), ("ASB_IDLE_ARCHIVE_SECS", "5")]),
            8192,
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_non_integer_env() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        assert!(Config::resolve(&env_of(&[("ASB_HOME", &home), ("ASB_TICK_SECS", "x")]), 8192).is_err());
    }

    #[test]
    fn physical_memory_is_positive() {
        assert!(physical_memory_mb() > 0);
    }
}
