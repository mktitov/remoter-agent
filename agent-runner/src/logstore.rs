//! Persistent JSONL store for per-run ACP logs (spec §7.1).
//!
//! Each run writes to `<logs_dir>/task-<task_id>/run-<run_id>.jsonl`.  Entries are
//! monotonically sequenced per run; an in-memory broadcast channel forwards
//! live entries to active WebSocket subscriptions.  Retention is enforced by
//! file mtime: stale files and empty directories are removed on startup and
//! once per hour.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::LogsConfig;

/// One structured log entry, persisted as a single JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: LogEntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogEntryKind {
    Prompt,
    Thought,
    Message,
    ToolCall,
    ToolCallUpdate,
    Plan,
    Error,
}

impl LogEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LogEntryKind::Prompt => "prompt",
            LogEntryKind::Thought => "thought",
            LogEntryKind::Message => "message",
            LogEntryKind::ToolCall => "toolCall",
            LogEntryKind::ToolCallUpdate => "toolCallUpdate",
            LogEntryKind::Plan => "plan",
            LogEntryKind::Error => "error",
        }
    }
}

/// A partially-built entry before the store assigns a sequence number.
#[derive(Debug, Clone)]
pub struct NewLogEntry {
    pub kind: LogEntryKind,
    pub text: Option<String>,
    pub tool_id: Option<String>,
    pub tool_kind: Option<String>,
    pub tool_title: Option<String>,
    pub tool_status: Option<String>,
}

impl NewLogEntry {
    pub fn prompt(text: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::Prompt,
            text: Some(text.into()),
            tool_id: None,
            tool_kind: None,
            tool_title: None,
            tool_status: None,
        }
    }

    pub fn message(text: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::Message,
            text: Some(text.into()),
            tool_id: None,
            tool_kind: None,
            tool_title: None,
            tool_status: None,
        }
    }

    pub fn thought(text: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::Thought,
            text: Some(text.into()),
            tool_id: None,
            tool_kind: None,
            tool_title: None,
            tool_status: None,
        }
    }

    pub fn plan(text: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::Plan,
            text: Some(text.into()),
            tool_id: None,
            tool_kind: None,
            tool_title: None,
            tool_status: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::Error,
            text: Some(text.into()),
            tool_id: None,
            tool_kind: None,
            tool_title: None,
            tool_status: None,
        }
    }

    pub fn tool_call(tool_id: impl Into<String>, kind: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            kind: LogEntryKind::ToolCall,
            text: None,
            tool_id: Some(tool_id.into()),
            tool_kind: Some(kind.into()),
            tool_title: Some(title.into()),
            tool_status: Some("pending".to_string()),
        }
    }

    pub fn tool_call_update(
        tool_id: impl Into<String>,
        kind: Option<String>,
        title: Option<String>,
        status: Option<String>,
    ) -> Self {
        Self {
            kind: LogEntryKind::ToolCallUpdate,
            text: None,
            tool_id: Some(tool_id.into()),
            tool_kind: kind,
            tool_title: title,
            tool_status: status,
        }
    }
}

struct ActiveRun {
    file: fs::File,
    next_seq: i64,
    tx: broadcast::Sender<LogEntry>,
}

struct Inner {
    logs_dir: PathBuf,
    retention_days: u64,
    runs: Mutex<HashMap<(i32, i32), ActiveRun>>,
}

/// Persistent JSONL store for per-run ACP logs.
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogStore")
            .field("logs_dir", &self.inner.logs_dir)
            .field("retention_days", &self.inner.retention_days)
            .finish_non_exhaustive()
    }
}

impl LogStore {
    pub fn new(cfg: &LogsConfig) -> anyhow::Result<Self> {
        let logs_dir = cfg
            .dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("remoter-agent-logs"));
        if !logs_dir.exists() {
            fs::create_dir_all(&logs_dir)?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                logs_dir,
                retention_days: cfg.retention_days,
                runs: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Opens (or reopens) the log file for a run and returns a handle that can
    /// be cloned into tasks.  Closing is optional — dropping all handles and
    /// calling [`LogStore::close_run`] stops live broadcasts for this run.
    pub fn open_run(&self, task_id: i32, run_id: i32) -> anyhow::Result<RunLogger> {
        let path = run_path(&self.inner.logs_dir, task_id, run_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let next_seq = read_next_seq(&path)?;
        let file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let mut runs = self.inner.runs.lock().expect("logstore runs poisoned");
        let entry = runs.entry((task_id, run_id)).or_insert_with(|| ActiveRun {
            file,
            next_seq,
            tx: broadcast::channel(1024).0,
        });
        // If the file was already open (e.g. a previous handle is still alive),
        // keep the existing broadcast and sequence counter.
        entry.next_seq = entry.next_seq.max(next_seq);
        Ok(RunLogger {
            store: self.clone(),
            task_id,
            run_id,
        })
    }

    /// Closes the live broadcast channel for a run.  Writes via existing
    /// handles still append to the file, but new subscriptions get no live
    /// stream.  This is called when the run finishes.
    pub fn close_run(&self, task_id: i32, run_id: i32) {
        let mut runs = self.inner.runs.lock().expect("logstore runs poisoned");
        runs.remove(&(task_id, run_id));
    }

    /// Returns a live-subscription receiver for an active run, if any.
    pub fn subscribe(&self, task_id: i32, run_id: i32) -> Option<broadcast::Receiver<LogEntry>> {
        let runs = self.inner.runs.lock().expect("logstore runs poisoned");
        runs.get(&(task_id, run_id)).map(|r| r.tx.subscribe())
    }

    /// Reads historical entries for a run.
    ///
    /// * `from_seq` — start at this sequence number (inclusive).  Takes
    ///   precedence over `tail` when both are provided.
    /// * `tail` — read the last `tail` entries instead of starting from the
    ///   beginning.
    /// * `limit` — cap the number of returned entries (default 100, max 1000).
    ///
    /// Returns the entries plus the sequence number of the next entry that
    /// would follow the returned batch (or the total count if at the end).
    pub fn read(
        &self,
        task_id: i32,
        run_id: i32,
        from_seq: Option<i64>,
        tail: Option<usize>,
        limit: Option<usize>,
    ) -> anyhow::Result<(Vec<LogEntry>, i64)> {
        let path = run_path(&self.inner.logs_dir, task_id, run_id);
        let limit = limit.unwrap_or(100).clamp(1, 1000);

        let file = fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut entries: Vec<LogEntry> = Vec::new();
        let mut total = 0i64;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: LogEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(error = %e, line = %line, "skipping malformed JSONL line");
                    continue;
                }
            };
            total += 1;
            if let Some(start) = from_seq
                && entry.seq < start
            {
                continue;
            }
            entries.push(entry);
            if entries.len() >= limit {
                break;
            }
        }

        if let Some(tail) = tail
            && from_seq.is_none()
            && entries.len() > tail
        {
            entries = entries.split_off(entries.len() - tail);
        }

        Ok((entries, total))
    }

    /// Whether the run file exists on disk.
    pub fn exists(&self, task_id: i32, run_id: i32) -> bool {
        run_path(&self.inner.logs_dir, task_id, run_id).exists()
    }

    /// Counts valid entries in the run file.  Used to compute a tail offset.
    pub fn count_entries(&self, task_id: i32, run_id: i32) -> anyhow::Result<i64> {
        let path = run_path(&self.inner.logs_dir, task_id, run_id);
        if !path.exists() {
            return Ok(0);
        }
        let file = fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut count = 0i64;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if serde_json::from_str::<LogEntry>(&line).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Removes log files whose mtime is older than `retention_days` and deletes
    /// empty task directories.
    pub fn sweep(&self) -> anyhow::Result<()> {
        let retention_days = self.inner.retention_days;
        if retention_days == 0 {
            return Ok(());
        }
        let cutoff = SystemTime::now() - Duration::from_secs(retention_days * 24 * 3600);
        let dir = fs::read_dir(&self.inner.logs_dir)?;
        for entry in dir {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            sweep_dir(entry.path(), cutoff)?;
        }
        Ok(())
    }

    /// How often the daemon should run the retention sweep.
    pub fn sweep_interval() -> Duration {
        Duration::from_secs(3600)
    }

    fn append(&self, task_id: i32, run_id: i32, entry: &LogEntry) -> anyhow::Result<()> {
        let mut runs = self.inner.runs.lock().expect("logstore runs poisoned");
        let Some(active) = runs.get_mut(&(task_id, run_id)) else {
            return Ok(());
        };
        let line = serde_json::to_string(entry)?;
        writeln!(active.file, "{line}")?;
        active.file.flush()?;
        active.next_seq = entry.seq + 1;
        // Drop the lock before broadcasting so subscribers do not block writers.
        let tx = active.tx.clone();
        drop(runs);
        let _ = tx.send(entry.clone());
        Ok(())
    }
}

fn run_path(logs_dir: &Path, task_id: i32, run_id: i32) -> PathBuf {
    logs_dir
        .join(format!("task-{task_id}"))
        .join(format!("run-{run_id}.jsonl"))
}

fn read_next_seq(path: &Path) -> anyhow::Result<i64> {
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut max_seq = -1i64;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<LogEntry>(&line)
            && entry.seq > max_seq
        {
            max_seq = entry.seq;
        }
    }
    Ok(max_seq + 1)
}

fn sweep_dir(path: PathBuf, cutoff: SystemTime) -> anyhow::Result<()> {
    let mut empty = true;
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            let meta = entry.metadata()?;
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if mtime < cutoff {
                if let Err(e) = fs::remove_file(entry.path()) {
                    tracing::warn!(path = %entry.path().display(), error = %e, "retention sweep: failed to remove stale log file");
                } else {
                    tracing::info!(path = %entry.path().display(), "retention sweep: removed stale log file");
                }
            } else {
                empty = false;
            }
        } else if file_type.is_dir() {
            sweep_dir(entry.path(), cutoff)?;
            if fs::read_dir(entry.path())?.next().is_none() {
                let _ = fs::remove_dir(entry.path());
            } else {
                empty = false;
            }
        }
    }
    if empty {
        let _ = fs::remove_dir(&path);
    }
    Ok(())
}

/// A cloneable handle that appends entries to one run's log.
#[derive(Clone, Debug)]
pub struct RunLogger {
    store: LogStore,
    task_id: i32,
    run_id: i32,
}

impl RunLogger {
    /// Appends one entry to the run log.  Never blocks the caller: if the
    /// in-memory broadcast queue is full the live subscriber simply lags.
    pub fn log(&self, entry: NewLogEntry) {
        let seq = {
            let mut runs = self.store.inner.runs.lock().expect("logstore runs poisoned");
            let Some(active) = runs.get_mut(&(self.task_id, self.run_id)) else {
                return;
            };
            let seq = active.next_seq;
            active.next_seq += 1;
            seq
        };
        let full = LogEntry {
            seq,
            ts: Utc::now(),
            kind: entry.kind,
            text: entry.text,
            tool_id: entry.tool_id,
            tool_kind: entry.tool_kind,
            tool_title: entry.tool_title,
            tool_status: entry.tool_status,
        };
        if let Err(e) = self.store.append(self.task_id, self.run_id, &full) {
            tracing::warn!(task_id = self.task_id, run_id = self.run_id, error = %e, "failed to append ACP log entry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_store() -> LogStore {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("remoter-logstore-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        LogStore::new(&LogsConfig {
            dir: Some(dir.clone()),
            retention_days: 7,
        })
        .unwrap()
    }

    #[test]
    fn append_and_read() {
        let store = temp_store();
        let logger = store.open_run(1, 2).unwrap();
        logger.log(NewLogEntry::prompt("hello"));
        logger.log(NewLogEntry::message("world"));
        let (entries, next) = store.read(1, 2, None, None, None).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, LogEntryKind::Prompt);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[1].seq, 1);
        assert_eq!(next, 2);
    }

    #[test]
    fn read_from_seq_and_tail() {
        let store = temp_store();
        let logger = store.open_run(3, 4).unwrap();
        for i in 0..10 {
            logger.log(NewLogEntry::message(format!("m{i}")));
        }
        let (from_seq, _) = store.read(3, 4, Some(5), None, Some(100)).unwrap();
        assert_eq!(from_seq.len(), 5);
        assert_eq!(from_seq[0].seq, 5);

        let (tail, _) = store.read(3, 4, None, Some(3), Some(100)).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.last().unwrap().seq, 9);
    }

    #[test]
    fn live_broadcast() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let store = temp_store();
            let logger = store.open_run(5, 6).unwrap();
            let mut rx = store.subscribe(5, 6).unwrap();
            logger.log(NewLogEntry::prompt("x"));
            let e = rx.recv().await.unwrap();
            assert_eq!(e.kind, LogEntryKind::Prompt);
            assert_eq!(e.seq, 0);
        });
    }

    #[test]
    fn retention_sweep_removes_old_files() {
        let dir = std::env::temp_dir().join(format!("remoter-logstore-retention-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = LogStore::new(&LogsConfig {
            dir: Some(dir.clone()),
            retention_days: 1,
        })
        .unwrap();
        let logger = store.open_run(7, 8).unwrap();
        logger.log(NewLogEntry::message("old"));

        let file = run_path(&dir, 7, 8);
        let old = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        let _ = fs::File::options().write(true).open(&file).unwrap().set_modified(old);

        store.sweep().unwrap();
        assert!(!file.exists());
    }
}
