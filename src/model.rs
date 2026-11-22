//! ドメインモデル。

use serde::{Deserialize, Serialize};

/// 管理対象を示すラベル。
pub const MANAGED_LABEL: &str = "asb.managed";
pub const MANAGED_VALUE: &str = "true";
/// internal ネットワーク名。
pub const INTERNAL_NETWORK: &str = "asb-internal";
/// 退避イメージのリポジトリ接頭辞。
pub const ARCHIVE_REPO_PREFIX: &str = "asb-archive/";

/// サンドボックスの段階。保存はせず、実状態と DB 行から導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hot,
    Stopped,
    Archived,
    /// DB 行はあるが、コンテナも退避イメージも無い。
    Lost,
    /// `asb.managed=true` のコンテナはあるが DB 行が無い。
    Orphan,
    /// コンテナが稼働中でも停止でもない過渡状態。
    Transient,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Hot => "hot",
            Tier::Stopped => "stopped",
            Tier::Archived => "archived",
            Tier::Lost => "lost",
            Tier::Orphan => "orphan",
            Tier::Transient => "transient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// 外向き通信なし(`asb-internal` ネットワーク)。既定。
    Internal,
    /// `container` の既定ネットワーク(インターネット可)。
    Internet,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::Internal => "internal",
            NetworkMode::Internet => "internet",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "internal" => Some(NetworkMode::Internal),
            "internet" => Some(NetworkMode::Internet),
            _ => None,
        }
    }
}

/// DB に保存するサンドボックスのメタデータ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    pub name: String,
    pub image: String,
    pub cpus: Option<u32>,
    pub memory_mb: u64,
    pub network: NetworkMode,
    /// 最終アクセス時刻(UNIX ミリ秒)。
    pub last_access_ms: i64,
    pub archive_ref: Option<String>,
    pub archive_gen: u32,
    pub created_at_ms: i64,
}

/// 実行中の exec を降格から守るリース。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: i64,
    pub sandbox: String,
    pub cli_pid: i32,
    pub cli_start: u64,
    pub exec_pid: Option<i32>,
    pub exec_start: Option<u64>,
    pub acquired_at_ms: i64,
}

/// サンドボックス名の検証。コンテナ名と退避イメージ名の両方に使える文字に限る。
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok_len = !name.is_empty() && name.len() <= 63;
    let ok_chars = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let ok_edges = name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok_len && ok_chars && ok_edges {
        Ok(())
    } else {
        Err(format!(
            "invalid sandbox name {name:?}: use 1-63 chars of [a-z0-9-], starting and ending with [a-z0-9]"
        ))
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("box-1").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("Box").is_err());
        assert!(validate_name("-box").is_err());
        assert!(validate_name("box-").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"a".repeat(64)).is_err());
    }

    #[test]
    fn network_mode_roundtrip() {
        for m in [NetworkMode::Internal, NetworkMode::Internet] {
            assert_eq!(NetworkMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(NetworkMode::parse("x"), None);
    }
}
