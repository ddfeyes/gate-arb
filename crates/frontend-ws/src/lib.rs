//! frontend-ws — WebSocket broadcast to trading dashboard.
//!
//! Thread 3 (cold): broadcasts spreads, P&L, positions to frontend.
//!
//! Protocol:
//!   - Browser HTTP GET / → serves the self-contained dashboard HTML
//!   - Browser WS Upgrade / → stream DashboardState JSON at 500ms interval
//!   - Non-upgrade TCP connections → HTTP 200 with HTML
//!
//! No separate HTTP port is needed.

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info};

const MAX_LOG_ENTRIES: usize = 100;

/// Embedded dashboard HTML served at HTTP GET /.
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Returns true if `request_text` contains a WebSocket upgrade header.
///
/// RFC 7230 §3.2: header field values are case-insensitive.
/// Handles "Upgrade: websocket", "Upgrade: WebSocket", etc.
#[inline]
pub fn is_ws_upgrade(request_text: &str) -> bool {
    request_text
        .to_ascii_lowercase()
        .contains("upgrade: websocket")
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardState {
    pub spread_bps: f64,
    pub bid_price: f64,
    pub ask_price: f64,
    pub pnl_raw: i64,
    pub pnl_usd: f64,
    pub pnl_pct: f64,
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub position_open: bool,
    /// Funding rate in integer basis points (e.g. 150 = 1.50% per 8 h).
    pub funding_rate_bps: i64,
    /// Unix epoch ms when state was last updated.
    pub timestamp_ms: u64,
    pub recent_logs: Vec<String>,
}

pub struct FrontendWs {
    pub state: Arc<RwLock<DashboardState>>,
    pub logs: Arc<RwLock<VecDeque<String>>>,
}

impl FrontendWs {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(DashboardState {
                spread_bps: 0.0,
                bid_price: 0.0,
                ask_price: 0.0,
                pnl_raw: 0,
                pnl_usd: 0.0,
                pnl_pct: 0.0,
                total_trades: 0,
                wins: 0,
                losses: 0,
                position_open: false,
                funding_rate_bps: 0,
                timestamp_ms: 0,
                recent_logs: Vec::new(),
            })),
            logs: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_LOG_ENTRIES))),
        }
    }

    pub fn update(&self, spread_bps: f64, bid_price: f64, ask_price: f64) {
        let mut s = self.state.write();
        s.spread_bps = spread_bps;
        s.bid_price = bid_price;
        s.ask_price = ask_price;
        s.timestamp_ms = now_ms();
    }

    pub fn update_pnl(
        &self,
        pnl_raw: i64,
        total_trades: u64,
        wins: u64,
        losses: u64,
        position_open: bool,
    ) {
        let mut s = self.state.write();
        s.pnl_raw = pnl_raw;
        s.pnl_usd = pnl_raw as f64 / 100_000_000.0;
        // PnL % relative to $500 notional (0.01 BTC @ $50k)
        const NOTIONAL_USD: f64 = 500.0;
        s.pnl_pct = (s.pnl_usd / NOTIONAL_USD) * 100.0;
        s.total_trades = total_trades;
        s.wins = wins;
        s.losses = losses;
        s.position_open = position_open;
        s.timestamp_ms = now_ms();
    }

    /// Update the latest funding rate.
    ///
    /// Unit: integer basis points (1 bps = 0.01%). So `150` means 1.50% per 8 h.
    /// The dashboard divides by 100 to display as a percentage.
    pub fn update_funding_rate(&self, funding_rate_bps: i64) {
        let mut s = self.state.write();
        s.funding_rate_bps = funding_rate_bps;
        s.timestamp_ms = now_ms();
    }

    pub fn log(&self, msg: String) {
        let mut logs = self.logs.write();
        if logs.len() >= MAX_LOG_ENTRIES {
            logs.pop_front();
        }
        logs.push_back(msg.clone());
        drop(logs);
        let mut s = self.state.write();
        s.recent_logs.push(msg);
        if s.recent_logs.len() > MAX_LOG_ENTRIES {
            s.recent_logs.remove(0);
        }
    }

    /// Start server on given port.
    /// - WS connections get a live JSON stream at 500ms intervals.
    /// - HTTP GET / returns the self-contained dashboard HTML.
    pub async fn run(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).await?;
        info!("Frontend WS listening on ws://{}", addr);
        info!("Dashboard available at http://{}/", addr);

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = this.dispatch(stream).await {
                            error!("Client {} error: {:?}", addr, e);
                        }
                    });
                }
                Err(e) => error!("Accept error: {:?}", e),
            }
        }
    }

    /// Peek at up to 4 KB of the request to decide HTTP vs WebSocket upgrade.
    ///
    /// RFC 7230 §3.2 — header field values are case-insensitive.
    /// We lowercase the entire peeked buffer before checking for "upgrade: websocket"
    /// so "Upgrade: WebSocket", "upgrade: websocket", or any mix all route correctly.
    async fn dispatch(&self, stream: tokio::net::TcpStream) -> anyhow::Result<()> {
        let mut buf = vec![0u8; 4096];
        let n = stream.peek(&mut buf).await?;

        // Not enough bytes or not an HTTP request — close silently.
        if n < 4 || !buf[..n].starts_with(b"GET") {
            return Ok(());
        }

        // Case-insensitive check for WebSocket upgrade header (RFC 7230).
        // Covers: "Upgrade: websocket", "Upgrade: WebSocket", proxy variants, etc.
        let is_ws = is_ws_upgrade(std::str::from_utf8(&buf[..n]).unwrap_or(""));

        if is_ws {
            self.handle_ws(stream).await
        } else {
            self.handle_http(stream).await
        }
    }

    async fn handle_http(&self, mut stream: tokio::net::TcpStream) -> anyhow::Result<()> {
        // Drain the request
        let mut buf = vec![0u8; 4096];
        let _ = stream.read(&mut buf).await?;

        let body = DASHBOARD_HTML.as_bytes();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(response.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn handle_ws(&self, stream: tokio::net::TcpStream) -> anyhow::Result<()> {
        let ws = accept_async(stream).await?;
        let (mut write, _read) = ws.split();

        // Send initial full state
        {
            let snapshot = serde_json::to_string(&*self.state.read())?;
            write.send(Message::Text(snapshot.into())).await?;
        }

        // Push updates at 500ms
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
        loop {
            interval.tick().await;
            let snapshot = serde_json::to_string(&*self.state.read())?;
            if write.send(Message::Text(snapshot.into())).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

impl Default for FrontendWs {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
