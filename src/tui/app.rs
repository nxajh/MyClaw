//! Main TUI application — full-screen terminal UI connecting to MyClaw WebSocket server.

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use std::io;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

// ── WebSocket protocol types ──────────────────────────────────────────────────

/// Server → Client message.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type")]
enum ServerMsg {
    #[serde(rename = "chunk")]
    Chunk { delta: String },
    #[serde(rename = "thinking")]
    Thinking { delta: String },
    #[serde(rename = "tool_call")]
    ToolCall { name: String, args: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult { name: String, output: String },
    #[serde(rename = "done")]
    Done { text: String },
    #[serde(rename = "cancelled")]
    Cancelled { partial: String },
    #[serde(rename = "error")]
    Error { message: String },
}

// ── Internal event types ──────────────────────────────────────────────────────

enum AppEvent {
    Key(event::KeyEvent),
    WsConnected,
    WsMessage(String),
    WsClosed,
    WsError(String),
}

// ── Chat line stored in scrollback ────────────────────────────────────────────

#[derive(Debug)]
struct ChatLine {
    prefix: String,
    content: String,
    color: Color,
}

// ── App state ─────────────────────────────────────────────────────────────────

pub struct App {
    /// WebSocket URL.
    url: String,
    /// True while the event loop should keep running.
    running: bool,
    /// Current connection status string shown in the status bar.
    status: String,
    /// Whether we are connected to the server.
    connected: bool,
    /// Whether we are currently receiving a streamed response.
    streaming: bool,
    /// User input buffer (current line being typed).
    input: String,
    /// Chat history lines for display.
    lines: Vec<ChatLine>,
    /// Current response accumulator (cleared on each new "done"/"cancelled"/"error").
    response_buf: String,
    /// Thinking accumulator.
    thinking_buf: String,
    /// Scroll offset (0 = bottom).
    scroll: u16,
    /// Sender half of WebSocket writer channel.
    ws_tx: Option<mpsc::Sender<String>>,
}

impl App {
    pub fn new(url: String) -> Self {
        Self {
            url,
            running: true,
            status: "Connecting…".to_string(),
            connected: false,
            streaming: false,
            input: String::new(),
            lines: Vec::new(),
            response_buf: String::new(),
            thinking_buf: String::new(),
            scroll: 0,
            ws_tx: None,
        }
    }

    /// Run the TUI application. Sets up terminal, connects to WS, enters event loop.
    pub async fn run(&mut self) -> Result<()> {
        // Set up terminal.
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend).context("failed to create terminal")?;
        terminal.clear()?;

        // Event channel.
        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(256);

        // Spawn keyboard reader.
        let key_tx = event_tx.clone();
        tokio::spawn(async move {
            loop {
                if event::poll(std::time::Duration::from_millis(50)).is_err() {
                    break;
                }
                if let Ok(Event::Key(key)) = event::read() {
                    if key_tx.send(AppEvent::Key(key)).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Connect WebSocket.
        let url = self.url.clone();
        let ws_tx_clone = event_tx.clone();
        let (ws_write_tx, mut ws_write_rx) = mpsc::channel::<String>(256);
        self.ws_tx = Some(ws_write_tx);

        tokio::spawn(async move {
            match connect_and_run_ws(&url, ws_tx_clone, &mut ws_write_rx).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("WebSocket task ended with error: {e:#}");
                }
            }
        });

        // Main event loop.
        while self.running {
            // Draw.
            terminal.draw(|f| self.draw(f))?;

            // Wait for next event.
            tokio::select! {
                Some(ev) = event_rx.recv() => {
                    self.handle_event(ev);
                }
            }
        }

        // Restore terminal.
        disable_raw_mode().context("failed to disable raw mode")?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        Ok(())
    }

    // ── Event handling ────────────────────────────────────────────────────────

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::WsConnected => {
                self.connected = true;
                self.streaming = false;
                self.status = "Connected".to_string();
                self.push_line("─".into(), "Connected to MyClaw server.".into(), Color::Cyan);
            }
            AppEvent::WsMessage(raw) => self.handle_ws_message(&raw),
            AppEvent::WsClosed => {
                self.connected = false;
                self.streaming = false;
                self.status = "Disconnected".to_string();
                self.ws_tx = None;
                self.push_line("─".into(), "Connection closed.".into(), Color::Yellow);
            }
            AppEvent::WsError(err) => {
                self.connected = false;
                self.streaming = false;
                self.status = format!("Error: {err}");
                self.push_line("!".into(), format!("WebSocket error: {err}"), Color::Red);
            }
        }
    }

    fn handle_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Ctrl+C → quit
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.running = false;
            }
            // Enter → send message
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.send_user_input();
            }
            // Escape → cancel current turn
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.cancel_current_turn();
            }
            // Backspace
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                self.input.pop();
            }
            // Character input
            (KeyModifiers::NONE, KeyCode::Char(ch)) => {
                self.input.push(ch);
            }
            // Scroll up
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                self.scroll = self.scroll.saturating_add(5);
            }
            // Scroll down
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.scroll = self.scroll.saturating_sub(5);
            }
            _ => {}
        }
    }

    fn send_user_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();

        // Display user message.
        self.push_line("You".into(), text.clone(), Color::Green);

        // Send over WebSocket.
        if let Some(tx) = &self.ws_tx {
            let msg = serde_json::json!({
                "type": "message",
                "content": text,
            })
            .to_string();
            // Non-blocking send; if channel full, just log.
            match tx.try_send(msg) {
                Ok(()) => {
                    self.streaming = true;
                    self.response_buf.clear();
                    self.thinking_buf.clear();
                    self.status = "Streaming…".to_string();
                }
                Err(e) => {
                    warn!("Failed to send message: {e}");
                    self.push_line("!".into(), "Send failed (not connected?).".into(), Color::Red);
                }
            }
        } else {
            self.push_line("!".into(), "Not connected to server.".into(), Color::Red);
        }
    }

    fn cancel_current_turn(&mut self) {
        if !self.streaming {
            return;
        }
        if let Some(tx) = &self.ws_tx {
            let msg = serde_json::json!({"type": "cancel"}).to_string();
            let _ = tx.try_send(msg);
        }
    }

    fn handle_ws_message(&mut self, raw: &str) {
        let msg = match serde_json::from_str::<ServerMsg>(raw) {
            Ok(m) => m,
            Err(e) => {
                debug!("Failed to parse WS message: {e} — raw: {raw}");
                return;
            }
        };

        match msg {
            ServerMsg::Chunk { delta } => {
                self.streaming = true;
                self.response_buf.push_str(&delta);
            }
            ServerMsg::Thinking { delta } => {
                self.thinking_buf.push_str(&delta);
            }
            ServerMsg::ToolCall { name, args } => {
                let detail = if args.is_object() && !args.as_object().is_none_or(|m| m.is_empty()) {
                    format!("{}({})", name, args)
                } else {
                    name
                };
                self.push_line("Tool".into(), format!("→ {detail}"), Color::Cyan);
            }
            ServerMsg::ToolResult { name, output } => {
                let preview = truncate_str(&output, 200);
                self.push_line("Tool".into(), format!("← {name}: {preview}"), Color::DarkGray);
            }
            ServerMsg::Done { text } => {
                // Finalize the accumulated response.
                if !text.is_empty() {
                    self.push_line("AI".into(), text, Color::White);
                } else {
                    let buf = std::mem::take(&mut self.response_buf);
                    if !buf.is_empty() {
                        self.push_line("AI".into(), buf, Color::White);
                    }
                }
                self.response_buf.clear();
                self.thinking_buf.clear();
                self.streaming = false;
                self.status = "Ready".to_string();
                // Reset scroll to bottom.
                self.scroll = 0;
            }
            ServerMsg::Cancelled { partial } => {
                if !partial.is_empty() {
                    self.push_line("AI".into(), partial, Color::Yellow);
                }
                self.response_buf.clear();
                self.thinking_buf.clear();
                self.streaming = false;
                self.status = "Cancelled".to_string();
                self.scroll = 0;
            }
            ServerMsg::Error { message } => {
                self.push_line("!".into(), message.clone(), Color::Red);
                self.response_buf.clear();
                self.thinking_buf.clear();
                self.streaming = false;
                self.status = format!("Error: {message}");
                self.scroll = 0;
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn push_line(&mut self, prefix: String, content: String, color: Color) {
        self.lines.push(ChatLine { prefix, content, color });
    }

    // ── Drawing ───────────────────────────────────────────────────────────────

    fn draw(&self, f: &mut Frame) {
        let size = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),  // status bar
                Constraint::Min(3),     // messages
                Constraint::Length(3),  // input
            ])
            .split(size);

        self.draw_status_bar(f, chunks[0]);
        self.draw_messages(f, chunks[1]);
        self.draw_input(f, chunks[2]);
    }

    fn draw_status_bar(&self, f: &mut Frame, area: Rect) {
        let style = if self.connected {
            Style::default().fg(Color::Black).bg(Color::Green)
        } else {
            Style::default().fg(Color::White).bg(Color::Red)
        };
        let status = Span::styled(format!(" MyClaw TUI │ {} ", self.status), style.add_modifier(Modifier::BOLD));
        let bar = Line::from(status);
        f.render_widget(Paragraph::new(bar), area);
    }

    fn draw_messages(&self, f: &mut Frame, area: Rect) {
        let mut text_lines: Vec<Line> = Vec::new();

        // Render stored chat lines.
        for line in &self.lines {
            let prefix_style = Style::default()
                .fg(line.color)
                .add_modifier(Modifier::BOLD);
            let content_style = Style::default().fg(line.color);
            text_lines.push(Line::from(vec![
                Span::styled(format!("[{}] ", line.prefix), prefix_style),
                Span::styled(&line.content, content_style),
            ]));
        }

        // Render in-progress streaming response.
        if self.streaming {
            if !self.thinking_buf.is_empty() {
                let think_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::ITALIC);
                text_lines.push(Line::from(Span::styled(
                    format!("💭 {}", self.thinking_buf),
                    think_style,
                )));
            }
            if !self.response_buf.is_empty() {
                let stream_style = Style::default().fg(Color::White);
                text_lines.push(Line::from(vec![
                    Span::styled("[AI] ".to_string(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(&self.response_buf, stream_style),
                ]));
            }
        }

        let total_lines = text_lines.len() as u16;
        let visible = area.height.saturating_sub(2); // minus border
        let max_scroll = total_lines.saturating_sub(visible);
        let effective_scroll = self.scroll.min(max_scroll);
        let start = max_scroll.saturating_sub(effective_scroll) as usize;
        let end = (start + visible as usize).min(text_lines.len());

        let visible_lines: Vec<Line> = text_lines.into_iter().skip(start).take(end - start).collect();

        let messages = Paragraph::new(visible_lines)
            .block(Block::default().borders(Borders::TOP).title(" Messages "))
            .wrap(Wrap { trim: false });
        f.render_widget(messages, area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let input_text = Text::from(vec![
            Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw(&self.input),
                Span::raw("█"),
            ]),
        ]);
        let input = Paragraph::new(input_text)
            .block(Block::default().borders(Borders::TOP).title(" Input (Enter=send, Esc=cancel, Ctrl+C=quit) "));
        f.render_widget(input, area);
    }
}

// ── WebSocket connection task ─────────────────────────────────────────────────

async fn connect_and_run_ws(
    url: &str,
    event_tx: mpsc::Sender<AppEvent>,
    ws_write_rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    info!("Connecting to WebSocket at {url}");

    let (mut ws_stream, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("failed to connect to WebSocket server")?;

    info!("WebSocket connected");
    let _ = event_tx.send(AppEvent::WsConnected).await;

    loop {
        tokio::select! {
            // Incoming WS messages.
            msg = ws_stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if event_tx.send(AppEvent::WsMessage(text.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // tungstenite auto-replies pings
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        let _ = event_tx.send(AppEvent::WsClosed).await;
                        break;
                    }
                    Some(Err(e)) => {
                        let _ = event_tx.send(AppEvent::WsError(format!("{e}"))).await;
                        break;
                    }
                    None => {
                        let _ = event_tx.send(AppEvent::WsClosed).await;
                        break;
                    }
                    _ => {} // binary, frame — ignore
                }
            }
            // Outgoing messages from user input.
            outgoing = ws_write_rx.recv() => {
                match outgoing {
                    Some(text) => {
                        if ws_stream.send(Message::Text(text.into())).await.is_err() {
                            let _ = event_tx.send(AppEvent::WsError("send failed".into())).await;
                            break;
                        }
                    }
                    None => {
                        // Channel closed — app is shutting down.
                        let _ = ws_stream.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Truncate a string to `max` characters, appending "…" if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}
