//! WebSocket streaming of per-run ACP logs (spec §4.5/§4.6).
//!
//! The backend owns the viewer-facing socket and relays commands to the daemon
//! over the existing agent-events WebSocket.  This module parses those commands
//! and produces the response frames the daemon sends back.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::logstore::{LogEntry, LogStore};

/// Maximum number of entries shipped in one `acpLogEntries` frame.
pub const MAX_BATCH_ENTRIES: usize = 100;
/// Soft ceiling on the serialized size of one `acpLogEntries` frame (JSON).
pub const MAX_BATCH_BYTES: usize = 64 * 1024;
/// How often live subscriptions are flushed to the WebSocket.
pub const FLUSH_INTERVAL_MS: u64 = 200;

/// Commands the backend sends to the daemon over the agent-events WebSocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AcpLogCommand {
    /// Start (or resume) streaming a run's log.
    AcpLogSubscribe {
        subscription_id: String,
        task_id: i32,
        run_id: i32,
        /// Inclusive starting sequence number.  Takes precedence over `tail`.
        from_seq: Option<i64>,
        /// Stream the last `tail` entries instead of starting from the beginning.
        tail: Option<usize>,
    },
    /// Stop streaming a subscription.
    AcpLogUnsubscribe { subscription_id: String },
}

/// Responses the daemon sends back to the backend over the agent-events WebSocket.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AcpLogResponse {
    /// One batch of historical or live entries.
    AcpLogEntries {
        subscription_id: String,
        entries: Vec<LogEntry>,
    },
    /// The subscription has ended; no more frames will arrive for it.
    AcpLogEnd { subscription_id: String, reason: String },
}

impl AcpLogResponse {
    pub fn end(subscription_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::AcpLogEnd {
            subscription_id: subscription_id.into(),
            reason: reason.into(),
        }
    }
}

struct Subscription {
    task_id: i32,
    run_id: i32,
    /// Next sequence number expected from the file or live broadcast.
    next_seq: i64,
    /// `Some` while the run is still active in this daemon process.
    live: Option<broadcast::Receiver<LogEntry>>,
    /// Set when `AcpLogEnd` has been emitted.
    ended: bool,
}

/// Owns all active ACP-log subscriptions for one daemon→backend WebSocket.
pub struct LogStreamManager {
    store: LogStore,
    subs: HashMap<String, Subscription>,
}

impl LogStreamManager {
    pub fn new(store: LogStore) -> Self {
        Self {
            store,
            subs: HashMap::new(),
        }
    }

    /// Handles one incoming command.  Returns all response frames that should be
    /// sent immediately: the first backlog batch when the run already has
    /// persisted entries, and possibly an `AcpLogEnd` for an unknown or already
    /// finished run.
    pub fn handle_command(&mut self, cmd: AcpLogCommand) -> Vec<AcpLogResponse> {
        match cmd {
            AcpLogCommand::AcpLogUnsubscribe { subscription_id } => {
                self.subs.remove(&subscription_id);
                Vec::new()
            }
            AcpLogCommand::AcpLogSubscribe {
                subscription_id,
                task_id,
                run_id,
                from_seq,
                tail,
            } => {
                // Remove any previous subscription with the same id.
                self.subs.remove(&subscription_id);

                let exists = self.store.exists(task_id, run_id);
                let live = self.store.subscribe(task_id, run_id);
                if !exists && live.is_none() {
                    return vec![AcpLogResponse::end(&subscription_id, "notFound")];
                }

                let start_seq = match from_seq {
                    Some(s) => s.max(0),
                    None => match tail {
                        Some(t) => {
                            let total = self.store.count_entries(task_id, run_id).unwrap_or(0);
                            (total - t as i64).max(0)
                        }
                        None => 0,
                    },
                };

                let sub = Subscription {
                    task_id,
                    run_id,
                    next_seq: start_seq,
                    live,
                    ended: false,
                };
                self.subs.insert(subscription_id.clone(), sub);
                self.drain_subscription(&subscription_id)
            }
        }
    }

    /// Called on every flush tick.  Returns frames for live entries or the
    /// remainder of a historical backlog.
    pub fn tick(&mut self) -> Vec<AcpLogResponse> {
        let ids: Vec<String> = self.subs.keys().cloned().collect();
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.drain_subscription(&id));
        }
        out
    }

    fn drain_subscription(&mut self, subscription_id: &str) -> Vec<AcpLogResponse> {
        let mut out = Vec::new();
        let mut remove = false;

        if let Some(sub) = self.subs.get_mut(subscription_id) {
            if sub.ended {
                return out;
            }

            // Backfill persisted entries first.  The JSONL file is the source
            // of truth: everything the live broadcast carries is written to
            // the file before it is broadcast, so this covers both the
            // historical backlog a fresh subscription is owed (spec §4.5:
            // stream "from `fromSeq` or the last `tail` entries" — live runs
            // included) and any entries a lagging broadcast receiver missed.
            // It also guarantees an immediate first frame on subscribe, which
            // the backend requires within its first-frame timeout.
            let (entries, has_more) = read_batch(&self.store, sub.task_id, sub.run_id, sub.next_seq);
            if let Some(last) = entries.last() {
                sub.next_seq = last.seq + 1;
            }
            if !entries.is_empty() {
                out.push(AcpLogResponse::AcpLogEntries {
                    subscription_id: subscription_id.to_string(),
                    entries,
                });
            }

            // Drain the live broadcast only once the backlog is caught up;
            // entries already shipped from the file are skipped by sequence.
            if !has_more && sub.live.is_some() {
                let mut batch = Vec::new();
                let mut closed = false;
                {
                    let live = sub.live.as_mut().expect("live checked above");
                    loop {
                        match live.try_recv() {
                            Ok(entry) => {
                                if entry.seq < sub.next_seq {
                                    continue; // already shipped from the file
                                }
                                if entry.seq > sub.next_seq {
                                    // Gap (e.g. after a lag): the entry and
                                    // everything missing before it are in the
                                    // file; the next tick's backfill resends
                                    // them in order.
                                    break;
                                }
                                let seq = entry.seq;
                                if !push_entry(&mut batch, entry) {
                                    // Batch full — keep `next_seq` so the file
                                    // backfill resends this entry next tick.
                                    break;
                                }
                                sub.next_seq = seq + 1;
                            }
                            Err(broadcast::error::TryRecvError::Empty) => break,
                            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                                // Missed entries are in the file; the next
                                // tick's backfill closes the gap.
                                break;
                            }
                            Err(broadcast::error::TryRecvError::Closed) => {
                                closed = true;
                                break;
                            }
                        }
                    }
                }
                if closed {
                    sub.live = None;
                }
                if !batch.is_empty() {
                    out.push(AcpLogResponse::AcpLogEntries {
                        subscription_id: subscription_id.to_string(),
                        entries: batch,
                    });
                }
            }

            // A run whose broadcast closed and whose backlog is exhausted is done.
            if sub.live.is_none() && !has_more {
                sub.ended = true;
                out.push(AcpLogResponse::end(subscription_id, "finished"));
                remove = true;
            }
        }

        if remove {
            self.subs.remove(subscription_id);
        }
        out
    }
}

/// Reads up to [`MAX_BATCH_ENTRIES`] entries starting at `next_seq`.  Returns
/// the entries and whether more entries likely exist.
fn read_batch(store: &LogStore, task_id: i32, run_id: i32, next_seq: i64) -> (Vec<LogEntry>, bool) {
    match store.read(task_id, run_id, Some(next_seq), None, Some(MAX_BATCH_ENTRIES)) {
        Ok((entries, _total)) => {
            let has_more = entries.len() >= MAX_BATCH_ENTRIES;
            (entries, has_more)
        }
        Err(e) => {
            tracing::warn!(task_id, run_id, error = %e, "failed to read ACP log batch");
            (Vec::new(), false)
        }
    }
}

/// Adds `entry` to `batch`, respecting [`MAX_BATCH_ENTRIES`] and a soft
/// [`MAX_BATCH_BYTES`] limit.  Returns `false` when the batch is full and the
/// entry was not added; an empty batch always accepts the entry.
fn push_entry(batch: &mut Vec<LogEntry>, entry: LogEntry) -> bool {
    if batch.len() >= MAX_BATCH_ENTRIES {
        return false;
    }
    // Approximate current serialized size without re-serializing the whole vec.
    if !batch.is_empty() {
        let approx = batch.len() * std::mem::size_of::<LogEntry>() + entry.text.as_ref().map(|t| t.len()).unwrap_or(0);
        if approx > MAX_BATCH_BYTES {
            return false;
        }
    }
    batch.push(entry);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogsConfig;
    use crate::logstore::NewLogEntry;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_store() -> LogStore {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("remoter-logstream-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        LogStore::new(&LogsConfig {
            dir: Some(dir),
            retention_days: 1,
        })
        .unwrap()
    }

    fn subscribe(from_seq: Option<i64>, tail: Option<usize>) -> AcpLogCommand {
        AcpLogCommand::AcpLogSubscribe {
            subscription_id: "s".to_string(),
            task_id: 1,
            run_id: 1,
            from_seq,
            tail,
        }
    }

    fn entries(frames: &[AcpLogResponse]) -> Vec<&LogEntry> {
        frames
            .iter()
            .flat_map(|f| match f {
                AcpLogResponse::AcpLogEntries { entries, .. } => entries.iter().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn end_reason(frames: &[AcpLogResponse]) -> Option<&str> {
        frames.iter().find_map(|f| match f {
            AcpLogResponse::AcpLogEnd { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
    }

    /// Regression: a fresh subscription on a *live* run must immediately
    /// receive the persisted backlog — previously the live broadcast short-
    /// circuited the file backfill, so viewers saw an empty journal and the
    /// backend's first-frame timeout fired (`acpLogError{daemonTimeout}`).
    #[test]
    fn live_subscription_gets_file_backlog() {
        let store = temp_store();
        let logger = store.open_run(1, 1).unwrap();
        logger.log(NewLogEntry::prompt("p"));
        logger.log(NewLogEntry::message("m"));

        let mut manager = LogStreamManager::new(store);
        let frames = manager.handle_command(subscribe(Some(0), None));

        let got = entries(&frames);
        assert_eq!(got.len(), 2, "expected the backlog, got {frames:?}");
        assert_eq!(got[0].seq, 0);
        assert_eq!(got[1].seq, 1);
        assert!(end_reason(&frames).is_none());
    }

    /// An entry that is both in the file and still queued in the live
    /// broadcast must be shipped exactly once.
    #[test]
    fn backlog_and_live_buffer_do_not_duplicate() {
        let store = temp_store();
        let logger = store.open_run(1, 1).unwrap();
        logger.log(NewLogEntry::prompt("p"));

        let mut manager = LogStreamManager::new(store);
        let frames = manager.handle_command(subscribe(Some(0), None));
        assert_eq!(entries(&frames).len(), 1);

        // No live entry arrived since; the tick must not resend anything.
        assert!(entries(&manager.tick()).is_empty());
    }

    #[test]
    fn live_entries_follow_the_backlog() {
        let store = temp_store();
        let logger = store.open_run(1, 1).unwrap();
        logger.log(NewLogEntry::prompt("p"));

        let mut manager = LogStreamManager::new(store);
        manager.handle_command(subscribe(Some(0), None));

        logger.log(NewLogEntry::message("live"));
        let frames = manager.tick();
        let got = entries(&frames);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].seq, 1);
        assert_eq!(got[0].text.as_deref(), Some("live"));
    }

    #[test]
    fn finished_run_streams_history_and_ends() {
        let store = temp_store();
        let logger = store.open_run(1, 1).unwrap();
        logger.log(NewLogEntry::prompt("p"));
        logger.log(NewLogEntry::message("m"));
        store.close_run(1, 1);

        let mut manager = LogStreamManager::new(store);
        let frames = manager.handle_command(subscribe(Some(0), None));

        assert_eq!(entries(&frames).len(), 2);
        assert_eq!(end_reason(&frames), Some("finished"));
        // The subscription is gone once ended.
        assert!(manager.tick().is_empty());
    }

    #[test]
    fn live_run_ends_after_close() {
        let store = temp_store();
        let logger = store.open_run(1, 1).unwrap();
        logger.log(NewLogEntry::prompt("p"));

        let mut manager = LogStreamManager::new(store.clone());
        manager.handle_command(subscribe(Some(0), None));

        store.close_run(1, 1);
        drop(logger);
        let frames = manager.tick();
        assert_eq!(end_reason(&frames), Some("finished"));
    }

    #[test]
    fn unknown_run_ends_not_found() {
        let store = temp_store();
        let mut manager = LogStreamManager::new(store);
        let frames = manager.handle_command(subscribe(Some(0), None));
        assert_eq!(end_reason(&frames), Some("notFound"));
    }
}
