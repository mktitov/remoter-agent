//! The daemon's wake-up channel (spec §4.5): a WebSocket to the backend's
//! `GET /api/v1/agent/events` that forwards every received task event as a
//! unit trigger to the main loop. The payload is a hint only — claiming and
//! cancellation still go through the REST poll cycle, so a lost socket costs
//! latency (the 30s fallback poll), never correctness.
//!
//! The same socket is now bidirectional: the backend can send ACP-log
//! subscription commands (`acpLogSubscribe` / `acpLogUnsubscribe`) and the
//! daemon streams the matching log frames back.  Non-command text frames keep
//! their old "poll now" semantics.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::client::{ProjectRepoConfig, RemoterClient};
use crate::config::Config;
use crate::logstore::LogStore;
use crate::logstream::{AcpLogCommand, AcpLogResponse, LogStreamManager};
use crate::staging::{self, StageCommand, StageState, StageTunnelFrame, StagingHandle};

/// Reconnect backoff cap. Between attempts the fallback poll keeps the daemon
/// working, so there is no reason to retry aggressively.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// WebSocket handshake timeout. Matches the REST client's `connect_timeout`
/// so a silent network black hole fails fast instead of hanging for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Dead-connection watchdog: the server heartbeats with a WS ping every 25s
/// (spec §4.5), so a connection with no traffic at all for three intervals is
/// half-open (a silently dead peer sends no RST) — drop it and let the
/// reconnect loop build a fresh one instead of stalling until the OS TCP
/// keepalive fires.
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// How often live subscriptions are flushed to the WebSocket.
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// Connects and listens forever, reconnecting with exponential backoff on
/// errors and server-side closes. Every non-command event becomes one pending
/// wake-up on `tx` (`try_send` coalesces bursts — the channel's capacity is 1).
/// Runs the legacy task-event stream without staging control. Kept as a small
/// compatibility wrapper for daemon integration tests and older embedders.
pub async fn run(config: Arc<Config>, tx: mpsc::Sender<()>, log_store: LogStore) {
    let client = RemoterClient::new(&config.api_url, &config.token, config.workspace_id);
    let staging = staging::StagingManager::handle(config.clone(), crate::image::ImageLocks::default());
    run_with_staging(config, tx, log_store, client, staging).await;
}

pub async fn run_with_staging(
    config: Arc<Config>,
    tx: mpsc::Sender<()>,
    log_store: LogStore,
    client: RemoterClient,
    staging: StagingHandle,
) {
    let url = ws_url(&config.api_url);
    let mut backoff = Duration::from_secs(config.retry_backoff_secs.max(1));
    loop {
        match listen_once(
            &url,
            &config.token,
            config.workspace_id,
            &tx,
            log_store.clone(),
            Some(client.clone()),
            Some(staging.clone()),
            IDLE_TIMEOUT,
            CONNECT_TIMEOUT,
        )
        .await
        {
            Ok(()) => {
                tracing::info!("event stream closed by the server; reconnecting");
                backoff = Duration::from_secs(config.retry_backoff_secs.max(1));
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "event stream failed; relying on the fallback poll");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection's lifetime: upgrade, then forward events until the stream
/// ends or errors. Any received frame (events, protocol pings/pongs, log
/// commands) resets the idle watchdog; `idle_timeout` without traffic means a
/// half-open connection and fails the connection so the caller reconnects. A
/// non-101 response (e.g. a 403 for a human token) fails the handshake with
/// `Error::Http`.
#[allow(clippy::too_many_arguments)]
async fn listen_once(
    url: &str,
    token: &str,
    workspace_id: Option<i32>,
    tx: &mpsc::Sender<()>,
    log_store: LogStore,
    client: Option<RemoterClient>,
    staging: Option<StagingHandle>,
    idle_timeout: Duration,
    connect_timeout: Duration,
) -> anyhow::Result<()> {
    // `&str::into_client_request` fills in the handshake headers (key,
    // version, upgrade) — a manually built `Request` would pass through
    // as-is and be rejected. A non-101 response (e.g. a 403 for a human
    // token) fails the handshake with `Error::Http`.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);
    if let Some(id) = workspace_id {
        request.headers_mut().insert("X-Workspace-Id", id.to_string().parse()?);
    }
    let (mut ws, _response) = tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| anyhow::anyhow!("websocket handshake timed out after {}s", connect_timeout.as_secs()))??;
    tracing::info!(url, "event stream connected");

    let mut manager = LogStreamManager::new(log_store);
    let mut flush = tokio::time::interval(FLUSH_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = tokio::time::timeout(idle_timeout, ws.next()) => {
                let msg = match msg {
                    Ok(Some(msg)) => msg,
                    Ok(None) => return Ok(()),
                    Err(_) => anyhow::bail!(
                        "no traffic for {}s — treating the connection as dead",
                        idle_timeout.as_secs()
                    ),
                };
                match msg? {
                    Message::Text(text) => {
                        if let Ok(command) = serde_json::from_str::<StageCommand>(&text) {
                            if let (Some(client), Some(staging)) = (client.as_ref(), staging.as_ref())
                                && let Some(frame) = handle_stage_command(command, client, staging).await
                            {
                                let json = serde_json::to_string(&frame)?;
                                if ws.send(Message::Text(json.into())).await.is_err() {
                                    return Ok(());
                                }
                            }
                        } else if let Some(responses) = handle_text(&text, tx, &mut manager) {
                            for resp in responses {
                                let json = serde_json::to_string(&resp)?;
                                if ws.send(Message::Text(json.into())).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    // Protocol pings/pongs are answered by the transport; they still
                    // count as traffic for the watchdog.
                    _ => {}
                }
            }
            _ = flush.tick() => {
                for resp in manager.tick() {
                    let json = serde_json::to_string(&resp)?;
                    if ws.send(Message::Text(json.into())).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Parses a text frame.  Recognized ACP-log commands are dispatched to the
/// [`LogStreamManager`]; everything else is treated as a wake-up hint.
fn handle_text(text: &str, tx: &mpsc::Sender<()>, manager: &mut LogStreamManager) -> Option<Vec<AcpLogResponse>> {
    match serde_json::from_str::<AcpLogCommand>(text) {
        Ok(cmd) => Some(manager.handle_command(cmd)),
        Err(_) => {
            let _ = tx.try_send(());
            None
        }
    }
}

/// `http(s)://host` → `ws(s)://host/api/v1/agent/events`.
fn ws_url(api_url: &str) -> String {
    format!("{}/api/v1/agent/events", api_url.replacen("http", "ws", 1))
}

/// The data-plane websocket URL. It is deliberately separate from the
/// bidirectional events/control socket so staging traffic cannot wake or
/// mutate the task poller.
pub fn stage_tunnel_url(api_url: &str) -> String {
    crate::staging::tunnel_url(api_url)
}

/// Sends the current environment snapshot immediately after a stage tunnel
/// reconnect. The backend registry is in-memory, so every new connection must
/// re-register rather than relying on the previous socket.
pub async fn send_stage_hello<S>(
    sink: &mut S,
    environments: impl Iterator<Item = crate::staging::StagingEnvironment>,
) -> anyhow::Result<()>
where
    S: futures::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let frame = crate::staging::hello_frame(environments);
    sink.send(Message::Text(serde_json::to_string(&frame)?.into())).await?;
    Ok(())
}

/// Maximum payload in one HTTP response frame.
pub const MAX_TUNNEL_CHUNK: usize = 256 * 1024;
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

enum WsCommand {
    Message(Vec<u8>),
    Close(u16, String),
}

/// Maintains the staging data-plane connection and re-registers all known
/// environments with a hello frame after every reconnect.
pub async fn run_stage_tunnel(config: Arc<Config>, staging: StagingHandle) {
    let mut backoff = Duration::from_secs(config.retry_backoff_secs.max(1));
    loop {
        match stage_tunnel_once(&config, &staging).await {
            Ok(()) => backoff = Duration::from_secs(config.retry_backoff_secs.max(1)),
            Err(e) => tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "stage tunnel failed"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn stage_tunnel_once(config: &Config, staging: &StagingHandle) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = stage_tunnel_url(&config.api_url).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", config.token).parse()?);
    if let Some(id) = config.workspace_id {
        request.headers_mut().insert("X-Workspace-Id", id.to_string().parse()?);
    }
    let (mut ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| anyhow::anyhow!("stage tunnel handshake timed out"))??;
    // Subscribe before taking the snapshot so a lifecycle update cannot land
    // in the gap between hello registration and the reactive update stream.
    let mut updates = staging.lock().await.subscribe_updates();
    let environments = staging.lock().await.snapshot();
    send_stage_hello(&mut ws, environments.into_iter()).await?;
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut proxies: HashMap<String, mpsc::UnboundedSender<WsCommand>> = HashMap::new();
    loop {
        let message = tokio::select! {
            message = ws.next() => message,
            update = updates.recv() => {
                if let Ok(frame) = update {
                    ws.send(Message::Text(serde_json::to_string(&frame)?.into())).await?;
                }
                continue;
            }
            frame = proxy_rx.recv() => {
                if let Some(frame) = frame {
                    ws.send(Message::Text(serde_json::to_string(&frame)?.into())).await?;
                }
                continue;
            }
        };
        let Some(message) = message else { break };
        match message? {
            Message::Text(text) => {
                let frame: StageTunnelFrame = match serde_json::from_str(&text) {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!(%error, "invalid stage tunnel frame");
                        continue;
                    }
                };
                if let StageTunnelFrame::WsOpen {
                    stream_id,
                    task_id,
                    route_path,
                    path,
                    headers,
                } = frame
                {
                    let response = open_ws_proxy(
                        stream_id.clone(),
                        task_id,
                        route_path,
                        path,
                        headers,
                        staging,
                        proxy_tx.clone(),
                    )
                    .await;
                    if let Some(sender) = response.1 {
                        proxies.insert(stream_id, sender);
                    }
                    ws.send(Message::Text(serde_json::to_string(&response.0)?.into()))
                        .await?;
                    continue;
                }
                if let StageTunnelFrame::WsMessage {
                    stream_id,
                    payload_base64,
                } = &frame
                {
                    if let Some(sender) = proxies.get(stream_id)
                        && let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(payload_base64)
                    {
                        let _ = sender.send(WsCommand::Message(payload));
                    }
                    continue;
                }
                if let StageTunnelFrame::WsClose {
                    stream_id,
                    code,
                    reason,
                } = &frame
                {
                    if let Some(sender) = proxies.remove(stream_id) {
                        let _ = sender.send(WsCommand::Close(*code, reason.clone()));
                    }
                    continue;
                }
                for response in handle_stage_frame(frame, staging).await {
                    ws.send(Message::Text(serde_json::to_string(&response)?.into())).await?;
                }
            }
            Message::Close(_) => return Ok(()),
            Message::Ping(payload) => ws.send(Message::Pong(payload)).await?,
            _ => {}
        }
    }
    Ok(())
}

async fn open_ws_proxy(
    stream_id: String,
    task_id: i32,
    route_path: String,
    path: String,
    headers: HashMap<String, String>,
    staging: &StagingHandle,
    output: tokio::sync::mpsc::UnboundedSender<StageTunnelFrame>,
) -> (StageTunnelFrame, Option<tokio::sync::mpsc::UnboundedSender<WsCommand>>) {
    let route = {
        let mut manager = staging.lock().await;
        let route = manager
            .get(task_id)
            .filter(|e| matches!(e.state, StageState::Running))
            .and_then(|e| staging::map_route(&e.routes, &route_path).cloned());
        manager.touch(task_id);
        route
    };
    let Some(route) = route.filter(|route| route.websocket) else {
        return (unsupported_ws(stream_id, "WebSocket route is not enabled"), None);
    };
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (path_without_query, query) = path
        .split_once('?')
        .map_or((path.as_str(), None), |(path, query)| (path, Some(query)));
    let mut url = format!(
        "ws://127.0.0.1:{}{}",
        route.host_port,
        local_request_path(&route.path, path_without_query)
    );
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    let mut request = match url.into_client_request() {
        Ok(request) => request,
        Err(error) => {
            return (
                unsupported_ws(stream_id, &format!("invalid local websocket URL: {error}")),
                None,
            );
        }
    };
    for (name, value) in headers {
        if !is_hop_by_hop(&name)
            && let (Ok(name), Ok(value)) = (
                name.parse::<tokio_tungstenite::tungstenite::http::header::HeaderName>(),
                value.parse::<tokio_tungstenite::tungstenite::http::header::HeaderValue>(),
            )
        {
            request.headers_mut().insert(name, value);
        }
    }
    let connected = tokio::time::timeout(LOCAL_REQUEST_TIMEOUT, tokio_tungstenite::connect_async(request)).await;
    let Ok(Ok((mut local, _))) = connected else {
        return (unsupported_ws(stream_id, "local websocket connection failed"), None);
    };
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    let accepted = StageTunnelFrame::WsAccepted {
        stream_id: stream_id.clone(),
        status: 101,
        headers: HashMap::new(),
    };
    tokio::spawn(async move {
        loop {
            tokio::select! {
                command = command_rx.recv() => match command {
                    Some(WsCommand::Message(payload)) => {
                        if local.send(Message::Binary(payload.into())).await.is_err() { break; }
                    }
                    Some(WsCommand::Close(code, reason)) => {
                        let _ = local.send(Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: code.into(), reason: reason.into()
                        }))).await;
                        break;
                    }
                    None => break,
                },
                message = tokio::time::timeout(WS_IDLE_TIMEOUT, local.next()) => match message {
                    Ok(Some(Ok(Message::Binary(payload)))) => {
                        let _ = output.send(StageTunnelFrame::WsMessage {
                            stream_id: stream_id.clone(),
                            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
                        });
                    }
                    Ok(Some(Ok(Message::Text(payload)))) => {
                        let _ = output.send(StageTunnelFrame::WsMessage {
                            stream_id: stream_id.clone(),
                            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload.as_bytes()),
                        });
                    }
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        let (code, reason) = frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1000, String::new()));
                        let _ = output.send(StageTunnelFrame::WsClose { stream_id: stream_id.clone(), code, reason });
                        break;
                    }
                    Ok(Some(Ok(_))) => {}
                    _ => {
                        let _ = output.send(StageTunnelFrame::WsClose { stream_id: stream_id.clone(), code: 1001, reason: "local websocket idle or disconnected".into() });
                        break;
                    }
                }
            }
        }
    });
    (accepted, Some(command_tx))
}

async fn handle_stage_frame(frame: StageTunnelFrame, staging: &StagingHandle) -> Vec<StageTunnelFrame> {
    match frame {
        StageTunnelFrame::HttpRequest {
            stream_id,
            task_id,
            route_path,
            method,
            path,
            query,
            headers,
            body_base64,
        } => {
            let request = HttpTunnelRequest {
                stream_id,
                task_id,
                route_path,
                method,
                path,
                query,
                headers,
                body_base64,
            };
            forward_http(staging, request).await
        }
        StageTunnelFrame::WsOpen {
            stream_id,
            task_id,
            route_path,
            ..
        } => {
            touch_environment(staging, task_id).await;
            let enabled = staging
                .lock()
                .await
                .get(task_id)
                .and_then(|e| staging::map_route(&e.routes, &route_path))
                .is_some_and(|r| r.websocket);
            vec![if enabled {
                StageTunnelFrame::WsClose {
                    stream_id,
                    code: 1013,
                    reason: "WebSocket proxy requires an active tunnel connection".to_string(),
                }
            } else {
                StageTunnelFrame::WsClose {
                    stream_id,
                    code: 1008,
                    reason: "WebSocket route is not enabled".to_string(),
                }
            }]
        }
        // A live tunnel handles WS messages in `stage_tunnel_once`; this
        // fallback must not reject an enabled route with close code 1008.
        StageTunnelFrame::WsMessage { .. } => Vec::new(),
        StageTunnelFrame::WsClose {
            stream_id,
            code,
            reason,
        } => {
            vec![StageTunnelFrame::WsClose {
                stream_id,
                code,
                reason,
            }]
        }
        StageTunnelFrame::Hello { .. }
        | StageTunnelFrame::EnvChanged { .. }
        | StageTunnelFrame::HttpResponse { .. }
        | StageTunnelFrame::WsAccepted { .. } => Vec::new(),
    }
}

async fn handle_stage_command(
    command: StageCommand,
    client: &RemoterClient,
    staging: &StagingHandle,
) -> Option<StageTunnelFrame> {
    let (task_id, restart) = match command {
        StageCommand::StageStart { task_id } => (task_id, false),
        StageCommand::StageStop { task_id } => (task_id, false),
        StageCommand::StageRestart { task_id } => (task_id, true),
    };
    if matches!(command, StageCommand::StageStop { .. }) {
        staging.lock().await.stop(task_id).await;
        return Some(StageTunnelFrame::EnvChanged {
            task_id,
            state: StageState::Stopped,
            routes: None,
            last_error: None,
        });
    }
    if restart {
        staging.lock().await.stop(task_id).await;
    }
    let project = match client.task_detail(task_id).await {
        Ok(task) => client
            .projects()
            .await
            .ok()
            .and_then(|projects| projects.into_iter().find(|p| p.id == task.project_id))
            .and_then(ProjectRepoConfig::from_dto),
        Err(error) => {
            tracing::warn!(task_id, %error, "stage command task lookup failed");
            None
        }
    };
    let Some(project) = project else {
        return Some(StageTunnelFrame::EnvChanged {
            task_id,
            state: StageState::Error,
            routes: None,
            last_error: Some("staging project is unavailable".to_string()),
        });
    };
    let result = staging.lock().await.start(task_id, &project).await;
    match result {
        Ok(()) => staging.lock().await.get(task_id).map(staging::env_changed_frame),
        Err(error) => Some(StageTunnelFrame::EnvChanged {
            task_id,
            state: StageState::Error,
            routes: None,
            last_error: Some(error),
        }),
    }
}

async fn touch_environment(staging: &StagingHandle, task_id: i32) -> bool {
    staging.lock().await.touch(task_id)
}

fn unsupported_ws(stream_id: String, reason: &str) -> StageTunnelFrame {
    StageTunnelFrame::WsClose {
        stream_id,
        code: 1008,
        reason: reason.to_string(),
    }
}

struct HttpTunnelRequest {
    stream_id: String,
    task_id: i32,
    route_path: String,
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body_base64: Option<String>,
}

async fn forward_http(staging: &StagingHandle, request: HttpTunnelRequest) -> Vec<StageTunnelFrame> {
    let HttpTunnelRequest {
        stream_id,
        task_id,
        route_path,
        method,
        path,
        query,
        headers,
        body_base64,
    } = request;
    let route = {
        let mut manager = staging.lock().await;
        let Some(environment) = manager.get(task_id) else {
            return error_response(stream_id, 404, "staging environment is not running");
        };
        if !matches!(environment.state, StageState::Running) {
            return error_response(stream_id, 503, "staging environment is not ready");
        }
        let route = staging::map_route(&environment.routes, &route_path).cloned();
        manager.touch(task_id);
        route
    };
    let Some(route) = route else {
        return error_response(stream_id, 404, "staging route not found");
    };
    let method = match reqwest::Method::from_bytes(method.as_bytes()) {
        Ok(method) => method,
        Err(_) => return error_response(stream_id, 400, "invalid HTTP method"),
    };
    let body = match body_base64 {
        Some(body) => match base64::engine::general_purpose::STANDARD.decode(body) {
            Ok(body) => body,
            Err(_) => return error_response(stream_id, 400, "invalid base64 request body"),
        },
        None => Vec::new(),
    };
    let target_path = local_request_path(&route.path, &path);
    let mut url = format!("http://127.0.0.1:{}{}", route.host_port, target_path);
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    let client = reqwest::Client::new();
    let mut request = client.request(method, url).timeout(LOCAL_REQUEST_TIMEOUT).body(body);
    for (name, value) in headers {
        if !is_hop_by_hop(&name) {
            request = request.header(name, value);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return error_response(stream_id, 502, &format!("staging request failed: {error}")),
    };
    let status = response.status().as_u16();
    let response_headers = filtered_headers(response.headers());
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return error_response(stream_id, 502, &format!("staging response failed: {error}")),
    };
    response_frames(stream_id, status, response_headers, &body)
}

fn local_request_path(route_path: &str, path: &str) -> String {
    if route_path == "/" {
        return if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
    }
    match path.strip_prefix(route_path) {
        Some("") | Some("/") => "/".to_string(),
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => path.to_string(),
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "transfer-encoding"
            | "upgrade"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
    )
}

fn filtered_headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            (!is_hop_by_hop(name.as_str()))
                .then(|| Some((name.to_string(), value.to_str().ok()?.to_string())))
                .flatten()
        })
        .collect()
}

fn response_frames(
    stream_id: String,
    status: u16,
    headers: HashMap<String, String>,
    body: &[u8],
) -> Vec<StageTunnelFrame> {
    if body.is_empty() {
        return vec![StageTunnelFrame::HttpResponse {
            stream_id,
            status,
            headers,
            body_base64: None,
            last_chunk: true,
        }];
    }
    body.chunks(MAX_TUNNEL_CHUNK)
        .enumerate()
        .map(|(index, chunk)| StageTunnelFrame::HttpResponse {
            stream_id: stream_id.clone(),
            status,
            headers: if index == 0 { headers.clone() } else { HashMap::new() },
            body_base64: Some(base64::engine::general_purpose::STANDARD.encode(chunk)),
            last_chunk: index == body.len().div_ceil(MAX_TUNNEL_CHUNK) - 1,
        })
        .collect()
}

fn error_response(stream_id: String, status: u16, message: &str) -> Vec<StageTunnelFrame> {
    response_frames(stream_id, status, HashMap::new(), message.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    /// Accepts one WS connection and runs `serve` on it. Returns the `ws://`
    /// URL the client should connect to.
    async fn spawn_ws_server<F, Fut>(serve: F) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            serve(ws).await;
        });
        format!("ws://{addr}")
    }

    fn empty_store() -> LogStore {
        let dir = std::env::temp_dir().join(format!("remoter-events-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        LogStore::new(&crate::config::LogsConfig {
            dir: Some(dir),
            retention_days: 1,
        })
        .unwrap()
    }

    #[test]
    fn ws_url_maps_http_schemes() {
        assert_eq!(
            ws_url("http://localhost:8181"),
            "ws://localhost:8181/api/v1/agent/events"
        );
        assert_eq!(ws_url("https://aiborda.ru"), "wss://aiborda.ru/api/v1/agent/events");
    }

    #[test]
    fn local_path_strips_selected_route_prefix() {
        assert_eq!(local_request_path("/api", "/api/v1/items"), "/v1/items");
        assert_eq!(local_request_path("/api", "/api"), "/");
        assert_eq!(local_request_path("/", "/health"), "/health");
        assert_eq!(local_request_path("/api", "/apix"), "/apix");
    }

    #[test]
    fn websocket_path_keeps_query_separate_from_route_prefix() {
        let (path, query) = "/api/socket?room=one".split_once('?').unwrap();
        assert_eq!(local_request_path("/api", path), "/socket");
        assert_eq!(query, "room=one");
    }

    #[test]
    fn hop_by_hop_headers_are_case_insensitive() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(!is_hop_by_hop("Content-Type"));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("connection", "close".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("x-stage", "yes".parse().unwrap());
        let filtered = filtered_headers(&headers);
        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("transfer-encoding"));
        assert_eq!(filtered.get("x-stage"), Some(&"yes".to_string()));
    }

    #[test]
    fn response_body_is_chunked_at_256_kib_and_empty_body_is_terminal() {
        let body = vec![b'x'; MAX_TUNNEL_CHUNK + 1];
        let frames = response_frames("stream".into(), 200, HashMap::new(), &body);
        assert_eq!(frames.len(), 2);
        let StageTunnelFrame::HttpResponse {
            body_base64: Some(first),
            last_chunk: false,
            ..
        } = &frames[0]
        else {
            panic!("expected non-terminal first chunk");
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(first).unwrap().len(),
            MAX_TUNNEL_CHUNK
        );
        let StageTunnelFrame::HttpResponse {
            body_base64: Some(last),
            last_chunk: true,
            ..
        } = &frames[1]
        else {
            panic!("expected terminal second chunk");
        };
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(last).unwrap().len(), 1);

        let empty = response_frames("empty".into(), 204, HashMap::new(), &[]);
        assert_eq!(empty.len(), 1);
        assert!(matches!(
            empty[0],
            StageTunnelFrame::HttpResponse {
                body_base64: None,
                last_chunk: true,
                ..
            }
        ));
    }

    #[test]
    fn unsupported_websocket_is_explicit_close() {
        assert_eq!(
            unsupported_ws("ws-1".into(), "unsupported"),
            StageTunnelFrame::WsClose {
                stream_id: "ws-1".into(),
                code: 1008,
                reason: "unsupported".into()
            }
        );
    }

    /// Accepts one plain TCP connection and keeps the socket open without
    /// completing the WebSocket handshake.
    async fn spawn_silent_tcp_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            futures::future::pending::<()>().await
        });
        format!("ws://{addr}")
    }

    #[test]
    fn invalid_request_body_is_explicit_error_response() {
        let frames = error_response("bad".into(), 400, "invalid base64 request body");
        assert!(matches!(
            frames.as_slice(),
            [StageTunnelFrame::HttpResponse {
                status: 400,
                last_chunk: true,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn silent_connection_is_dropped_by_the_idle_watchdog() {
        let url = spawn_ws_server(|ws| async move {
            // A half-open peer: the handshake completed, then silence forever.
            // `ws` must be held — dropping it would RST the socket and fail
            // the client with a transport error instead of the watchdog's.
            let _hold = ws;
            futures::future::pending::<()>().await;
        })
        .await;
        let (tx, _rx) = mpsc::channel(1);
        let err = listen_once(
            &url,
            "tok",
            None,
            &tx,
            empty_store(),
            None,
            None,
            Duration::from_millis(150),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no traffic"), "{err}");
    }

    #[tokio::test]
    async fn incoming_traffic_resets_the_idle_watchdog() {
        let url = spawn_ws_server(|mut ws| async move {
            // Ping faster than the watchdog: 6 × 40ms = 240ms of life against
            // a 150ms watchdog, then close cleanly. Without the reset this
            // ends in the watchdog error instead of `Ok`.
            for _ in 0..6 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
            let _ = ws.close(None).await;
            // Drain the client's pongs/close-reply until it disconnects —
            // dropping the socket with unread data would RST it and fail the
            // client with a transport error instead of a clean close.
            while ws.next().await.is_some() {}
        })
        .await;
        let (tx, _rx) = mpsc::channel(1);
        let result = listen_once(
            &url,
            "tok",
            None,
            &tx,
            empty_store(),
            None,
            None,
            Duration::from_millis(150),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn text_frame_forwards_a_wake_up() {
        let url = spawn_ws_server(|mut ws| async move {
            ws.send(Message::Text("{\"taskId\":1}".into())).await.unwrap();
            // Keep the connection open so the client observes the frame
            // before any close; the client side finishes via the watchdog.
            futures::future::pending::<()>().await;
        })
        .await;
        let (tx, mut rx) = mpsc::channel(1);
        let handle = tokio::spawn({
            let url = url.clone();
            async move {
                listen_once(
                    &url,
                    "tok",
                    None,
                    &tx,
                    empty_store(),
                    None,
                    None,
                    Duration::from_millis(150),
                    Duration::from_secs(5),
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap();
        // The watchdog then drops the idle connection with an error.
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("no traffic"), "{err}");
    }

    #[tokio::test]
    async fn silent_handshake_times_out_fast() {
        let url = spawn_silent_tcp_server().await;
        let (tx, _rx) = mpsc::channel(1);
        let start = tokio::time::Instant::now();
        let err = listen_once(
            &url,
            "tok",
            None,
            &tx,
            empty_store(),
            None,
            None,
            Duration::from_secs(60),
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("handshake timed out"),
            "expected timeout error, got {err}"
        );
        // Must fail well before the 60 s idle watchdog.
        assert!(elapsed < Duration::from_secs(2), "connect hung for {elapsed:?}");
    }
}
