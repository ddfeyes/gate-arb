//! frontend-ws — WebSocket broadcast to trading dashboard + HTTP API.
//!
//! Thread 3 (cold): broadcasts spreads, P&L, positions to frontend.
//! HTTP API on port +1 (8081): GET /api/trades, GET /api/stats

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info};

use db::Db;

const MAX_LOG_ENTRIES: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct DashboardState {
    pub spread_bps: f64,
    pub bid_price: f64,
    pub ask_price: f64,
    pub pnl_raw: i64,
    pub cumulative_pnl_usd: f64,
    pub total_trades: i64,
    pub win_rate: f64,
    pub position_open: bool,
    pub recent_logs: Vec<String>,
}

pub struct FrontendWs {
    pub state: Arc<RwLock<DashboardState>>,
    pub logs: Arc<RwLock<VecDeque<String>>>,
    pub db: Option<Arc<Db>>,
}

impl FrontendWs {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(DashboardState {
                spread_bps: 0.0,
                bid_price: 0.0,
                ask_price: 0.0,
                pnl_raw: 0,
                cumulative_pnl_usd: 0.0,
                total_trades: 0,
                win_rate: 0.0,
                position_open: false,
                recent_logs: Vec::new(),
            })),
            logs: Arc::new(RwLock::new(VecDeque::with_capacity(MAX_LOG_ENTRIES))),
            db: None,
        }
    }

    pub fn with_db(mut self, db: Arc<Db>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn update(&self, spread_bps: f64, bid_price: f64, ask_price: f64) {
        let mut s = self.state.write();
        s.spread_bps = spread_bps;
        s.bid_price = bid_price;
        s.ask_price = ask_price;
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

    /// Start WS + HTTP servers.
    pub async fn run(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let ws_port = port;
        let api_port = port + 1;

        // WS server
        let ws_self = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = ws_self.run_ws(ws_port).await {
                tracing::error!("Frontend WS error: {:?}", e);
            }
        });

        // HTTP API server
        let api_self = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = api_self.run_api(api_port).await {
                tracing::error!("HTTP API error: {:?}", e);
            }
        });

        info!("Frontend servers: WS={}, HTTP API={}", ws_port, api_port);
        // Keep-alive
        tokio::signal::ctrl_c().await.ok();
        Ok(())
    }

    async fn run_ws(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).await?;
        info!("Frontend WS listening on ws://{}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let state = Arc::clone(&self.state);
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_client(state, stream).await {
                            error!("Client error {}: {:?}", addr, e);
                        }
                    });
                }
                Err(e) => error!("Accept error: {:?}", e),
            }
        }
    }

    /// Refresh cumulative P&L + stats from DB before broadcast.
    fn refresh_stats(&self) {
        if let Some(ref db) = self.db {
            let stats = db.get_stats();
            let mut s = self.state.write();
            s.pnl_raw = stats.total_pnl_usd;
            s.cumulative_pnl_usd = stats.total_pnl_usd as f64 / 100_000_000.0;
            s.total_trades = stats.total_trades;
            s.win_rate = stats.win_rate;
        }
    }

    async fn handle_client(
        &self,
        state: Arc<RwLock<DashboardState>>,
        stream: tokio::net::TcpStream,
    ) -> anyhow::Result<()> {
        let ws = accept_async(stream).await?;
        let (mut write, _read) = ws.split();

        // Send initial state
        let snapshot = {
            let s = state.read();
            serde_json::to_string(&*s)?
        };
        write.send(Message::Text(snapshot.into())).await?;

        // Broadcast updates every 500ms
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Refresh P&L stats from DB before each broadcast
                    self.refresh_stats();
                    let snapshot = {
                        let s = state.read();
                        serde_json::to_string(&*s)?
                    };
                    if write.send(Message::Text(snapshot.into())).await.is_err() {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Minimal HTTP API server — GET /api/trades, GET /api/stats.
    async fn run_api(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", port);
        let listener = TcpListener::bind(&addr).await?;
        info!("HTTP API listening on http://{}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_http(stream).await {
                            tracing::debug!("HTTP error: {:?}", e);
                        }
                    });
                }
                Err(e) => error!("HTTP accept error: {:?}", e),
            }
        }
    }

    async fn handle_http(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }

        let request = std::str::from_utf8(&buf[..n])?;
        let first_line = request.lines().next().unwrap_or("");
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");

        let response = match path {
            "/api/trades" => {
                let trades = self
                    .db
                    .as_ref()
                    .map(|db| db.get_recent_trades(50))
                    .unwrap_or_default();
                let body = serde_json::to_string(&trades).unwrap_or_else(|_| "[]".into());
                http_200("application/json", &body)
            }
            "/api/stats" => {
                let stats = self
                    .db
                    .as_ref()
                    .map(|db| db.get_stats())
                    .unwrap_or_default();
                let body = serde_json::to_string(&stats).unwrap_or_else(|_| "{}".into());
                http_200("application/json", &body)
            }
            "/health" => http_200("text/plain", "ok"),
            _ => http_404(),
        };

        stream.write_all(response.as_bytes()).await?;
        stream.flush().await?;
        Ok(())
    }
}

fn http_200(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n\
         {}",
        content_type,
        body.len(),
        body
    )
}

fn http_404() -> String {
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
}

impl Default for FrontendWs {
    fn default() -> Self {
        Self::new()
    }
}
