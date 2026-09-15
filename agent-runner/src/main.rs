//! `remoter-agent` daemon entry point: load config, validate the agent token,
//! reconcile orphans from a previous process, then poll forever (spec §5.2).

use std::sync::Arc;

use remoter_agent::{
    claim::Daemon,
    client::RemoterClient,
    config::Config,
    driver, events, logs,
    logstore::LogStore,
    sync::{PrFeedbackSync, PrMergeableSync},
};
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log shipping (ticket: agent logs in the Users tab): besides stdout, every
    // tracing event is mirrored into a queue that a background task batches to
    // the backend (`source = "daemon"`).
    let (log_sink, log_rx) = logs::LogSink::channel();
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter.clone()))
        .with(logs::ShipperLayer::new(log_sink.clone()).with_filter(filter))
        .init();

    let config = Arc::new(Config::load()?);
    config.validate_execution()?;
    tracing::info!(
        api_url = %config.api_url,
        workspace_root = %config.workspace_root.display(),
        max_concurrent_runs = config.max_concurrent_runs,
        execution_mode = ?config.execution.mode,
        "configuration loaded"
    );

    let log_store = LogStore::new(&config.logs)?;
    if let Err(e) = log_store.sweep() {
        tracing::warn!(error = %e, "initial log retention sweep failed");
    }
    let sweep_store = log_store.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LogStore::sweep_interval()).await;
            if let Err(e) = sweep_store.sweep() {
                tracing::warn!(error = %e, "log retention sweep failed");
            }
        }
    });

    let client = RemoterClient::new(&config.api_url, &config.token, config.workspace_id);
    let event_client = client.clone();
    let driver = driver::from_config(&config.driver, &config.api_url, &config.token, config.workspace_id)?;
    let mut daemon = Daemon::new(config.clone(), client, driver, log_store.clone());

    // Best-effort log shipper: batches the queue to `POST /agent-logs`.
    tokio::spawn(logs::run_shipper(
        log_rx,
        RemoterClient::new(&config.api_url, &config.token, config.workspace_id),
    ));

    // Retries transient backend outages forever; fatal on a human token or a
    // non-transient HTTP error (spec §5.1/§5.7).
    daemon.validate_token().await?;

    // Finish orphan `running` rows from a previous process (spec §5.7).
    daemon.reconcile().await;

    let staging_handle = daemon.staging_handle();
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    // Event stream (spec §4.5): push wake-ups on top of the poll. Capacity 1 —
    // a burst of events coalesces into one pending wake-up, and the consumer
    // drains before re-polling. When disabled, the receiver just never fires.
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<()>(1);
    if config.events_enabled {
        tokio::spawn(events::run_with_staging(
            config.clone(),
            events_tx,
            log_store.clone(),
            event_client.clone(),
            staging_handle.clone(),
        ));
    }
    // The data-plane is independent from event wake-ups: disabling events must
    // not make an already configured staging environment unreachable.
    tokio::spawn(events::run_stage_tunnel(config.clone(), staging_handle));
    // PR feedback sync (PROD-8, spec §4.4) and PR mergeability sync
    // (PROD-9): shared cadence (`pr_sync_interval_secs`), independent of the
    // claim poll; 0 disables both. First pass after one interval.
    let sync_interval = std::time::Duration::from_secs(config.pr_sync_interval_secs);
    let mut feedback_sync = PrFeedbackSync::new(&config.workspace_root);
    let mut mergeable_sync = PrMergeableSync::new(&config.workspace_root);
    let sync_client = RemoterClient::new(&config.api_url, &config.token, config.workspace_id);
    let mut last_sync = std::time::Instant::now();
    loop {
        daemon.poll_once().await;
        if config.pr_sync_interval_secs > 0 && last_sync.elapsed() >= sync_interval {
            feedback_sync.sync_once(&sync_client, &config).await;
            mergeable_sync.sync_once(&sync_client, &config).await;
            last_sync = std::time::Instant::now();
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            // A task event woke us: drain any queued duplicates and poll now
            // instead of waiting out the interval. `Some(_)` keeps the branch
            // disabled if the producer ever dies (closed channel).
            Some(_) = events_rx.recv() => {
                while events_rx.try_recv().is_ok() {}
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown requested; cancelling live runs");
                break;
            }
        }
    }
    daemon.shutdown().await;
    Ok(())
}
