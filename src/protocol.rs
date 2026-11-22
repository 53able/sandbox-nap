//! CLI とデーモンの間の通信(Unix ソケット上の改行区切り JSON)。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Create { name: String, image: String, cpus: Option<u32>, memory_mb: Option<u64>, internet: bool },
    Acquire { name: String, cli_pid: i32, cli_start: u64 },
    Attach { lease_id: i64, exec_pid: i32, exec_start: u64 },
    Release { lease_id: i64 },
    List,
    Remove { name: String },
}

/// エラーの種類。CLI の終了コードと対応する。
pub mod code {
    pub const REJECT: &str = "reject";
    pub const NOT_MANAGED: &str = "not_managed";
    pub const NOT_FOUND: &str = "not_found";
    pub const EXISTS: &str = "exists";
    pub const LOST: &str = "lost";
    pub const ORPHAN: &str = "orphan";
    pub const BUSY: &str = "busy";
    pub const INVALID: &str = "invalid";
    pub const TRANSIENT: &str = "transient";
    pub const RUNTIME: &str = "runtime";
    pub const INTERNAL: &str = "internal";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxView {
    pub name: String,
    pub tier: String,
    pub last_access_ms: Option<i64>,
    pub leases: usize,
    pub archive_ref: Option<String>,
    pub image: Option<String>,
    pub memory_mb: Option<u64>,
    pub cpus: Option<u32>,
    pub network: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandboxes: Option<Vec<SandboxView>>,
}

impl Response {
    pub fn ok() -> Self {
        Response { ok: true, ..Default::default() }
    }

    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Response { ok: false, code: Some(code.to_string()), message: Some(message.into()), ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape() {
        let r = Request::Acquire { name: "a".into(), cli_pid: 1, cli_start: 2 };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"op":"acquire","name":"a","cli_pid":1,"cli_start":2}"#);
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        assert_eq!(serde_json::from_str::<Request>(r#"{"op":"list"}"#).unwrap(), Request::List);
    }

    #[test]
    fn response_omits_empty_fields() {
        assert_eq!(serde_json::to_string(&Response::ok()).unwrap(), r#"{"ok":true}"#);
        let e = Response::error(code::REJECT, "no room");
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"ok":false,"code":"reject","message":"no room"}"#);
    }
}
