use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use tokio::sync::watch;

use crate::config::Config;
use crate::player::{MediaPlayer, PlaybackState};
use crate::state::{SharedState, TransportState, extract_title_from_didl, format_time, parse_time};
use crate::xml;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub local_ip: IpAddr,
    pub http_port: u16,
    pub av_state: SharedState,
    pub player: Arc<std::sync::Mutex<Box<dyn MediaPlayer>>>,
    pub subscriptions: Arc<RwLock<HashMap<String, Subscription>>>,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    #[allow(dead_code)]
    pub sid: String,
    pub callback: String, // e.g. <http://192.168.1.10:1234/notify>
    #[allow(dead_code)]
    pub nt: String,
    pub timeout_secs: u64,
    pub seq: u32,
}

fn transport_state_for_player(player_state: PlaybackState, has_media: bool) -> TransportState {
    if !has_media {
        return TransportState::NoMediaPresent;
    }
    match player_state {
        PlaybackState::Playing => TransportState::Playing,
        PlaybackState::Paused => TransportState::PausedPlayback,
        PlaybackState::Stopped => TransportState::Stopped,
        PlaybackState::Transitioning => TransportState::Transitioning,
    }
}

pub async fn run_http_server(
    config: Config,
    local_ip: IpAddr,
    av_state: SharedState,
    player: Arc<std::sync::Mutex<Box<dyn MediaPlayer>>>,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<(SocketAddr, watch::Sender<bool>)> {
    let subscriptions: Arc<RwLock<HashMap<String, Subscription>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Bind to 0.0.0.0:0 (ephemeral) to avoid port conflicts, as per DLNA spec ephemeral is fine
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
    let addr = listener.local_addr()?;
    let http_port = addr.port();
    tracing::info!(
        "HTTP server listening on {} (ephemeral {})",
        addr,
        http_port
    );

    let (tx, mut rx) = watch::channel(false);
    let shutdown_tx = tx.clone();

    let app_state_with_port = AppState {
        config: config.clone(),
        local_ip,
        http_port,
        av_state: av_state.clone(),
        player: player.clone(),
        subscriptions: subscriptions.clone(),
    };
    // Keep a clone for the polling task so external VLC state changes can
    // generate normal DLNA LastChange events.
    let poll_app_state = app_state_with_port.clone();
    let app2 = Router::new()
        .route("/", get(root_handler))
        .route("/description.xml", get(description_handler))
        .route("/AVTransport/scpd.xml", get(av_scpd_handler))
        .route("/RenderingControl/scpd.xml", get(rc_scpd_handler))
        .route("/ConnectionManager/scpd.xml", get(cm_scpd_handler))
        .route("/AVTransport/control", post(control_handler))
        .route("/AVTransport/event", any(event_handler))
        .route("/RenderingControl/control", post(control_handler))
        .route("/RenderingControl/event", any(event_handler))
        .route("/ConnectionManager/control", post(control_handler))
        .route("/ConnectionManager/event", any(event_handler))
        .route("/api/play", get(api_play))
        .route("/api/pause", get(api_pause))
        .route("/api/stop", get(api_stop))
        .route("/api/peer", get(api_add_peer))
        .with_state(app_state_with_port);

    tokio::spawn(async move {
        axum::serve(listener, app2)
            .with_graceful_shutdown(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                    if *rx.borrow() {
                        break;
                    }
                }
                tracing::info!("HTTP server shutting down");
            })
            .await
            .unwrap();
    });

    // Poll VLC's authoritative state and clock with adaptive intervals
    // and offloaded to spawn_blocking to avoid blocking Tokio worker threads.
    let av_state_clone = av_state.clone();
    let player_clone = player.clone();
    tokio::spawn(async move {
        let mut last_player_state = PlaybackState::Stopped;
        loop {
            // Adaptive polling interval:
            // - Playing / Transitioning: 500ms for smooth timeline & buffer tracking
            // - Paused: 1000ms
            // - Stopped / Idle: 2500ms to save CPU & battery
            let sleep_duration = match last_player_state {
                PlaybackState::Playing | PlaybackState::Transitioning => {
                    std::time::Duration::from_millis(500)
                }
                PlaybackState::Paused => std::time::Duration::from_millis(1000),
                PlaybackState::Stopped => std::time::Duration::from_millis(2500),
            };
            tokio::time::sleep(sleep_duration).await;

            let player_c = player_clone.clone();
            let (pos, dur, player_state) = tokio::task::spawn_blocking(move || {
                if let Ok(mut p) = player_c.lock() {
                    p.poll_sync();
                    let (pos, dur) = p.get_position();
                    (pos, dur, p.get_state())
                } else {
                    (
                        std::time::Duration::ZERO,
                        std::time::Duration::ZERO,
                        PlaybackState::Stopped,
                    )
                }
            })
            .await
            .unwrap_or((
                std::time::Duration::ZERO,
                std::time::Duration::ZERO,
                PlaybackState::Stopped,
            ));

            last_player_state = player_state;

            let state_changed = {
                let mut state = av_state_clone.write().unwrap();
                let mapped =
                    transport_state_for_player(player_state, !state.current_uri.is_empty());
                let changed = state.transport_state != mapped;
                state.position = pos;
                if dur > std::time::Duration::ZERO {
                    state.duration = dur;
                }
                if changed {
                    state.transport_state = mapped;
                    state.last_change_seq = state.last_change_seq.wrapping_add(1);
                }
                state.last_updated = std::time::Instant::now();
                changed
            };

            if state_changed {
                notify_avtransport(&poll_app_state).await;
            }
        }
    });

    Ok((addr, shutdown_tx))
}

async fn root_handler(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let av = state.av_state.read().unwrap().clone();
    let endpoints = crate::config::get_candidate_endpoints();
    let peers = crate::config::get_tailscale_peers();

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut ep_html = String::new();
    for ep in &endpoints {
        let tag = if ep.is_tailscale {
            "<span style=\"color:#00c853;\">[Tailscale]</span>"
        } else if ep.is_primary {
            "<span style=\"color:#2979ff;\">[Primary/Hotspot]</span>"
        } else {
            ""
        };
        ep_html.push_str(&format!(
            "<li><strong>{}</strong>: <code>http://{}:{}/description.xml</code> {}</li>",
            ep.name, ep.ip, state.http_port, tag
        ));
    }

    let mut peers_html = String::new();
    if peers.is_empty() {
        peers_html.push_str("<li><em>No Tailscale peers detected yet.</em></li>");
    } else {
        for p in &peers {
            peers_html.push_str(&format!(
                "<li><code>{}</code> (notified via unicast)</li>",
                p
            ));
        }
    }

    let track_title = if av.title.is_empty() {
        "No media playing"
    } else {
        &av.title
    };
    let time_str = format!("{}/{}", format_time(av.position), format_time(av.duration));

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{friendly} - DLNA Renderer</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; margin: 0; padding: 24px; background: #121212; color: #e0e0e0; }}
        .card {{ background: #1e1e1e; border-radius: 12px; padding: 20px; margin-bottom: 20px; box-shadow: 0 4px 12px rgba(0,0,0,0.4); max-width: 650px; }}
        h1 {{ margin-top: 0; color: #4fc3f7; font-size: 24px; }}
        h2 {{ color: #b0bec5; font-size: 18px; border-bottom: 1px solid #333; padding-bottom: 6px; }}
        .btn {{ display: inline-block; background: #0288d1; color: white; border: none; padding: 10px 18px; border-radius: 6px; margin: 4px; font-weight: bold; text-decoration: none; cursor: pointer; }}
        .btn:hover {{ background: #039be5; }}
        .btn-stop {{ background: #d32f2f; }}
        .btn-stop:hover {{ background: #e53935; }}
        ul {{ padding-left: 20px; line-height: 1.6; }}
        code {{ background: #263238; padding: 2px 6px; border-radius: 4px; color: #80d8ff; font-size: 13px; }}
        .status-badge {{ display: inline-block; padding: 4px 10px; border-radius: 12px; font-size: 13px; font-weight: bold; background: #37474f; color: #eceff1; }}
    </style>
</head>
<body>
    <div class="card">
        <h1>{friendly}</h1>
        <p><span class="status-badge">{state_str}</span> <span style="margin-left:10px;">{time_str}</span></p>
        <p><strong>Track:</strong> {track_title}</p>
        <p><strong>Volume:</strong> {vol}%</p>
        <div style="margin-top: 15px;">
            <a href="/api/play" class="btn">&#9654; Play</a>
            <a href="/api/pause" class="btn">&#10074;&#10074; Pause</a>
            <a href="/api/stop" class="btn btn-stop">&#9632; Stop</a>
        </div>
    </div>

    <div class="card">
        <h2>Network Endpoints (Reachable by Phone)</h2>
        <p>BubbleUPnP or any DLNA controller can access this renderer at:</p>
        <ul>{ep_html}</ul>
        <p style="font-size: 12px; color: #90a4ae;">Current request host: <code>{host}</code></p>
    </div>

    <div class="card">
        <h2>Tailscale & Hotspot Peers</h2>
        <ul>{peers_html}</ul>
        <form action="/api/peer" method="get" style="margin-top: 10px;">
            <input type="text" name="ip" placeholder="e.g. 100.80.1.2 or 192.168.43.1" style="padding: 8px; border-radius: 4px; border: 1px solid #455a64; background: #263238; color: white; width: 220px;">
            <button type="submit" class="btn" style="padding: 8px 14px;">Add & Notify Peer</button>
        </form>
    </div>
</body>
</html>"#,
        friendly = state.config.friendly_name,
        state_str = av.transport_state.as_str(),
        time_str = time_str,
        track_title = track_title,
        vol = av.volume,
        ep_html = ep_html,
        peers_html = peers_html,
        host = host,
    );

    (
        StatusCode::OK,
        [("Content-Type", "text/html; charset=utf-8")],
        html,
    )
}

async fn description_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let base_url = if !host.is_empty() {
        format!("http://{}/", host)
    } else {
        format!("http://{}:{}/", state.local_ip, state.http_port)
    };

    let xml = xml::device_description(&state.config, &base_url);
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml,
    )
}

async fn api_play(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(mut p) = state.player.lock() {
        let _ = p.play();
    }
    axum::response::Redirect::to("/")
}

async fn api_pause(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(mut p) = state.player.lock() {
        let _ = p.pause();
    }
    axum::response::Redirect::to("/")
}

async fn api_stop(State(state): State<AppState>) -> impl IntoResponse {
    if let Ok(mut p) = state.player.lock() {
        let _ = p.stop();
    }
    axum::response::Redirect::to("/")
}

#[derive(serde::Deserialize)]
pub struct PeerParam {
    pub ip: Option<String>,
}

async fn api_add_peer(
    axum::extract::Query(param): axum::extract::Query<PeerParam>,
) -> impl IntoResponse {
    if let Some(ip_str) = param.ip {
        if let Ok(ip) = ip_str.trim().parse::<std::net::Ipv4Addr>() {
            crate::config::register_peer(ip);
        }
    }
    axum::response::Redirect::to("/")
}

async fn av_scpd_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml::avtransport_scpd(),
    )
}
async fn rc_scpd_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml::rendering_control_scpd(),
    )
}
async fn cm_scpd_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml::connection_manager_scpd(),
    )
}

async fn event_handler(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let uri = req.uri().path().to_string();
    tracing::debug!("GENA {} {} headers={:?}", method, uri, headers);

    // We need to handle SUBSCRIBE, UNSUBSCRIBE
    // Axum's Method may not have SUBSCRIBE constant, so we check string
    let method_str = method.as_str();
    if method_str == "SUBSCRIBE" {
        return handle_subscribe(state, headers, uri).await;
    } else if method_str == "UNSUBSCRIBE" {
        return handle_unsubscribe(state, headers).await;
    }
    // For other methods, return 400
    (StatusCode::BAD_REQUEST, "Unsupported method").into_response()
}

async fn handle_subscribe(state: AppState, headers: HeaderMap, uri: String) -> Response {
    let callback = headers
        .get("callback")
        .or_else(|| headers.get("CALLBACK"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let nt = headers
        .get("nt")
        .or_else(|| headers.get("NT"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("upnp:event")
        .to_string();
    let timeout_str = headers
        .get("timeout")
        .or_else(|| headers.get("TIMEOUT"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Second-1800")
        .to_string();
    let sid_in = headers
        .get("sid")
        .or_else(|| headers.get("SID"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let timeout_secs = parse_timeout(&timeout_str);

    // If SID present, it's renewal
    if let Some(sid) = sid_in {
        let mut subs = state.subscriptions.write().unwrap();
        if let Some(sub) = subs.get_mut(&sid) {
            sub.timeout_secs = timeout_secs;
            tracing::info!("GENA renewal SID={} timeout={}", sid, timeout_secs);
            return (
                StatusCode::OK,
                [
                    ("SID", sid.clone()),
                    ("TIMEOUT", format!("Second-{}", timeout_secs)),
                ],
                "",
            )
                .into_response();
        } else {
            return (StatusCode::PRECONDITION_FAILED, "Invalid SID").into_response();
        }
    }

    // New subscription
    if callback.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing CALLBACK").into_response();
    }
    // CALLBACK is like <http://192.168.1.10:1234/notify>
    let callback_clean = callback
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string();
    let sid = format!("uuid:{}", uuid::Uuid::new_v4());
    let sub = Subscription {
        sid: sid.clone(),
        callback: callback_clean.clone(),
        nt: nt.clone(),
        timeout_secs,
        seq: 0,
    };
    {
        let mut subs = state.subscriptions.write().unwrap();
        subs.insert(sid.clone(), sub);
    }
    tracing::info!(
        "GENA SUBSCRIBE {} -> SID {} CALLBACK {}",
        uri,
        sid,
        callback_clean
    );

    // Immediately send initial event
    let av_state = state.av_state.read().unwrap().clone();
    let body = if uri.contains("AVTransport") {
        let last_change = xml::avtransport_last_change(&av_state);
        format!(
            r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
            last_change
        )
    } else if uri.contains("RenderingControl") {
        let last_change = xml::rendering_last_change(&av_state);
        format!(
            r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
            last_change
        )
    } else {
        r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><SinkProtocolInfo>http-get:*:*:*</SinkProtocolInfo></e:property></e:propertyset>"#.to_string()
    };

    // Send NOTIFY asynchronously
    let subs_clone = state.subscriptions.clone();
    let sid_clone = sid.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        send_notify(&subs_clone, &sid_clone, &body).await;
    });

    (
        StatusCode::OK,
        [
            ("SID", sid.clone()),
            ("TIMEOUT", format!("Second-{}", timeout_secs)),
            ("Content-Type", "text/xml; charset=\"utf-8\"".to_string()),
        ],
        "",
    )
        .into_response()
}

async fn handle_unsubscribe(state: AppState, headers: HeaderMap) -> Response {
    let sid = headers
        .get("sid")
        .or_else(|| headers.get("SID"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if sid.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing SID").into_response();
    }
    let mut subs = state.subscriptions.write().unwrap();
    if subs.remove(&sid).is_some() {
        tracing::info!("GENA UNSUBSCRIBE SID={}", sid);
        (StatusCode::OK, "").into_response()
    } else {
        (StatusCode::PRECONDITION_FAILED, "Invalid SID").into_response()
    }
}

fn parse_timeout(s: &str) -> u64 {
    // Second-1800 or Second-infinite
    if s.to_lowercase().contains("infinite") {
        return 1800;
    }
    if let Some(dash) = s.find('-') {
        if let Ok(v) = s[dash + 1..].parse::<u64>() {
            return v;
        }
    }
    1800
}

async fn send_notify(subs: &Arc<RwLock<HashMap<String, Subscription>>>, sid: &str, body: &str) {
    let (callback, seq) = {
        let mut guard = subs.write().unwrap();
        if let Some(sub) = guard.get_mut(sid) {
            let cb = sub.callback.clone();
            let seq = sub.seq;
            sub.seq += 1;
            (cb, seq)
        } else {
            return;
        }
    };
    tracing::debug!(
        "GENA NOTIFY to {} SID {} SEQ {} body len {}",
        callback,
        sid,
        seq,
        body.len()
    );
    // Use reqwest to send NOTIFY (custom method)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    if let Ok(client) = client {
        // callback may be multiple URLs comma separated? For now take first.
        let url = callback
            .split(',')
            .next()
            .unwrap_or(&callback)
            .trim()
            .trim_matches('<')
            .trim_matches('>')
            .trim();
        // reqwest doesn't have NOTIFY method constant, use Method::from_bytes
        let method = reqwest::Method::from_bytes(b"NOTIFY").unwrap_or(reqwest::Method::POST);
        let res = client
            .request(method, url)
            .header(
                "HOST",
                url.replace("http://", "").split('/').next().unwrap_or(""),
            )
            .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
            .header("NT", "upnp:event")
            .header("NTS", "upnp:propchange")
            .header("SID", sid)
            .header("SEQ", seq.to_string())
            .body(body.to_string())
            .send()
            .await;
        match res {
            Ok(r) => tracing::debug!("NOTIFY response status {}", r.status()),
            Err(e) => tracing::warn!("NOTIFY failed to {}: {}", url, e),
        }
    }
}

// Helper to notify all AVTransport subscribers
pub async fn notify_avtransport(state: &AppState) {
    let av_state = state.av_state.read().unwrap().clone();
    let body_inner = xml::avtransport_last_change(&av_state);
    let body = format!(
        r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
        body_inner
    );
    let subs = state.subscriptions.read().unwrap().clone();
    for (sid, sub) in subs.iter() {
        if sub.callback.is_empty() {
            continue;
        }
        // Only notify AVTransport subs - heuristic: check if callback was from AVTransport? We don't track uri per sub.
        // For MVP, notify all (RenderingControl subs will also get AVTransport LastChange but BubbleUPnP tolerates)
        let sid_clone = sid.clone();
        let cb = sub.callback.clone();
        let body_clone = body.clone();
        let subs_clone = state.subscriptions.clone();
        tokio::spawn(async move {
            // inline send
            let seq = {
                let mut g = subs_clone.write().unwrap();
                if let Some(s) = g.get_mut(&sid_clone) {
                    let sseq = s.seq;
                    s.seq += 1;
                    sseq
                } else {
                    0
                }
            };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();
            let url = cb
                .split(',')
                .next()
                .unwrap_or(&cb)
                .trim()
                .trim_matches('<')
                .trim_matches('>')
                .trim();
            let method = reqwest::Method::from_bytes(b"NOTIFY").unwrap();
            let _ = client
                .request(method, url)
                .header("NT", "upnp:event")
                .header("NTS", "upnp:propchange")
                .header("SID", sid_clone.clone())
                .header("SEQ", seq.to_string())
                .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
                .body(body_clone)
                .send()
                .await;
        });
    }
}

pub async fn notify_rendering(state: &AppState) {
    let av_state = state.av_state.read().unwrap().clone();
    let body_inner = xml::rendering_last_change(&av_state);
    let body = format!(
        r#"<?xml version="1.0"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>{}</LastChange></e:property></e:propertyset>"#,
        body_inner
    );
    let subs = state.subscriptions.read().unwrap().clone();
    for (sid, sub) in subs.iter() {
        let sid_clone = sid.clone();
        let cb = sub.callback.clone();
        let body_clone = body.clone();
        let subs_clone = state.subscriptions.clone();
        tokio::spawn(async move {
            let seq = {
                let mut g = subs_clone.write().unwrap();
                if let Some(s) = g.get_mut(&sid_clone) {
                    let sseq = s.seq;
                    s.seq += 1;
                    sseq
                } else {
                    0
                }
            };
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();
            let url = cb.split(',').next().unwrap_or(&cb).trim();
            let method = reqwest::Method::from_bytes(b"NOTIFY").unwrap();
            let _ = client
                .request(method, url)
                .header("NT", "upnp:event")
                .header("NTS", "upnp:propchange")
                .header("SID", sid_clone)
                .header("SEQ", seq.to_string())
                .header("CONTENT-TYPE", "text/xml; charset=\"utf-8\"")
                .body(body_clone)
                .send()
                .await;
        });
    }
}

async fn control_handler(State(state): State<AppState>, req: Request) -> Response {
    let headers = req.headers().clone();
    let uri_path = req.uri().path().to_string();
    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Failed to read control body: {}", e);
            return (StatusCode::BAD_REQUEST, "Invalid body").into_response();
        }
    };
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();
    // Extract SOAPAction header (case-insensitive)
    let soap_action_header = headers
        .get("soapaction")
        .or_else(|| headers.get("SOAPAction"))
        .or_else(|| headers.get("SoapAction"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Also try lower-case via iteration
    let soap_action = if soap_action_header.is_empty() {
        headers
            .iter()
            .find(|(k, _)| k.as_str().eq_ignore_ascii_case("soapaction"))
            .and_then(|(_, v)| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    } else {
        soap_action_header
    };
    let action = xml::parse_soap_action(&soap_action).unwrap_or_else(|| {
        // fallback: try to extract from body <u:Play> etc.
        // find first <u:Action
        if let Some(start) = body_str.find("<u:") {
            let rest = &body_str[start + 3..];
            if let Some(end) = rest.find(|c: char| c == ' ' || c == '>' || c == '/') {
                rest[..end].to_string()
            } else {
                "".to_string()
            }
        } else {
            "".to_string()
        }
    });
    tracing::info!(
        "SOAP {} {} action={} body_len={}",
        uri_path,
        soap_action,
        action,
        body_str.len()
    );
    tracing::debug!("SOAP body: {}", body_str);

    // Route based on uri and action
    let result = if uri_path.contains("AVTransport") {
        handle_avtransport(&state, &action, &body_str).await
    } else if uri_path.contains("RenderingControl") {
        handle_rendering(&state, &action, &body_str).await
    } else if uri_path.contains("ConnectionManager") {
        handle_connection_manager(&action, &body_str).await
    } else {
        Err((500, "Unknown service".to_string()))
    };

    match result {
        Ok(inner_xml) => {
            // Determine service urn for response
            let service = if uri_path.contains("AVTransport") {
                "urn:schemas-upnp-org:service:AVTransport:1"
            } else if uri_path.contains("RenderingControl") {
                "urn:schemas-upnp-org:service:RenderingControl:1"
            } else {
                "urn:schemas-upnp-org:service:ConnectionManager:1"
            };
            let soap_resp = xml::soap_response(&action, service, &inner_xml);
            (
                StatusCode::OK,
                [("Content-Type", "text/xml; charset=\"utf-8\""), ("EXT", "")],
                soap_resp,
            )
                .into_response()
        }
        Err((code, msg)) => {
            let fault = xml::soap_fault(code, &msg);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("Content-Type", "text/xml; charset=\"utf-8\"")],
                fault,
            )
                .into_response()
        }
    }
}

async fn handle_avtransport(
    state: &AppState,
    action: &str,
    body: &str,
) -> Result<String, (u32, String)> {
    match action {
        "SetAVTransportURI" => {
            let uri = xml::extract_tag(body, "CurrentURI").unwrap_or_default();
            let metadata = xml::extract_tag(body, "CurrentURIMetaData").unwrap_or_default();
            let mut title = extract_title_from_didl(&metadata);
            let artist = crate::state::extract_artist_from_didl(&metadata);
            if !artist.is_empty() {
                if !title.is_empty() {
                    title = format!("{} - {}", artist, title);
                } else {
                    title = artist;
                }
            }
            if title.is_empty() {
                if let Some(pos) = uri.rfind('/') {
                    title = uri[pos + 1..].split('?').next().unwrap_or("Media").to_string();
                }
            }
            tracing::info!("SetAVTransportURI uri={} title={} metadata_len={}", uri, title, metadata.len());
            if uri.is_empty() {
                return Err((402, "Invalid Args".into()));
            }
            let duration_from_didl = crate::state::extract_duration_from_didl(&metadata);
            {
                let mut av = state.av_state.write().unwrap();
                av.current_uri = uri.clone();
                av.current_metadata = metadata.clone();
                av.title = title.clone();
                av.transport_state = TransportState::Stopped;
                av.position = std::time::Duration::from_secs(0);
                // A new URI must not inherit the previous track's length.
                av.duration = duration_from_didl.unwrap_or(std::time::Duration::ZERO);
                av.track = 0;
                av.last_change_seq = av.last_change_seq.wrapping_add(1);
                av.last_updated = std::time::Instant::now();
            }
            {
                let mut player = state.player.lock().unwrap();
                player.set_uri(uri.clone(), title.clone());
            }
            notify_avtransport(state).await;
            Ok("".to_string())
        }
        "SetNextAVTransportURI" => {
            // Not supported, just ack
            Ok("".to_string())
        }
        "GetMediaInfo" => {
            let av = state.av_state.read().unwrap().clone();
            let nr_tracks = if av.current_uri.is_empty() { "0" } else { "1" };
            let media_duration = if av.duration.as_secs() == 0 { "00:00:00".to_string() } else { format_time(av.duration) };
            let inner = format!(
                "<NrTracks>{}</NrTracks><MediaDuration>{}</MediaDuration><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData><NextURI></NextURI><NextURIMetaData></NextURIMetaData><PlayMedium>NETWORK</PlayMedium><RecordMedium>NOT_IMPLEMENTED</RecordMedium><WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                nr_tracks,
                media_duration,
                xml_escape(&av.current_uri),
                xml_escape(&av.current_metadata)
            );
            Ok(inner)
        }
        "GetTransportInfo" => {
            let av = state.av_state.read().unwrap();
            let inner = format!(
                "<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                av.transport_state.as_str()
            );
            Ok(inner)
        }
        "GetPositionInfo" => {
            let av = state.av_state.read().unwrap().clone();
            // Read from AVState directly with smooth subsecond interpolation
            // without acquiring player.lock(), completely eliminating lock contention with control actions.
            let pos = if av.transport_state == TransportState::Playing {
                let subsecond = av.last_updated.elapsed().min(std::time::Duration::from_millis(950));
                av.position + subsecond
            } else {
                av.position
            };
            let dur = av.duration;
            let effective_duration = dur;
            let effective_position = if effective_duration > std::time::Duration::ZERO {
                pos.min(effective_duration)
            } else {
                pos
            };
            let track_duration = if effective_duration.is_zero() {
                "00:00:00".to_string()
            } else {
                format_time(effective_duration)
            };
            let rel_time = format_time(effective_position);
            let track = if av.current_uri.is_empty() { "0" } else { "1" };
            let uri = av.current_uri.clone();
            let metadata = av.current_metadata.clone();
            let inner = format!(
                "<Track>{}</Track><TrackDuration>{}</TrackDuration><TrackMetaData>{}</TrackMetaData><TrackURI>{}</TrackURI><RelTime>{}</RelTime><AbsTime>{}</AbsTime><RelCount>2147483647</RelCount><AbsCount>2147483647</AbsCount>",
                track,
                track_duration,
                xml_escape(&metadata),
                xml_escape(&uri),
                rel_time,
                rel_time
            );
            Ok(inner)
        }
        "GetDeviceCapabilities" => {
            Ok("<PlayMedia>NETWORK</PlayMedia><RecMedia>NOT_IMPLEMENTED</RecMedia><RecQualityModes>NOT_IMPLEMENTED</RecQualityModes>".to_string())
        }
        "GetTransportSettings" => {
            Ok("<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>".to_string())
        }
        "Stop" => {
            tracing::info!("AVTransport Stop");
            let (player_state, position, duration) = {
                let mut p = state
                    .player
                    .lock()
                    .map_err(|_| (501, "VLC player unavailable".to_string()))?;
                p.stop()
                    .map_err(|e| (501, format!("VLC stop failed: {}", e)))?;
                let (position, duration) = p.get_position();
                (p.get_state(), position, duration)
            };
            {
                let mut av = state.av_state.write().unwrap();
                av.transport_state = transport_state_for_player(player_state, !av.current_uri.is_empty());
                av.position = position;
                if duration > std::time::Duration::ZERO {
                    av.duration = duration;
                }
                av.last_change_seq = av.last_change_seq.wrapping_add(1);
                av.last_updated = std::time::Instant::now();
            }
            notify_avtransport(state).await;
            Ok("".to_string())
        }
        "Play" => {
            tracing::info!("AVTransport Play");
            if state.av_state.read().unwrap().current_uri.is_empty() {
                return Err((701, "Transition not available".into()));
            }
            let (player_state, position, duration) = {
                let mut p = state
                    .player
                    .lock()
                    .map_err(|_| (501, "VLC player unavailable".to_string()))?;
                p.play()
                    .map_err(|e| (501, format!("VLC play failed: {}", e)))?;
                let (position, duration) = p.get_position();
                (p.get_state(), position, duration)
            };
            {
                let mut av = state.av_state.write().unwrap();
                av.transport_state = transport_state_for_player(player_state, true);
                av.position = position;
                if duration > std::time::Duration::ZERO {
                    av.duration = duration;
                }
                av.last_change_seq = av.last_change_seq.wrapping_add(1);
                av.last_updated = std::time::Instant::now();
            }
            notify_avtransport(state).await;
            Ok("".to_string())
        }
        "Pause" => {
            tracing::info!("AVTransport Pause");
            let (player_state, position, duration) = {
                let mut p = state
                    .player
                    .lock()
                    .map_err(|_| (501, "VLC player unavailable".to_string()))?;
                p.pause()
                    .map_err(|e| (501, format!("VLC pause failed: {}", e)))?;
                let (position, duration) = p.get_position();
                (p.get_state(), position, duration)
            };
            {
                let mut av = state.av_state.write().unwrap();
                av.transport_state = transport_state_for_player(player_state, !av.current_uri.is_empty());
                av.position = position;
                if duration > std::time::Duration::ZERO {
                    av.duration = duration;
                }
                av.last_change_seq = av.last_change_seq.wrapping_add(1);
                av.last_updated = std::time::Instant::now();
            }
            notify_avtransport(state).await;
            Ok("".to_string())
        }
        "Seek" => {
            let unit = xml::extract_tag(body, "Unit").unwrap_or_default();
            let target = xml::extract_tag(body, "Target").unwrap_or_default();
            tracing::info!("AVTransport Seek unit={} target={}", unit, target);
            if !unit.eq_ignore_ascii_case("REL_TIME") && !unit.eq_ignore_ascii_case("ABS_TIME") {
                return Err((402, "Invalid Args".into()));
            }
            let dur = parse_time(&target).ok_or((402, "Invalid Args".into()))?;
            let (position, duration) = {
                let mut p = state
                    .player
                    .lock()
                    .map_err(|_| (501, "VLC player unavailable".to_string()))?;
                p.seek(dur)
                    .map_err(|e| (501, format!("VLC seek failed: {}", e)))?;
                p.get_position()
            };
            {
                let mut av = state.av_state.write().unwrap();
                av.position = position;
                if duration > std::time::Duration::ZERO {
                    av.duration = duration;
                }
                av.last_change_seq = av.last_change_seq.wrapping_add(1);
                av.last_updated = std::time::Instant::now();
            }
            notify_avtransport(state).await;
            Ok("".to_string())
        }
        "Next" | "Previous" => {
            // Not supported in our single-track model, just ack
            Ok("".to_string())
        }
        "SetPlayMode" => Ok("".to_string()),
        "GetCurrentTransportActions" => {
            let av = state.av_state.read().unwrap();
            let actions = match av.transport_state {
                TransportState::Playing => "Play,Stop,Pause,Seek",
                TransportState::PausedPlayback => "Play,Stop,Seek",
                TransportState::Stopped => "Play",
                TransportState::Transitioning => "Stop",
                TransportState::NoMediaPresent => "",
            };
            Ok(format!("<Actions>{}</Actions>", actions))
        }
        other => {
            tracing::warn!("Unknown AVTransport action: {}", other);
            Err((401, format!("Invalid Action {}", other)))
        }
    }
}

async fn handle_rendering(
    state: &AppState,
    action: &str,
    body: &str,
) -> Result<String, (u32, String)> {
    match action {
        "GetVolume" => {
            let av = state.av_state.read().unwrap();
            Ok(format!("<CurrentVolume>{}</CurrentVolume>", av.volume))
        }
        "SetVolume" => {
            let vol_str = xml::extract_tag(body, "DesiredVolume")
                .unwrap_or_else(|| xml::extract_tag(body, "DesiredVolume").unwrap_or_default());
            let vol: u8 = vol_str.parse().unwrap_or(50);
            let vol = vol.min(100);
            {
                let mut av = state.av_state.write().unwrap();
                av.volume = vol;
            }
            if let Ok(mut p) = state.player.lock() {
                let _ = p.set_volume(vol);
            }
            notify_rendering(state).await;
            Ok("".to_string())
        }
        "GetMute" => {
            let av = state.av_state.read().unwrap();
            Ok(format!(
                "<CurrentMute>{}</CurrentMute>",
                if av.mute { "1" } else { "0" }
            ))
        }
        "SetMute" => {
            let mute_str = xml::extract_tag(body, "DesiredMute").unwrap_or_default();
            let mute = mute_str == "1" || mute_str.to_lowercase() == "true";
            {
                let mut av = state.av_state.write().unwrap();
                av.mute = mute;
            }
            if let Ok(mut p) = state.player.lock() {
                let _ = p.set_mute(mute);
            }
            notify_rendering(state).await;
            Ok("".to_string())
        }
        "ListPresets" | "SelectPreset" => {
            Ok("<CurrentPresetNameList></CurrentPresetNameList>".to_string())
        }
        other => Err((401, format!("Invalid Action {}", other))),
    }
}

async fn handle_connection_manager(action: &str, _body: &str) -> Result<String, (u32, String)> {
    match action {
        "GetProtocolInfo" => {
            // Must advertise sink we support else BubbleUPnP greys out
            let sink = "http-get:*:video/mp4:*,http-get:*:video/x-matroska:*,http-get:*:video/avi:*,http-get:*:audio/mpeg:*,http-get:*:audio/mp3:*,http-get:*:audio/flac:*,http-get:*:image/jpeg:*,http-get:*:*:*";
            Ok(format!("<Source></Source><Sink>{}</Sink>", sink))
        }
        "GetCurrentConnectionIDs" => Ok("<ConnectionIDs>0</ConnectionIDs>".to_string()),
        "GetCurrentConnectionInfo" => Ok("<RcsID>0</RcsID><AVTransportID>0</AVTransportID><ProtocolInfo>http-get:*:*:*</ProtocolInfo><PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID><Direction>Input</Direction><Status>OK</Status>".to_string()),
        other => Err((401, format!("Invalid Action {}", other))),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
