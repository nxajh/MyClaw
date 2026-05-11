//! `myclaw tui` — launch the TUI client connected to the WebSocket server.

use anyhow::Result;

/// Default WebSocket URL.
const DEFAULT_WS_URL: &str = "ws://127.0.0.1:18789/myclaw";

pub async fn run(url: Option<&str>) -> Result<()> {
    let ws_url = url
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_WS_URL.to_string());

    tracing::info!("Starting TUI client, connecting to {ws_url}");

    let mut app = myclaw::tui::App::new(ws_url);
    app.run().await?;

    Ok(())
}
