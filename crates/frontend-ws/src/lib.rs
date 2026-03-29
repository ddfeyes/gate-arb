//! frontend-ws — WebSocket broadcast to trading dashboard.
//!
//! Thread 3 (cold): broadcasts spreads, P&L, positions to frontend.

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info};

const MAX_LOG_ENTRIES: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct PnlUpdate {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub pnl_usd: f64,
    pub pnl_pct: f64,
    pub last_signal_at: Option<u64>,
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
        // PnL % is relative to a notional base (e.g. 0.01 BTC * $50k ≈ $500)
        // Use a fixed notional of $500 for display purposes
        const NOTIONAL_USD: f64 = 500.0;
        s.pnl_pct = if NOTIONAL_USD > 0.0 {
            (s.pnl_usd / NOTIONAL_USD) * 100.0
        } else {
            0.0
        };
        s.total_trades = total_trades;
        s.wins = wins;
        s.losses = losses;
        s.position_open = position_open;
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

    /// Start WS server on given port, broadcast state to all connections.
    pub async fn run(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
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
}

impl Default for FrontendWs {
    fn default() -> Self {
        Self::new()
    }
}
