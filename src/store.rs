//! SQLite の保存層。段階は保存しない(実状態から導出する)。

use crate::model::{Lease, NetworkMode, Sandbox};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sandboxes (
    name         TEXT PRIMARY KEY,
    image        TEXT NOT NULL,
    cpus         INTEGER,
    memory_mb    INTEGER NOT NULL,
    network_mode TEXT NOT NULL,
    last_access  INTEGER NOT NULL,
    archive_ref  TEXT,
    archive_gen  INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS leases (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    sandbox     TEXT NOT NULL,
    cli_pid     INTEGER NOT NULL,
    cli_start   INTEGER NOT NULL,
    exec_pid    INTEGER,
    exec_start  INTEGER,
    acquired_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS leases_sandbox ON leases(sandbox);
"#;

fn row_to_sandbox(r: &rusqlite::Row<'_>) -> rusqlite::Result<Sandbox> {
    let net: String = r.get(4)?;
    Ok(Sandbox {
        name: r.get(0)?,
        image: r.get(1)?,
        cpus: r.get::<_, Option<i64>>(2)?.map(|v| v as u32),
        memory_mb: r.get::<_, i64>(3)? as u64,
        network: NetworkMode::parse(&net).unwrap_or(NetworkMode::Internal),
        last_access_ms: r.get(5)?,
        archive_ref: r.get(6)?,
        archive_gen: r.get::<_, i64>(7)? as u32,
        created_at_ms: r.get(8)?,
    })
}

fn row_to_lease(r: &rusqlite::Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        id: r.get(0)?,
        sandbox: r.get(1)?,
        cli_pid: r.get(2)?,
        cli_start: r.get::<_, i64>(3)? as u64,
        exec_pid: r.get(4)?,
        exec_start: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        acquired_at_ms: r.get(6)?,
    })
}

const SANDBOX_COLS: &str = "name, image, cpus, memory_mb, network_mode, last_access, archive_ref, archive_gen, created_at";
const LEASE_COLS: &str = "id, sandbox, cli_pid, cli_start, exec_pid, exec_start, acquired_at";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn insert_sandbox(&self, s: &Sandbox) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO sandboxes ({SANDBOX_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
            params![
                s.name,
                s.image,
                s.cpus.map(i64::from),
                s.memory_mb as i64,
                s.network.as_str(),
                s.last_access_ms,
                s.archive_ref,
                i64::from(s.archive_gen),
                s.created_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn get_sandbox(&self, name: &str) -> Result<Option<Sandbox>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {SANDBOX_COLS} FROM sandboxes WHERE name = ?1"), [name], row_to_sandbox)
            .optional()?)
    }

    pub fn list_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let mut st = self.conn.prepare(&format!("SELECT {SANDBOX_COLS} FROM sandboxes ORDER BY name"))?;
        let rows = st.query_map([], row_to_sandbox)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_sandbox(&self, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM sandboxes WHERE name = ?1", [name])?;
        self.conn.execute("DELETE FROM leases WHERE sandbox = ?1", [name])?;
        Ok(())
    }

    pub fn touch(&self, name: &str, now_ms: i64) -> Result<()> {
        self.conn.execute("UPDATE sandboxes SET last_access = ?2 WHERE name = ?1", params![name, now_ms])?;
        Ok(())
    }

    pub fn set_archive(&self, name: &str, archive_ref: &str, generation: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET archive_ref = ?2, archive_gen = ?3 WHERE name = ?1",
            params![name, archive_ref, i64::from(generation)],
        )?;
        Ok(())
    }

    pub fn insert_lease(&self, sandbox: &str, cli_pid: i32, cli_start: u64, now_ms: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO leases (sandbox, cli_pid, cli_start, acquired_at) VALUES (?1, ?2, ?3, ?4)",
            params![sandbox, cli_pid, cli_start as i64, now_ms],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// exec 子プロセスを記録する。リースが無ければ false。
    pub fn attach_lease(&self, id: i64, exec_pid: i32, exec_start: u64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE leases SET exec_pid = ?2, exec_start = ?3 WHERE id = ?1",
            params![id, exec_pid, exec_start as i64],
        )?;
        Ok(n == 1)
    }

    pub fn get_lease(&self, id: i64) -> Result<Option<Lease>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {LEASE_COLS} FROM leases WHERE id = ?1"), [id], row_to_lease)
            .optional()?)
    }

    pub fn delete_lease(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM leases WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn list_leases(&self) -> Result<Vec<Lease>> {
        let mut st = self.conn.prepare(&format!("SELECT {LEASE_COLS} FROM leases ORDER BY id"))?;
        let rows = st.query_map([], row_to_lease)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn lease_count(&self, sandbox: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM leases WHERE sandbox = ?1", [sandbox], |r| r.get(0))?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb(name: &str) -> Sandbox {
        Sandbox {
            name: name.into(),
            image: "alpine:3.22".into(),
            cpus: Some(2),
            memory_mb: 512,
            network: NetworkMode::Internet,
            last_access_ms: 10,
            archive_ref: None,
            archive_gen: 0,
            created_at_ms: 10,
        }
    }

    #[test]
    fn sandbox_roundtrip_and_updates() {
        let s = Store::in_memory().unwrap();
        s.insert_sandbox(&sb("a")).unwrap();
        assert!(s.insert_sandbox(&sb("a")).is_err(), "duplicate name must fail");
        assert_eq!(s.get_sandbox("a").unwrap().unwrap(), sb("a"));
        s.touch("a", 99).unwrap();
        s.set_archive("a", "asb-archive/a:g1", 1).unwrap();
        let got = s.get_sandbox("a").unwrap().unwrap();
        assert_eq!(got.last_access_ms, 99);
        assert_eq!(got.archive_ref.as_deref(), Some("asb-archive/a:g1"));
        assert_eq!(got.archive_gen, 1);
        assert_eq!(s.list_sandboxes().unwrap().len(), 1);
    }

    #[test]
    fn leases_lifecycle() {
        let s = Store::in_memory().unwrap();
        s.insert_sandbox(&sb("a")).unwrap();
        let id = s.insert_lease("a", 100, 5, 1).unwrap();
        assert_eq!(s.lease_count("a").unwrap(), 1);
        assert!(s.attach_lease(id, 200, 6).unwrap());
        assert!(!s.attach_lease(id + 100, 1, 1).unwrap());
        let l = s.get_lease(id).unwrap().unwrap();
        assert_eq!((l.exec_pid, l.exec_start), (Some(200), Some(6)));
        s.delete_lease(id).unwrap();
        assert_eq!(s.lease_count("a").unwrap(), 0);
    }

    #[test]
    fn delete_sandbox_removes_leases() {
        let s = Store::in_memory().unwrap();
        s.insert_sandbox(&sb("a")).unwrap();
        s.insert_lease("a", 1, 1, 1).unwrap();
        s.delete_sandbox("a").unwrap();
        assert!(s.get_sandbox("a").unwrap().is_none());
        assert!(s.list_leases().unwrap().is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.db");
        {
            let s = Store::open(&p).unwrap();
            s.insert_sandbox(&sb("a")).unwrap();
            s.insert_lease("a", 1, 2, 3).unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert!(s.get_sandbox("a").unwrap().is_some());
        assert_eq!(s.list_leases().unwrap().len(), 1);
    }
}
