// src/history.rs
//
// Command history with output: every finished command (from shell
// integration's blocks) is saved to SQLite with full-text search over the
// command line and its output -- "what did that curl return on Tuesday?".
//
// Lives at `$XDG_DATA_HOME/cyberterm/history.sqlite3` (default
// `~/.local/share/...`), shared by every window and the daemon (SQLite in
// WAL mode handles several writers). Before anything is written, common
// secrets are redacted, and commands typed with a leading space are
// skipped, the way shells' HISTCONTROL=ignorespace works.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

use alacritty_terminal::Term;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};

use crate::shell::ShellState;

/// One finished command, ready to store.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub command: String,
    pub output: String,
    pub cwd: Option<PathBuf>,
    pub exit: Option<i32>,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub host: String,
    pub session: String,
}

/// One stored command, as returned by searches.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub command: String,
    pub cwd: Option<String>,
    pub exit: Option<i32>,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub host: String,
    pub session: String,
    /// A match excerpt from the output, `[`/`]` around hits.
    pub snippet: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Words to find in the command or its output (all must match).
    pub text: Option<String>,
    pub failed_only: bool,
    pub cwd: Option<String>,
    /// Exactly this command line.
    pub command: Option<String>,
    /// Only entries that started before this time.
    pub before_ms: Option<u64>,
    pub limit: usize,
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cyberterm").join("history.sqlite3")
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS commands (
                 id INTEGER PRIMARY KEY,
                 command TEXT NOT NULL,
                 output TEXT NOT NULL,
                 cwd TEXT,
                 exit INTEGER,
                 started_ms INTEGER NOT NULL,
                 finished_ms INTEGER NOT NULL,
                 host TEXT NOT NULL,
                 session TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS commands_started ON commands(started_ms);
             CREATE VIRTUAL TABLE IF NOT EXISTS commands_fts USING fts5(
                 command, output, content='commands', content_rowid='id'
             );
             CREATE TRIGGER IF NOT EXISTS commands_ai AFTER INSERT ON commands BEGIN
                 INSERT INTO commands_fts(rowid, command, output)
                 VALUES (new.id, new.command, new.output);
             END;
             CREATE TRIGGER IF NOT EXISTS commands_ad AFTER DELETE ON commands BEGIN
                 INSERT INTO commands_fts(commands_fts, rowid, command, output)
                 VALUES ('delete', old.id, old.command, old.output);
             END;",
        )?;
        Ok(Self { conn })
    }

    pub fn insert(&self, r: &Record) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO commands (command, output, cwd, exit, started_ms, finished_ms, host, session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.command,
                r.output,
                r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                r.exit,
                r.started_ms as i64,
                r.finished_ms as i64,
                r.host,
                r.session,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn search(&self, q: &Query) -> rusqlite::Result<Vec<Entry>> {
        let mut sql = String::from(
            "SELECT c.id, c.command, c.cwd, c.exit, c.started_ms, c.finished_ms, c.host, c.session, ",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let fts = q.text.as_deref().map(fts_query).filter(|t| !t.is_empty());
        match &fts {
            Some(text) => {
                sql.push_str(
                    "snippet(commands_fts, 1, '[', ']', '…', 10)
                     FROM commands_fts JOIN commands c ON c.id = commands_fts.rowid
                     WHERE commands_fts MATCH ?",
                );
                args.push(Box::new(text.clone()));
            }
            None => sql.push_str("NULL FROM commands c WHERE 1"),
        }
        if q.failed_only {
            sql.push_str(" AND c.exit IS NOT NULL AND c.exit != 0");
        }
        if let Some(cwd) = &q.cwd {
            sql.push_str(" AND c.cwd = ?");
            args.push(Box::new(cwd.clone()));
        }
        if let Some(command) = &q.command {
            sql.push_str(" AND c.command = ?");
            args.push(Box::new(command.clone()));
        }
        if let Some(before) = q.before_ms {
            sql.push_str(" AND c.started_ms < ?");
            args.push(Box::new(before as i64));
        }
        sql.push_str(" ORDER BY c.started_ms DESC LIMIT ?");
        args.push(Box::new(q.limit.max(1) as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    command: row.get(1)?,
                    cwd: row.get(2)?,
                    exit: row.get(3)?,
                    started_ms: row.get::<_, i64>(4)? as u64,
                    finished_ms: row.get::<_, i64>(5)? as u64,
                    host: row.get(6)?,
                    session: row.get(7)?,
                    snippet: row.get(8)?,
                })
            },
        )?;
        rows.collect()
    }

    pub fn output(&self, id: i64) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT output FROM commands WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
    }

    /// The output of the most recent earlier run of `command` (in `cwd`,
    /// when given) -- what "diff with the previous run" compares against
    /// once it's no longer in the scrollback.
    pub fn previous_output(
        &self,
        command: &str,
        cwd: Option<&str>,
        before_ms: u64,
    ) -> rusqlite::Result<Option<String>> {
        let hit = self.search(&Query {
            command: Some(command.to_string()),
            cwd: cwd.map(str::to_string),
            before_ms: Some(before_ms),
            limit: 1,
            ..Query::default()
        })?;
        match hit.first() {
            Some(e) => self.output(e.id),
            None => Ok(None),
        }
    }
}

/// Turns free text into an FTS5 query that can't be a syntax error: each
/// word quoted, all required.
fn fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replaces likely secrets with `[redacted]`: private keys, cloud and
/// service tokens, bearer tokens, JWTs, and `password=`/`token:`-style
/// assignments.
pub fn redact(text: &str) -> String {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            (
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
                "[redacted private key]",
            ),
            (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", "[redacted]"),
            (r"\bgh[pousr]_[A-Za-z0-9]{36,}\b", "[redacted]"),
            (r"\bgithub_pat_[A-Za-z0-9_]{40,}\b", "[redacted]"),
            (r"\bglpat-[A-Za-z0-9_-]{20,}\b", "[redacted]"),
            (r"\bxox[abposr]-[A-Za-z0-9-]{10,}\b", "[redacted]"),
            (r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}\b", "[redacted]"),
            (r"\bAIza[0-9A-Za-z_-]{35}\b", "[redacted]"),
            (r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b", "[redacted]"),
            (r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]{16,}", "$1 [redacted]"),
            (
                r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)(["']?\s*[:=]\s*["']?)[^\s"']{4,}"#,
                "$1$2[redacted]",
            ),
        ]
        .into_iter()
        .filter_map(|(re, with)| Regex::new(re).ok().map(|r| (r, with)))
        .collect()
    });
    let mut out = text.to_string();
    for (re, with) in patterns {
        out = re.replace_all(&out, *with).into_owned();
    }
    out
}

/// Keeps the head and tail of very long output.
fn cap(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let half = max_bytes / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[… {} bytes omitted …]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

/// Settings for what gets recorded.
#[derive(Clone, Debug)]
pub struct Policy {
    pub max_output_bytes: usize,
    pub ignore: Vec<Regex>,
}

impl Policy {
    pub fn from_config(c: &crate::config::HistoryConfig) -> Self {
        Self {
            max_output_bytes: c.max_output_kb.saturating_mul(1024),
            ignore: c
                .ignore
                .iter()
                .filter_map(|p| match Regex::new(p) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        eprintln!("cyberterm: history ignore pattern `{p}`: {e}");
                        None
                    }
                })
                .collect(),
        }
    }

    fn skips(&self, command: &str) -> bool {
        command.starts_with(' ') || self.ignore.iter().any(|r| r.is_match(command))
    }
}

/// Collects commands that finished since the last call (marking them
/// recorded), with their output read from the terminal.
pub fn collect<T>(
    term: &Term<T>,
    shell: &mut ShellState,
    session: &str,
    policy: &Policy,
) -> Vec<Record> {
    let host = hostname();
    let mut out = Vec::new();
    for block in shell.blocks.iter_mut() {
        if block.recorded || block.finished_ms.is_none() || !block.is_command() {
            continue;
        }
        block.recorded = true;
        let command = block.command.clone().unwrap_or_default();
        if policy.skips(&command) {
            continue;
        }
        let output = crate::blocks::span_of(term, block.mark)
            .map(|span| crate::blocks::output_text(term, &span))
            .unwrap_or_default();
        out.push(Record {
            command: redact(command.trim()),
            output: cap(redact(&output), policy.max_output_bytes),
            cwd: block.cwd.clone(),
            exit: block.exit,
            started_ms: block.started_ms.unwrap_or_default(),
            finished_ms: block.finished_ms.unwrap_or_default(),
            host: host.clone(),
            session: session.to_string(),
        });
    }
    out
}

/// `2026-10-09 14:22` in local time.
pub fn format_time(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the provided struct.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::from("?");
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

/// Writes records on a background thread so the terminal never waits on
/// the disk.
#[derive(Clone)]
pub struct Recorder {
    tx: Sender<Record>,
}

impl Recorder {
    pub fn start(path: PathBuf) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Record>();
        std::thread::Builder::new()
            .name("history".into())
            .spawn(move || {
                let store = match Store::open(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("cyberterm: history disabled ({}): {e}", path.display());
                        return;
                    }
                };
                while let Ok(record) = rx.recv() {
                    if let Err(e) = store.insert(&record) {
                        eprintln!("cyberterm: history write failed: {e}");
                    }
                }
            })
            .ok()?;
        Some(Self { tx })
    }

    pub fn record(&self, records: Vec<Record>) {
        for r in records {
            let _ = self.tx.send(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "cyberterm-history-{}-{name}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).unwrap(), path)
    }

    fn rec(command: &str, output: &str, exit: i32, started: u64) -> Record {
        Record {
            command: command.into(),
            output: output.into(),
            cwd: Some("/proj".into()),
            exit: Some(exit),
            started_ms: started,
            finished_ms: started + 1500,
            host: "box".into(),
            session: "1".into(),
        }
    }

    #[test]
    fn stores_and_searches_commands_and_output() {
        let (store, path) = temp_store("search");
        store
            .insert(&rec(
                "curl api/health",
                "{\"status\":\"degraded\"}",
                0,
                1000,
            ))
            .unwrap();
        store
            .insert(&rec(
                "cargo test",
                "test result: FAILED. 3 passed; 1 failed",
                101,
                2000,
            ))
            .unwrap();
        store
            .insert(&rec("cargo test", "test result: ok. 4 passed", 0, 3000))
            .unwrap();

        let all = store
            .search(&Query {
                limit: 10,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].started_ms, 3000, "newest first");

        let hits = store
            .search(&Query {
                text: Some("degraded".into()),
                limit: 10,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].command, "curl api/health");
        assert!(hits[0].snippet.as_deref().unwrap().contains("[degraded]"));

        let failed = store
            .search(&Query {
                failed_only: true,
                limit: 10,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].exit, Some(101));

        // Quotes and FTS operators in user text can't break the query.
        assert!(store
            .search(&Query {
                text: Some("\"AND NOT(* -".into()),
                limit: 5,
                ..Query::default()
            })
            .is_ok());

        let prev = store
            .previous_output("cargo test", Some("/proj"), 3000)
            .unwrap();
        assert_eq!(
            prev.as_deref(),
            Some("test result: FAILED. 3 passed; 1 failed")
        );
        assert_eq!(
            store.previous_output("cargo test", None, 2000).unwrap(),
            None
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn secrets_are_redacted() {
        let text = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\n\
                    aws AKIAABCDEFGHIJKLMNOP\n\
                    Authorization: Bearer abcdef0123456789abcdef\n\
                    password=hunter22 and api_key: \"s3cr3tvalue\"\n\
                    -----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n\
                    normal text stays";
        let r = redact(text);
        for secret in [
            "ghp_abc",
            "AKIAABCD",
            "abcdef0123456789abcdef",
            "hunter22",
            "s3cr3tvalue",
            "AAAA",
        ] {
            assert!(!r.contains(secret), "{secret} leaked: {r}");
        }
        assert!(r.contains("normal text stays"));
        assert!(r.contains("password=[redacted]"));
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let text = format!("{}MIDDLE{}", "a".repeat(100), "z".repeat(100));
        let capped = cap(text, 60);
        assert!(capped.starts_with("aaa"));
        assert!(capped.ends_with("zzz"));
        assert!(capped.contains("omitted"));
        assert!(!capped.contains("MIDDLE"));
        assert_eq!(cap("short".into(), 60), "short");
    }

    #[test]
    fn collect_records_finished_commands_once() {
        use crate::frame::tests::{feed, test_term};
        use crate::shell::tap::OscTap;
        use parking_lot::Mutex;
        use std::sync::Arc;

        let mut term = test_term(30, 10);
        let shell = Arc::new(Mutex::new(ShellState::default()));
        let mut tap = OscTap::new(shell.clone());
        let mut bytes = Vec::new();
        tap.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\r\n\x1b]133;C;cmdline_url=echo%20hi\x07hi\r\n\x1b]133;D;0\x07\
              \x1b]133;A\x07$ \x1b]133;B\x07 secret-cmd\r\n\x1b]133;C;cmdline_url=%20secret-cmd\x07x\r\n\x1b]133;D;0\x07\
              \x1b]133;A\x07$ \x1b]133;B\x07",
            &mut bytes,
        );
        feed(&mut term, &bytes);
        let policy = Policy {
            max_output_bytes: 1024,
            ignore: vec![],
        };
        let mut state = shell.lock().clone();
        let records = collect(&term, &mut state, "s", &policy);
        assert_eq!(records.len(), 1, "space-prefixed command skipped");
        assert_eq!(records[0].command, "echo hi");
        assert_eq!(records[0].output, "hi");
        assert_eq!(records[0].exit, Some(0));
        assert!(
            collect(&term, &mut state, "s", &policy).is_empty(),
            "only once"
        );
    }
}
