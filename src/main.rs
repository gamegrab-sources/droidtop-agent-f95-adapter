//! droidtop-agent-f95-adapter: the F95 plugin's context adapter for
//! droidtop-agent. It keeps F95Checker's watched threads on the computer in
//! step with the F95 plugin on the handheld, as the `f95checker` context.
//!
//! The agent runs it and speaks JSON with it over standard input and output
//! (droidtop-agent's docs/DESIGN.md section 8, adapter protocol 2):
//! - `describe` prints `{"protocol": 2, "id": "f95checker", "snapshot": true, ...}`;
//! - `snapshot` prints `{"records": {<thread id>: {<field>: <value>}}}`, F95Checker's
//!   threads now, which the agent publishes as the context's copy when they
//!   changed, before a handheld reads it; it is remembered beside the database
//!   as the base `received` merges against;
//! - `received` reads `{"generation": n, "records": {...}}`, the merged copy, and
//!   brings into F95Checker what the handheld changed since that snapshot,
//!   printing `{"deferred": null}`, or `{"deferred": "<why>"}` while F95Checker is open;
//! - `pull` and `push` stay for an agent or plugin that still syncs by
//!   changes (protocol 1);
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

/// What `received` reads: the copy made current.
#[derive(Deserialize)]
struct Received {
    #[allow(dead_code)]
    generation: u64,
    records: Records,
}

const CONTEXT: &str = "f95checker";
const PROTOCOL: u64 = 2;

fn main() {
    let verb = std::env::args().nth(1).unwrap_or_default();
    let db = std::env::var_os("F95CHECKER_DB").map(PathBuf::from).unwrap_or_else(|| f95checker_dir().join("db.sqlite3"));
    let adapter = F95Checker { db };
    let reply = match verb.as_str() {
        "describe" => Ok(json!({
            "protocol": PROTOCOL,
            "id": CONTEXT,
            "snapshot": true,
            "description": "F95Checker's watched threads, for droidtop's F95 plugin",
        })),
        "snapshot" => adapter.snapshot().map(|records| json!({ "records": records })),
        "received" => read_input::<Received>("a copy").and_then(|r| adapter.received(r.records)).map(|deferred| json!({ "deferred": deferred })),
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
        _ => Err(format!("droidtop-agent-f95-adapter: run by droidtop-agent with describe, snapshot, received, pull or push (got {verb:?}).\nAdd it with: droidtop-agent contexts add <path to this program>")),
    };
    match reply {
        Ok(v) => println!("{v}"),
        Err(e) => {
            println!("{}", json!({ "error": e }));
            std::process::exit(1);
        }
    }
}

/// Reads the one JSON object the agent sends on standard input.
fn read_input<T: serde::de::DeserializeOwned>(what: &str) -> Result<T, String> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).map_err(|e| e.to_string())?;
    serde_json::from_slice(&input).map_err(|e| format!("the agent sent {what} this adapter cannot read ({e})"))
}

/// The changes that bring into F95Checker what the handheld changed in
/// [`copy`] since [`base`] (the last snapshot), leaving alone what changed in
/// F95Checker itself since then ([`now`]): a field is written only when the
/// copy's value differs from the base's, a thread is added only when neither
/// the base nor F95Checker has it, and removed only when the base had it and
/// the copy no longer does. Only the writable fields travel this way. Pure.
fn changes_from(copy: &Records, base: &Records, now: &Records) -> Vec<RecordChange> {
    let mut out = Vec::new();
    for (id, record) in copy {
        match (base.get(id), now.contains_key(id)) {
            // Watched on the handheld: added with what the copy knows of it.
            (None, false) => out.push(RecordChange::Upsert { key: id.clone(), fields: record.clone() }),
            // Removed in F95Checker since the snapshot: that stands.
            (Some(_), false) => {}
            (was, true) => {
                let fields: Record = record
                    .iter()
                    .filter(|(f, v)| WRITABLE.contains(&f.as_str()) && was.and_then(|b| b.get(*f)) != Some(*v))
                    .map(|(f, v)| (f.clone(), v.clone()))
                    .collect();
                if !fields.is_empty() {
                    out.push(RecordChange::Upsert { key: id.clone(), fields });
                }
            }
        }
    }
    for id in now.keys() {
        if base.contains_key(id) && !copy.contains_key(id) {
            out.push(RecordChange::Remove { key: id.clone() });
        }
    }
    out
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

/// A consistent SQLite snapshot in a temporary folder, removed when dropped.
/// SQLite's backup API includes committed WAL content under its own read locks;
/// independently copying the main file and WAL can combine different moments.
struct DbCopy {
    dir: PathBuf,
    conn: rusqlite::Connection,
}

impl DbCopy {
    fn open(path: &Path) -> Result<DbCopy, String> {
        // One folder per read: two reads in one process (tests run in parallel) never share, or remove, each other's.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("droidtop-agent-f95-adapter-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let target = dir.join("db.sqlite");
        snapshot_database(path, &target)?;
        let conn = rusqlite::Connection::open(&target).map_err(|e| e.to_string())?;
        Ok(DbCopy { dir, conn })
    }
}

fn snapshot_database(source: &Path, target: &Path) -> Result<(), String> {
    let conn = rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", source.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    conn.backup(rusqlite::DatabaseName::Main, target, None).map_err(|e| format!("could not snapshot F95Checker's database: {e}"))
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
    /// Where the last snapshot is remembered: beside the database, like the backup.
    fn base_path(&self) -> PathBuf {
        self.db.with_file_name("db.sqlite3.droidtop-agent-snapshot.json")
    }

    fn base(&self) -> Records {
        fs::read(self.base_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn remember(&self, records: &Records) -> Result<(), String> {
        let path = self.base_path();
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(records).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    /// F95Checker's threads now, remembered as the base for `received`.
    fn snapshot(&self) -> Result<Records, String> {
        let records = self.pull()?;
        self.remember(&records)?;
        Ok(records)
    }

    /// Brings the merged copy into F95Checker ([`changes_from`]), only while
    /// it is closed; then the database as written is the new base.
    fn received(&self, copy: Records) -> Result<Option<String>, String> {
        if f95checker_running() {
            return Ok(Some("F95Checker is open on the computer; the change is made there once it is closed".into()));
        }
        let changes = changes_from(&copy, &self.base(), &self.pull()?);
        if let Some(deferred) = self.push(changes)? {
            return Ok(Some(deferred));
        }
        self.snapshot().map(|_| None)
    }

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
        rows.collect::<Result<Records, _>>().map_err(|e| format!("could not read the complete F95Checker snapshot: {e}"))
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
        snapshot_database(&self.db, &backup)?;
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

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dta-f95-adapter-test-{name}-{}", std::process::id()));
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
    fn backup_contains_committed_wal_rows() {
        let db = fixture("wal-backup");
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; INSERT INTO games (id, name) VALUES (200, 'In WAL');")
            .unwrap();
        let target = db.with_file_name("consistent-backup.sqlite3");
        snapshot_database(&db, &target).unwrap();
        let backup = rusqlite::Connection::open(&target).unwrap();
        assert_eq!(backup.query_row("SELECT name FROM games WHERE id=200", [], |r| r.get::<_, String>(0)).unwrap(), "In WAL");
    }

    #[test]
    fn invalid_row_does_not_replace_the_last_snapshot() {
        let db = fixture("bad-row");
        let adapter = F95Checker { db: db.clone() };
        adapter.snapshot().unwrap();
        let before = fs::read(adapter.base_path()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("ALTER TABLE games RENAME TO original_games; CREATE VIEW games AS SELECT 'invalid-id' AS id, custom, name, version, url, added_on, installed, finished, archived, rating, notes FROM original_games;").unwrap();
        assert!(adapter.snapshot().is_err());
        assert_eq!(fs::read(adapter.base_path()).unwrap(), before);
    }

    #[test]
    fn reads_threads_and_writes_only_the_writable_fields() {
        let db = fixture("rw");
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

    fn rec(pairs: &[(&str, Value)]) -> Record {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn received_takes_only_what_the_handheld_changed() {
        let base: Records = [("1".to_string(), rec(&[("name", json!("A")), ("installed", json!("0.1")), ("notes", json!(""))]))].into();
        // Since the snapshot, F95Checker set notes and watched thread 3.
        let now: Records = [
            ("1".to_string(), rec(&[("name", json!("A")), ("installed", json!("0.1")), ("notes", json!("mine"))])),
            ("3".to_string(), rec(&[("name", json!("C"))])),
        ]
        .into();
        // The handheld changed installed (and sends the old notes), and watched thread 2.
        let copy: Records = [
            ("1".to_string(), rec(&[("name", json!("A2")), ("installed", json!("0.2")), ("notes", json!(""))])),
            ("2".to_string(), rec(&[("name", json!("B")), ("url", json!("https://f95zone.to/threads/2/"))])),
        ]
        .into();
        let changes = changes_from(&copy, &base, &now);
        assert_eq!(
            changes,
            vec![
                RecordChange::Upsert { key: "1".into(), fields: rec(&[("installed", json!("0.2"))]) },
                RecordChange::Upsert {
                    key: "2".into(),
                    fields: rec(&[("name", json!("B")), ("url", json!("https://f95zone.to/threads/2/"))])
                },
            ],
            "thread 3, F95Checker's notes and the computer-side name stay"
        );
        // Unwatched on the handheld: thread 1 goes, as the base had it.
        let gone = changes_from(&Records::new(), &base, &now);
        assert_eq!(gone, vec![RecordChange::Remove { key: "1".into() }]);
    }

    #[test]
    fn a_snapshot_is_remembered_and_received_brings_the_copy_in() {
        let db = fixture("snap");
        let a = F95Checker { db: db.clone() };
        let snap = a.snapshot().unwrap();
        assert!(a.base_path().is_file());
        if f95checker_running() {
            return;
        }
        let mut copy = snap.clone();
        copy.get_mut("100").unwrap().insert("installed".into(), json!("0.2"));
        assert_eq!(a.received(copy).unwrap(), None);
        assert_eq!(a.pull().unwrap()["100"]["installed"], json!("0.2"));
        // The database as written is the new base.
        assert_eq!(a.base()["100"]["installed"], json!("0.2"));
        let _ = fs::remove_dir_all(db.parent().unwrap());
    }
}
