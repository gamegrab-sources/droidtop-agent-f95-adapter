//! droidtop-agent-f95-adapter: the F95 plugin's context adapter for
//! droidtop-agent. It keeps F95Checker's watched threads on the computer in
//! step with the F95 plugin on the handheld, as the `f95checker` context.
//!
//! The agent runs it once per pull or push and speaks JSON with it over
//! standard input and output (droidtop-agent's docs/DESIGN.md section 8,
//! adapter protocol 1):
//! - `describe` prints `{"protocol": 1, "id": "f95checker", "description": ...}`;
//! - `pull` prints `{"records": {<thread id>: {<field>: <value>}}}`;
//! - `push` reads `{"changes": [...]}` and prints `{"deferred": null}`, or
//!   `{"deferred": "<why>"}` while F95Checker is open;
//! - a failure prints `{"error": "..."}` and exits with 1.
//!
//! It reads only the `games` columns the context declares, never the
//! `cookies` table or the settings' passwords and tokens, and writes only the
//! fields the handheld may change, and only while F95Checker is closed:
//! F95Checker keeps its database in memory and writes it back, so a change
//! made under it would be lost.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde_json::{json, Value};

type Record = BTreeMap<String, Value>;
type Records = BTreeMap<String, Record>;

/// A change to one record, as droidtop-agent's core states it.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
enum RecordChange {
    Upsert { key: String, fields: Record },
    Remove { key: String },
}

#[derive(Deserialize)]
struct Push {
    changes: Vec<RecordChange>,
}

const CONTEXT: &str = "f95checker";
const PROTOCOL: u64 = 1;

fn main() {
    let verb = std::env::args().nth(1).unwrap_or_default();
    let db = std::env::var_os("F95CHECKER_DB").map(PathBuf::from).unwrap_or_else(|| f95checker_dir().join("db.sqlite3"));
    let adapter = F95Checker { db };
    let reply = match verb.as_str() {
        "describe" => Ok(json!({
            "protocol": PROTOCOL,
            "id": CONTEXT,
            "description": "F95Checker's watched threads, for droidtop's F95 plugin",
        })),
        "pull" => adapter.pull().map(|records| json!({ "records": records })),
        "push" => {
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .map_err(|e| e.to_string())
                .and_then(|_| serde_json::from_slice::<Push>(&input).map_err(|e| format!("the agent sent changes this adapter cannot read ({e})")))
                .and_then(|p| adapter.push(p.changes))
                .map(|deferred| json!({ "deferred": deferred }))
        }
        _ => Err(format!("droidtop-agent-f95-adapter: run by droidtop-agent with describe, pull or push (got {verb:?}).\nAdd it with: droidtop-agent contexts add <path to this program>")),
    };
    match reply {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({ "error": e }));
            std::process::exit(1);
        }
    }
}

/// F95Checker's data folder, the same place F95Checker itself uses (its
/// `modules/globals.py`).
fn f95checker_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming")).join("f95checker")
    } else if cfg!(target_os = "macos") {
        home.join("Library/Application Support/f95checker")
    } else {
        home.join(".config/f95checker")
    }
}

struct F95Checker {
    db: PathBuf,
}

/// The `games` columns the context carries, and what kind each is.
const FIELDS: &[(&str, Kind)] = &[
    ("name", Kind::Text),
    ("url", Kind::Text),
    ("version", Kind::Text),
    ("developer", Kind::Text),
    ("status", Kind::Int),
    ("type", Kind::Int),
    ("last_updated", Kind::Int),
    ("installed", Kind::Text),
    ("finished", Kind::Text),
    ("archived", Kind::Bool),
    ("rating", Kind::Int),
    ("notes", Kind::Text),
];

/// The fields the handheld may change on the computer.
const WRITABLE: &[&str] = &["installed", "finished", "archived", "rating", "notes"];

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Int,
    Bool,
}

fn to_json(kind: Kind, v: SqlValue) -> Value {
    match (kind, v) {
        (_, SqlValue::Null) => Value::Null,
        (Kind::Bool, SqlValue::Integer(i)) => Value::Bool(i != 0),
        (_, SqlValue::Integer(i)) => Value::from(i),
        (_, SqlValue::Real(f)) => Value::from(f),
        (_, SqlValue::Text(t)) => Value::String(t),
        (_, SqlValue::Blob(_)) => Value::Null,
    }
}

fn to_sql(kind: Kind, v: &Value) -> SqlValue {
    match (kind, v) {
        (_, Value::Null) => SqlValue::Null,
        (Kind::Bool, Value::Bool(b)) => SqlValue::Integer(i64::from(*b)),
        (Kind::Int, Value::Number(n)) => n.as_i64().map(SqlValue::Integer).unwrap_or(SqlValue::Null),
        (Kind::Text, Value::String(s)) => SqlValue::Text(s.clone()),
        (Kind::Text, other) => SqlValue::Text(other.to_string()),
        (_, Value::Bool(b)) => SqlValue::Integer(i64::from(*b)),
        (_, Value::Number(n)) => n.as_i64().map(SqlValue::Integer).unwrap_or(SqlValue::Null),
        _ => SqlValue::Null,
    }
}

/// The columns a table has, so the queries follow F95Checker's schema as it changes.
fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else { return Vec::new() };
    stmt.query_map([], |row| row.get::<_, String>(1)).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

/// A copy of the database (with its write-ahead log) in a temporary folder,
/// removed when dropped: reading a copy never takes a lock F95Checker could
/// trip over, and sees changes still in its log.
struct DbCopy {
    dir: PathBuf,
    conn: rusqlite::Connection,
}

impl DbCopy {
    fn open(path: &Path) -> Result<DbCopy, String> {
        let dir = std::env::temp_dir().join(format!("droidtop-agent-f95-adapter-{}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let target = dir.join("db.sqlite");
        fs::copy(path, &target).map_err(|e| format!("{}: {e}", path.display()))?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if side.is_file() {
                let _ = fs::copy(&side, PathBuf::from(format!("{}{suffix}", target.display())));
            }
        }
        let conn = rusqlite::Connection::open(&target).map_err(|e| e.to_string())?;
        Ok(DbCopy { dir, conn })
    }
}

impl Drop for DbCopy {
    fn drop(&mut self) {
        // Only ever the folder made above, under the system's temp folder.
        if self.dir.starts_with(std::env::temp_dir())
            && self.dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with("droidtop-agent-f95-adapter-"))
        {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Whether F95Checker is running on this computer.
fn f95checker_running() -> bool {
    let me = std::process::id().to_string();
    let not_us = |l: &str| l.contains("f95checker") && !l.contains("droidtop-agent");
    if cfg!(windows) {
        return Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().lines().any(not_us))
            .unwrap_or(false);
    }
    if cfg!(target_os = "linux") {
        let Ok(read) = fs::read_dir("/proc") else { return false };
        return read.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.chars().all(|c| c.is_ascii_digit())
                && name != me
                && fs::read(e.path().join("cmdline")).map(|c| String::from_utf8_lossy(&c).to_lowercase()).is_ok_and(|c| not_us(&c))
        });
    }
    Command::new("ps")
        .args(["-axo", "command"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().lines().any(not_us))
        .unwrap_or(false)
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl F95Checker {
    fn pull(&self) -> Result<Records, String> {
        if !self.db.is_file() {
            return Err("F95Checker's database is not on this computer".into());
        }
        let copy = DbCopy::open(&self.db)?;
        let present = columns(&copy.conn, "games");
        let fields: Vec<(&str, Kind)> = FIELDS.iter().copied().filter(|(f, _)| present.iter().any(|p| p == f)).collect();
        let custom = if present.iter().any(|p| p == "custom") { "AND (custom IS NULL OR custom = 0)" } else { "" };
        let sql = format!(
            "SELECT id{}{} FROM games WHERE id > 0 {custom}",
            if fields.is_empty() { "" } else { ", " },
            fields.iter().map(|(f, _)| format!("\"{f}\"")).collect::<Vec<_>>().join(", ")
        );
        let mut stmt = copy.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let mut record = Record::new();
                for (i, (name, kind)) in fields.iter().enumerate() {
                    let v: SqlValue = row.get(i + 1)?;
                    record.insert(name.to_string(), to_json(*kind, v));
                }
                Ok((id.to_string(), record))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    fn push(&self, changes: Vec<RecordChange>) -> Result<Option<String>, String> {
        if changes.is_empty() {
            return Ok(None);
        }
        if f95checker_running() {
            return Ok(Some("F95Checker is open on the computer; the change is made there once it is closed".into()));
        }
        if !self.db.is_file() {
            return Err("F95Checker's database is not on this computer".into());
        }
        // One copy of the database as it was before this adapter last wrote it.
        let backup = self.db.with_file_name("db.sqlite3.droidtop-agent-backup");
        fs::copy(&self.db, &backup).map_err(|e| format!("could not back up F95Checker's database: {e}"))?;
        let mut conn = rusqlite::Connection::open(&self.db).map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
        let present = columns(&conn, "games");
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for change in changes {
            match change {
                RecordChange::Upsert { key, fields } => {
                    let Ok(id) = key.parse::<i64>() else { continue };
                    if id <= 0 {
                        continue;
                    }
                    let exists: bool =
                        tx.query_row("SELECT EXISTS(SELECT 1 FROM games WHERE id = ?1)", [id], |r| r.get(0)).map_err(|e| e.to_string())?;
                    if !exists {
                        let name =
                            fields.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("Unknown ({id})"));
                        let url = fields
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("https://f95zone.to/threads/{id}/"));
                        // The columns F95Checker's own create_game sets for a thread.
                        tx.execute(
                            "INSERT INTO games (id, custom, name, url, added_on) VALUES (?1, 0, ?2, ?3, ?4)",
                            rusqlite::params![id, name, url, now_s()],
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    for (name, value) in &fields {
                        if !WRITABLE.contains(&name.as_str()) || !present.iter().any(|p| p == name) {
                            continue;
                        }
                        let Some((_, kind)) = FIELDS.iter().find(|(f, _)| f == name) else { continue };
                        tx.execute(&format!("UPDATE games SET \"{name}\" = ?1 WHERE id = ?2"), rusqlite::params![to_sql(*kind, value), id])
                            .map_err(|e| e.to_string())?;
                    }
                }
                RecordChange::Remove { key } => {
                    let Ok(id) = key.parse::<i64>() else { continue };
                    tx.execute("DELETE FROM games WHERE id = ?1 AND id > 0", [id]).map_err(|e| e.to_string())?;
                }
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dta-f95-adapter-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db.sqlite3");
        let conn = rusqlite::Connection::open(&db).unwrap();
        // The shape of F95Checker's games table (modules/db.py), trimmed to what is used here.
        conn.execute_batch(
            r#"CREATE TABLE games (id INTEGER PRIMARY KEY, custom INTEGER DEFAULT NULL, name TEXT DEFAULT "", version TEXT DEFAULT "Unchecked",
               url TEXT DEFAULT "", added_on INTEGER DEFAULT 0, installed TEXT DEFAULT "", finished TEXT DEFAULT "",
               archived INTEGER DEFAULT 0, rating INTEGER DEFAULT 0, notes TEXT DEFAULT "");
               CREATE TABLE cookies (key TEXT PRIMARY KEY, value TEXT DEFAULT "");
               INSERT INTO games (id, name, version, installed) VALUES (100, 'A Game', '0.2', '0.1');
               INSERT INTO games (id, custom, name) VALUES (-5, 1, 'Custom');
               INSERT INTO cookies VALUES ('xf_session', 'secret');"#,
        )
        .unwrap();
        db
    }

    #[test]
    fn reads_threads_and_writes_only_the_writable_fields() {
        let db = fixture();
        let a = F95Checker { db: db.clone() };
        let records = a.pull().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records["100"]["installed"], json!("0.1"));
        assert_eq!(records["100"]["archived"], json!(false));
        assert!(!serde_json::to_string(&records).unwrap().contains("secret"));
        let changes: Push = serde_json::from_value(json!({ "changes": [
            { "op": "upsert", "key": "100", "fields": { "installed": "0.2", "version": "9.9" } },
            { "op": "upsert", "key": "200", "fields": { "name": "New", "archived": true } },
        ]}))
        .unwrap();
        if f95checker_running() {
            return;
        }
        assert_eq!(a.push(changes.changes).unwrap(), None);
        let after = a.pull().unwrap();
        assert_eq!(after["100"]["installed"], json!("0.2"));
        assert_eq!(after["100"]["version"], json!("0.2"));
        assert_eq!(after["200"]["name"], json!("New"));
        assert_eq!(after["200"]["archived"], json!(true));
        assert!(db.with_file_name("db.sqlite3.droidtop-agent-backup").is_file());
        a.push(vec![RecordChange::Remove { key: "200".into() }]).unwrap();
        assert!(!a.pull().unwrap().contains_key("200"));
        let _ = fs::remove_dir_all(db.parent().unwrap());
    }
}
