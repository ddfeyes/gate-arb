//! db — SQLite persistence layer for gate-arb.
//!
//! Provides:
//! - `trades` table: entry/exit/P&L for each arbitrage round-trip
//! - `spreads` table: time-series of spread_pct snapshots
//! - Async writer via bounded mpsc (non-blocking from hot path)
//! - HTTP API: `GET /api/trades`, `GET /api/pnl-summary`

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Trade record written atomically on position close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub symbol: String,
    pub entry_ts: i64, // unix micros
    pub exit_ts: i64,
    pub spot_entry: i64, // raw fixed-point (1e8)
    pub perp_entry: i64,
    pub spot_exit: i64,
    pub perp_exit: i64,
    pub size_usd: i64,
    pub pnl_usd: i64,
    pub pnl_pct: i64, // basis points * 100 (i.e. 10000 = 100%)
    pub exit_reason: String,
}

/// Spread snapshot for analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpreadRecord {
    pub ts: i64,
    pub spread_bps: i64, // basis points * 100
    pub bid_price: i64,
    pub ask_price: i64,
}

/// P&L summary returned by GET /api/pnl-summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSummary {
    pub total_trades: i64,
    pub win_rate: f64,
    pub total_pnl_usd: i64,
    pub avg_trade_duration_secs: f64,
}

enum DbCmd {
    WriteTrade(TradeRecord),
    WriteSpread(SpreadRecord),
    GetTrades(i64, tokio::sync::oneshot::Sender<Vec<TradeRecord>>),
    GetPnlSummary(tokio::sync::oneshot::Sender<PnlSummary>),
    Shutdown,
}

/// SQLite-backed async writer. Thread-safe, cheap to clone.
#[derive(Clone)]
pub struct DbWriter {
    tx: tokio::sync::mpsc::Sender<DbCmd>,
}

impl DbWriter {
    /// Create and spawn the DB thread. Returns a `DbWriter` handle.
    pub fn new(db_path: &str) -> Result<Self> {
        let (tx, rx) = mpsc::channel(1024);

        let conn = Connection::open(db_path)?;
        Self::init_schema(&conn)?;

        std::thread::spawn(move || Self::db_loop(conn, rx));

        Ok(Self { tx })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol TEXT NOT NULL,
                entry_ts INTEGER NOT NULL,
                exit_ts INTEGER NOT NULL,
                spot_entry INTEGER NOT NULL,
                perp_entry INTEGER NOT NULL,
                spot_exit INTEGER NOT NULL,
                perp_exit INTEGER NOT NULL,
                size_usd INTEGER NOT NULL,
                pnl_usd INTEGER NOT NULL,
                pnl_pct INTEGER NOT NULL,
                exit_reason TEXT NOT NULL
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS spreads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                spread_bps INTEGER NOT NULL,
                bid_price INTEGER NOT NULL,
                ask_price INTEGER NOT NULL
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_trades_entry_ts ON trades(entry_ts DESC)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_spreads_ts ON spreads(ts DESC)",
            [],
        )?;

        Ok(())
    }

    fn db_loop(conn: Connection, mut rx: mpsc::Receiver<DbCmd>) {
        let conn = Mutex::new(conn);
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                DbCmd::WriteTrade(trade) => {
                    let conn = conn.lock();
                    if let Err(e) = conn.execute(
                        "INSERT INTO trades
                         (symbol,entry_ts,exit_ts,spot_entry,perp_entry,spot_exit,perp_exit,size_usd,pnl_usd,pnl_pct,exit_reason)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![
                            trade.symbol,
                            trade.entry_ts,
                            trade.exit_ts,
                            trade.spot_entry,
                            trade.perp_entry,
                            trade.spot_exit,
                            trade.perp_exit,
                            trade.size_usd,
                            trade.pnl_usd,
                            trade.pnl_pct,
                            trade.exit_reason,
                        ],
                    ) {
                        tracing::error!("failed to write trade: {}", e);
                    }
                }
                DbCmd::WriteSpread(spread) => {
                    let conn = conn.lock();
                    if let Err(e) = conn.execute(
                        "INSERT INTO spreads (ts,spread_bps,bid_price,ask_price) VALUES (?1,?2,?3,?4)",
                        params![spread.ts, spread.spread_bps, spread.bid_price, spread.ask_price],
                    ) {
                        tracing::error!("failed to write spread: {}", e);
                    }
                }
                DbCmd::GetTrades(n, resp) => {
                    let conn = conn.lock();
                    let mut stmt = conn.prepare(
                        "SELECT symbol,entry_ts,exit_ts,spot_entry,perp_entry,spot_exit,perp_exit,
                                size_usd,pnl_usd,pnl_pct,exit_reason
                         FROM trades ORDER BY id DESC LIMIT ?1"
                    ).expect("prepare trades query");
                    let trades = stmt
                        .query_map([n], |row| {
                            Ok(TradeRecord {
                                symbol: row.get(0)?,
                                entry_ts: row.get(1)?,
                                exit_ts: row.get(2)?,
                                spot_entry: row.get(3)?,
                                perp_entry: row.get(4)?,
                                spot_exit: row.get(5)?,
                                perp_exit: row.get(6)?,
                                size_usd: row.get(7)?,
                                pnl_usd: row.get(8)?,
                                pnl_pct: row.get(9)?,
                                exit_reason: row.get(10)?,
                            })
                        })
                        .expect("query map");
                    let result: Vec<_> = trades.filter_map(|t| t.ok()).collect();
                    let _ = resp.send(result);
                }
                DbCmd::GetPnlSummary(resp) => {
                    let conn = conn.lock();
                    let total: i64 = conn
                        .query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))
                        .unwrap_or(0);
                    let wins: i64 = conn
                        .query_row("SELECT COUNT(*) FROM trades WHERE pnl_usd > 0", [], |r| {
                            r.get(0)
                        })
                        .unwrap_or(0);
                    let total_pnl: i64 = conn
                        .query_row("SELECT COALESCE(SUM(pnl_usd),0) FROM trades", [], |r| {
                            r.get(0)
                        })
                        .unwrap_or(0);
                    let avg_dur: f64 = conn
                        .query_row(
                            "SELECT COALESCE(AVG(exit_ts - entry_ts) / 1_000_000.0, 0.0) FROM trades",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap_or(0.0);
                    let win_rate = if total > 0 {
                        wins as f64 / total as f64
                    } else {
                        0.0
                    };
                    let _ = resp.send(PnlSummary {
                        total_trades: total,
                        win_rate,
                        total_pnl_usd: total_pnl,
                        avg_trade_duration_secs: avg_dur,
                    });
                }
                DbCmd::Shutdown => break,
            }
        }
    }

    /// Write a trade atomically (called from strategy thread on position close).
    #[inline(always)]
    pub fn write_trade(&self, trade: TradeRecord) {
        // non-blocking send — hot path must not block
        let _ = self.tx.try_send(DbCmd::WriteTrade(trade));
    }

    /// Write a spread snapshot (called from hot path engine tick).
    #[inline(always)]
    pub fn write_spread(&self, spread: SpreadRecord) {
        let _ = self.tx.try_send(DbCmd::WriteSpread(spread));
    }

    /// Get last N trades (awaitable).
    pub async fn get_trades(&self, n: i64) -> Vec<TradeRecord> {
        let (resp, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(DbCmd::GetTrades(n, resp)).await;
        rx.await.unwrap_or_default()
    }

    /// Get P&L summary (awaitable).
    pub async fn get_pnl_summary(&self) -> PnlSummary {
        let (resp, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(DbCmd::GetPnlSummary(resp)).await;
        rx.await.unwrap_or(PnlSummary {
            total_trades: 0,
            win_rate: 0.0,
            total_pnl_usd: 0,
            avg_trade_duration_secs: 0.0,
        })
    }
}

/// Start the HTTP API server (thin, no external deps).
pub async fn start_http_server(port: u16, db: DbWriter) -> Result<()> {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    tracing::info!("db HTTP API listening on :{}", port);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let db = db.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_http(stream, db).await {
                        tracing::debug!("HTTP error: {}", e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("accept error: {}", e);
            }
        }
    }
}

async fn handle_http(mut stream: tokio::net::TcpStream, db: DbWriter) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await?;

    // Parse HTTP path
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body) = match path {
        "/api/trades" => {
            let trades = db.get_trades(100).await;
            let body = serde_json::to_string(&trades).unwrap_or_else(|_| "[]".to_string());
            (200, body)
        }
        "/api/pnl-summary" => {
            let summary = db.get_pnl_summary().await;
            let body = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".to_string());
            (200, body)
        }
        "/health" => (200, r#"{"ok":true}"#.to_string()),
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    };

    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}
