use std::sync::Arc;

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State},
    response::IntoResponse,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{info, warn};

use crate::{parser, AppState};

/// WebSocket upgrade handler — one connection per session
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(session_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, session_id, state))
}

async fn handle_socket(mut socket: WebSocket, session_id: String, state: Arc<AppState>) {
    // Find the JSONL file
    let fp = match crate::sessions::find_session_file(&state.cfg.claude_dir, &session_id) {
        Some(p) => p,
        None => {
            let _ = socket.send(Message::Text(
                r#"{"event":"error","data":{"message":"session not found"}}"#.into()
            )).await;
            return;
        }
    };

    info!("WS connected: {session_id}");

    // Open file and read existing content first
    let mut file = match tokio::fs::File::open(&fp).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Cannot open {fp:?}: {e}");
            return;
        }
    };

    let mut partial = String::new();

    // ── Catch-up: stream all existing lines ────────────────────────────────
    let mut offset: u64;
    {
        let mut buf = String::new();
        let _ = file.read_to_string(&mut buf).await;
        offset = file.stream_position().await.unwrap_or(0);

        for line in buf.lines() {
            if let Some(evt) = parser::parse_line(line) {
                if let Ok(json) = serde_json::to_string(&evt) {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    // Send ready
    let _ = socket.send(Message::Text(
        serde_json::json!({ "event": "ready", "data": { "session_id": &session_id } })
            .to_string().into()
    )).await;

    // ── Live tail: poll every 1 second for new bytes ────────────────────────
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Check for new bytes
                match file.metadata().await {
                    Ok(m) if m.len() > offset => {
                        let mut buf = vec![0u8; (m.len() - offset) as usize];
                        if let Ok(_) = file.seek(tokio::io::SeekFrom::Start(offset)).await {
                            if let Ok(n) = file.read(&mut buf).await {
                                offset += n as u64;
                                partial.push_str(&String::from_utf8_lossy(&buf[..n]));

                                let mut lines: Vec<&str> = partial.lines().collect();
                                // If partial doesn't end with newline, last token is incomplete
                                let keep_last = !partial.ends_with('\n');
                                let last = if keep_last { lines.pop() } else { None };

                                for line in lines {
                                    if let Some(evt) = parser::parse_line(line) {
                                        if let Ok(json) = serde_json::to_string(&evt) {
                                            if socket.send(Message::Text(json.into())).await.is_err() {
                                                info!("WS disconnected: {session_id}");
                                                return;
                                            }
                                        }
                                    }
                                }

                                partial = last.map(|s| s.to_string()).unwrap_or_default();
                            }
                        }
                    }
                    Err(e) => {
                        warn!("stat error for {session_id}: {e}");
                        return;
                    }
                    _ => {} // no new bytes
                }
            }
            // Handle incoming pings / close frames
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WS closed: {session_id}");
                        return;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}
