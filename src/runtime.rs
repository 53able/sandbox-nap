//! 実行基盤(Apple Container)の抽象。

use crate::model::{NetworkMode, ARCHIVE_REPO_PREFIX};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Stopped,
    /// 稼働中でも停止でもない(起動中・停止中など)。
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub id: String,
    pub state: ContainerState,
    /// `asb.managed=true` ラベルを持つか。
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    pub name: String,
    pub image: String,
    pub cpus: Option<u32>,
    pub memory_mb: u64,
    pub network: NetworkMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// 対象が `asb.managed=true` を持たない。操作は実行していない。
    NotManaged(String),
    NotFound(String),
    /// 退避イメージの参照名が規則に合わない。削除は実行していない。
    ImageRefRejected(String),
    /// 同名でラベルの無いネットワークがある。
    NetworkConflict(String),
    Command { args: Vec<String>, code: i32, stderr: String },
    Parse(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::NotManaged(n) => write!(f, "{n} is not managed by sandbox-nap (no asb.managed=true label)"),
            RuntimeError::NotFound(n) => write!(f, "{n} not found"),
            RuntimeError::ImageRefRejected(r) => write!(f, "refusing to touch image {r}: not a sandbox-nap archive of this sandbox"),
            RuntimeError::NetworkConflict(n) => {
                write!(f, "network {n} exists without asb.managed=true; refusing to reuse it")
            }
            RuntimeError::Command { args, code, stderr } => {
                write!(f, "`container {}` failed (exit {code}): {}", args.join(" "), stderr.trim())
            }
            RuntimeError::Parse(m) => write!(f, "cannot parse container output: {m}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

pub type RtResult<T> = Result<T, RuntimeError>;

/// Apple Container の操作。破壊的操作(stop / remove / commit / image_remove)は
/// 実装側で対象が sandbox-nap の管理物であることを確認してから実行しなければならない。
pub trait ContainerRuntime: Send + Sync {
    fn list(&self) -> RtResult<Vec<ContainerInfo>>;
    fn create(&self, spec: &CreateSpec) -> RtResult<()>;
    fn start(&self, name: &str) -> RtResult<()>;
    fn stop(&self, name: &str, timeout_secs: Option<u64>) -> RtResult<()>;
    /// 停止してから削除する。
    fn remove(&self, name: &str) -> RtResult<()>;
    fn commit(&self, name: &str, reference: &str) -> RtResult<()>;
    /// 正規化済みのイメージ名一覧(`docker.io/` を除いたもの)。
    fn image_list(&self) -> RtResult<Vec<String>>;
    /// `sandbox` の退避イメージ(リポジトリ名が `asb-archive/<sandbox>` に完全一致)のみ削除する。
    fn image_remove(&self, sandbox: &str, reference: &str) -> RtResult<()>;
    fn ensure_network(&self) -> RtResult<()>;

    fn find(&self, name: &str) -> RtResult<Option<ContainerInfo>> {
        Ok(self.list()?.into_iter().find(|c| c.id == name))
    }

    fn image_exists(&self, reference: &str) -> RtResult<bool> {
        let want = normalize_image_name(reference);
        Ok(self.image_list()?.contains(&want))
    }
}

/// `docker.io/` 接頭辞を外す。`container` は参照名に `docker.io/` を補うため。
pub fn normalize_image_name(name: &str) -> String {
    name.strip_prefix("docker.io/").unwrap_or(name).to_string()
}

/// 参照名を (リポジトリ, タグ) に分ける。タグが無ければ None。
pub fn split_ref(reference: &str) -> (&str, Option<&str>) {
    let slash = reference.rfind('/').map_or(0, |i| i + 1);
    match reference[slash..].rfind(':') {
        Some(i) => (&reference[..slash + i], Some(&reference[slash + i + 1..])),
        None => (reference, None),
    }
}

pub fn archive_repo(sandbox: &str) -> String {
    format!("{ARCHIVE_REPO_PREFIX}{sandbox}")
}

pub fn archive_ref(sandbox: &str, generation: u32) -> String {
    format!("{}:g{generation}", archive_repo(sandbox))
}

/// `reference` が `sandbox` の退避イメージか(リポジトリ名の完全一致)。
pub fn is_archive_of(sandbox: &str, reference: &str) -> bool {
    let n = normalize_image_name(reference);
    split_ref(&n).0 == archive_repo(sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_refs() {
        assert_eq!(split_ref("asb-archive/a:g1"), ("asb-archive/a", Some("g1")));
        assert_eq!(split_ref("alpine"), ("alpine", None));
        assert_eq!(split_ref("localhost:5000/x"), ("localhost:5000/x", None));
        assert_eq!(split_ref("localhost:5000/x:t"), ("localhost:5000/x", Some("t")));
    }

    #[test]
    fn archive_matching_is_exact() {
        assert!(is_archive_of("foo", "asb-archive/foo:g2"));
        assert!(is_archive_of("foo", "docker.io/asb-archive/foo:g2"));
        assert!(!is_archive_of("foo", "asb-archive/foo-bar:g1"));
        assert!(!is_archive_of("foo-bar", "asb-archive/foo:g1"));
        assert!(!is_archive_of("foo", "library/alpine:3.22"));
        assert!(!is_archive_of("foo", "evil/asb-archive/foo:g1"));
    }

    #[test]
    fn normalize() {
        assert_eq!(normalize_image_name("docker.io/asb-archive/a:g1"), "asb-archive/a:g1");
        assert_eq!(normalize_image_name("asb-archive/a:g1"), "asb-archive/a:g1");
    }
}
