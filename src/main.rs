mod api;
mod config;
mod parser;
mod sessions;
mod supabase;
mod ws;

use std::sync::Arc;

use axum::{routing::get, Router};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

pub struct AppState {
    pub cfg: config::Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "claude_progress=info,tower_http=warn".into()),
        )
        .init();

    let cfg = config::Config::from_env();
    let port = cfg.port;

    // Optional: Supabase background sync every 5 minutes
    if let Some(sb) = supabase::SupabaseClient::new(&cfg) {
        let cfg_clone = cfg.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                let sessions = sessions::list_sessions(&cfg_clone);
                if let Err(e) = sb.upsert_sessions(&sessions).await {
                    tracing::warn!("Supabase sync failed: {e}");
                } else {
                    info!("Supabase: synced {} sessions", sessions.len());
                }
            }
        });
    }

    let state = Arc::new(AppState { cfg });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // REST
        .route("/api/health",              get(api::health))
        .route("/api/sessions",            get(api::list_sessions))
        .route("/api/sessions/:id",        get(api::get_session))
        .route("/api/sessions/:id/input",  axum::routing::post(api::session_input))
        // WebSocket  →  ws://localhost:PORT/ws/<session_id>
        .route("/ws/:session_id",          get(ws::ws_handler))
        .with_state(state)
        .layer(cors);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("claude-progress  http://{addr}");
    info!("  WS endpoint:   ws://{addr}/ws/<session_id>");
    info!("  Sessions API:  http://{addr}/api/sessions");

    axum::serve(listener, app).await?;
    Ok(())
}
