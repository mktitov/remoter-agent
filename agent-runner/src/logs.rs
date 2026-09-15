//! Log shipping (ticket: agent logs in the Users tab). The daemon sends only
//! its own tracing output to the backend's `POST /agent-logs`, captured by
//! [ShipperLayer] (a tracing layer next to the stdout fmt layer).  ACP
//! conversation logs are stored locally as structured JSONL (spec §7.1) and
//! streamed to viewers on demand.
//!
//! A background task ([run_shipper]) batches lines and POSTs them every
//! [FLUSH_INTERVAL] or when [FLUSH_BATCH] lines accumulate. Shipping is
//! best-effort: a full queue or a failed POST drops lines (counted, never
//! fatal) — logging must not block or break ticket work.

use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing_subscriber::Layer;

use crate::client::RemoterClient;

/// Queue capacity; a flooded queue drops new lines (best-effort).
const QUEUE_CAPACITY: usize = 1024;
/// Flush when this many lines accumulated…
const FLUSH_BATCH: usize = 50;
/// …or when this much time passed since the last flush.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One line to ship. String levels/sources match the backend's
/// `AgentLogLevel`/`AgentLogSource` serde tags.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub source: &'static str,
    pub ts: DateTime<Utc>,
    pub level: &'static str,
    pub message: String,
}

/// Cloneable producer handle shared by the tracing layer and the ACP driver.
#[derive(Clone)]
pub struct LogSink {
    tx: mpsc::Sender<LogLine>,
}

impl LogSink {
    pub fn new(tx: mpsc::Sender<LogLine>) -> Self {
        Self { tx }
    }

    /// The channel both producers feed; pair with [run_shipper].
    pub fn channel() -> (LogSink, mpsc::Receiver<LogLine>) {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        (LogSink::new(tx), rx)
    }

    pub(crate) fn push(&self, source: &'static str, level: &'static str, message: String) {
        // try_send: a full queue must never block a run; the line is dropped.
        let _ = self.tx.try_send(LogLine {
            source,
            ts: Utc::now(),
            level,
            message,
        });
    }
}

/// Tracing layer that mirrors every event (as `target: message fields`) into
/// the ship queue with `source = "daemon"`.
pub struct ShipperLayer {
    sink: LogSink,
}

impl ShipperLayer {
    pub fn new(sink: LogSink) -> Self {
        Self { sink }
    }
}

impl<S> Layer<S> for ShipperLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let level = match *meta.level() {
            tracing::Level::ERROR => "error",
            tracing::Level::WARN => "warn",
            tracing::Level::INFO => "info",
            tracing::Level::DEBUG => "debug",
            tracing::Level::TRACE => "trace",
        };
        self.sink.push("daemon", level, visitor.render(meta.target()));
    }
}

/// Collects the event's `message` plus any structured fields (`error=…`,
/// `task_id=…`) so shipped lines read like the stdout fmt output. The default
/// `Visit` methods all forward to `record_debug`.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: String,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

impl FieldVisitor {
    fn render(self, target: &str) -> String {
        format!("{target}: {}{}", self.message, self.fields)
    }
}

/// Batching consumer: POSTs accumulated lines to the backend. Runs until the
/// queue closes (daemon shutdown), flushing the remainder.
pub async fn run_shipper(mut rx: mpsc::Receiver<LogLine>, client: RemoterClient) {
    let mut buf: Vec<LogLine> = Vec::new();
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    loop {
        tokio::select! {
            line = rx.recv() => {
                match line {
                    Some(l) => {
                        buf.push(l);
                        if buf.len() >= FLUSH_BATCH {
                            flush(&client, &mut buf).await;
                        }
                    }
                    None => {
                        flush(&client, &mut buf).await;
                        break;
                    }
                }
            }
            _ = tick.tick() => flush(&client, &mut buf).await,
        }
    }
}

async fn flush(client: &RemoterClient, buf: &mut Vec<LogLine>) {
    if buf.is_empty() {
        return;
    }
    let batch = std::mem::take(buf);
    if let Err(e) = client.post_agent_logs(&batch).await {
        tracing::warn!(dropped = batch.len(), error = %e, "log shipment failed; dropping batch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_queue_drops_without_blocking() {
        let (tx, mut rx) = mpsc::channel(1);
        let sink = LogSink::new(tx);
        sink.push("daemon", "info", "first".to_string());
        // Second send must not block or panic — the line is dropped.
        sink.push("daemon", "info", "second".to_string());
        assert_eq!(rx.blocking_recv().unwrap().message, "first");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sink_stamps_source_and_level() {
        let (sink, mut rx) = LogSink::channel();
        sink.push("daemon", "warn", "hello".to_string());
        let line = rx.blocking_recv().unwrap();
        assert_eq!(line.source, "daemon");
        assert_eq!(line.level, "warn");
        assert_eq!(line.message, "hello");
    }
}
