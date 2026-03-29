//! db — structured trade logging to SQLite.
//!
//! Provides a non-blocking async writer (mpsc channel → background task)
//! for use from the hot/warm strategy path.
//!
//! Schema:
//! - `trades`: one row per closed arbitrage position
//! - `spreads`: time-series spread snapshots (sampled, not every tick)
//!
//! Env vars:
//! - `DB_PATH` — path to SQLite file (default: `gate-arb.db`)

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ──────────────────────────────────────────────────────────────────────────────
// Public types
// ──────────────────────────────────────────────────────────────────────────────

/// A closed arbitrage trade record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TradeRecord {
    /// Unix microseconds when position was opened.
    pub opened_at_us: u64,
    /// Unix microseconds when position was closed.
    pub closed_at_us: u64,
    /// Entry spot bid price in raw units (1e8 = 1 USDT).
    pub entry_price_raw: i64,
    /// Exit price in raw units.
    pub exit_price_raw: i64,
    /// Position size in raw units (1e8 = 1 unit).
    pub size_raw: i64,
    /// Realized P&L in raw units (positive = profit).
    pub pnl_raw: i64,
    /// P&L as percentage of position value × 1e6 (i.e. 1% = 1_000_000).
    pub pnl_pct_raw: i64,
    /// Human-readable close reason (e.g. "exit_spread", "timeout", "emergency").
    pub exit_reason: String,
    /// True if paper trade (not executed on exchange).
    pub paper: bool,
}

/// A spread snapshot for time-series analysis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpreadSnapshot {
    /// Unix microseconds.
    pub ts_us: u64,
    /// Spread in raw units (positive = spot cheaper than perp).
    pub spread_raw: i64,
    /// True if inverted direction (perp cheaper).
    pub inverted: bool,
    /// Spot bid price (raw).
    pub spot_price_raw: i64,
    /// Perp ask price (raw).
    pub perp_price_raw: i64,
}

// ──────────────────────────────────────────────────────────────────────────────
// Writer command enum
// ──────────────────────────────────────────────────────────────────────────────

enum DbCmd {
    Trade(TradeRecord),
    Spread(SpreadSnapshot),
    Shutdown,
}

// ──────────────────────────────────────────────────────────────────────────────
// DbWriter — non-blocking handle
// ──────────────────────────────────────────────────────────────────────────────

/// Non-blocking handle to the DB writer background task.
///
/// Clone freely — all clones share the same sender channel.
/// Drop all clones to trigger graceful shutdown (channel closes → background task exits).
#[derive(Clone, Debug)]
pub struct DbWriter {
    tx: mpsc::Sender<DbCmd>,
}

impl DbWriter {
    /// Open (or create) the SQLite database at `path` and spawn a background writer task.
    ///
    /// Returns `(DbWriter, join_handle)`. Keep the handle if you want to await shutdown.
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<(Self, tokio::task::JoinHandle<()>)> {
        let path: PathBuf = path.as_ref().to_path_buf();
        let conn = open_db(&path)?;
        let (tx, mut rx) = mpsc::channel::<DbCmd>(1024);

        let handle = tokio::spawn(async move {
            info!("db: writer task started ({})", path.display());
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    DbCmd::Trade(r) => {
                        if let Err(e) = insert_trade(&conn, &r) {
                            error!("db: failed to insert trade: {}", e);
                        }
                    }
                    DbCmd::Spread(s) => {
                        if let Err(e) = insert_spread(&conn, &s) {
                            error!("db: failed to insert spread: {}", e);
                        }
                    }
                    DbCmd::Shutdown => {
                        info!("db: writer task shutting down");
                        break;
                    }
                }
            }
            info!("db: writer task exited");
        });

        Ok((Self { tx }, handle))
    }

    /// Open using `DB_PATH` env var, or `gate-arb.db` as default.
    pub fn from_env() -> anyhow::Result<(Self, tokio::task::JoinHandle<()>)> {
        let path = std::env::var("DB_PATH").unwrap_or_else(|_| "gate-arb.db".to_string());
        info!("db: using path={}", path);
        Self::open(path)
    }

    /// Record a closed trade (non-blocking, best-effort).
    /// Drops silently if the channel is full (hot-path safe).
    pub fn write_trade(&self, record: TradeRecord) {
        if self.tx.try_send(DbCmd::Trade(record)).is_err() {
            warn!("db: trade write dropped (channel full or closed)");
        }
    }

    /// Record a spread snapshot (non-blocking, best-effort).
    pub fn write_spread(&self, snapshot: SpreadSnapshot) {
        if self.tx.try_send(DbCmd::Spread(snapshot)).is_err() {
            warn!("db: spread write dropped (channel full or closed)");
        }
    }

    /// Signal the background task to flush and exit.
    /// After calling this, further writes are dropped.
    pub fn shutdown(&self) {
        let _ = self.tx.try_send(DbCmd::Shutdown);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal DB helpers
// ──────────────────────────────────────────────────────────────────────────────

fn open_db(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;

    // WAL mode for concurrent reads; foreign keys on.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trades (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            opened_at_us    INTEGER NOT NULL,
            closed_at_us    INTEGER NOT NULL,
            entry_price_raw INTEGER NOT NULL,
            exit_price_raw  INTEGER NOT NULL,
            size_raw        INTEGER NOT NULL,
            pnl_raw         INTEGER NOT NULL,
            pnl_pct_raw     INTEGER NOT NULL,
            exit_reason     TEXT NOT NULL,
            paper           INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX IF NOT EXISTS idx_trades_opened ON trades(opened_at_us);

        CREATE TABLE IF NOT EXISTS spreads (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_us           INTEGER NOT NULL,
            spread_raw      INTEGER NOT NULL,
            inverted        INTEGER NOT NULL DEFAULT 0,
            spot_price_raw  INTEGER NOT NULL,
            perp_price_raw  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_spreads_ts ON spreads(ts_us);",
    )?;

    info!("db: schema ready ({})", path.display());
    Ok(conn)
}

fn insert_trade(conn: &Connection, r: &TradeRecord) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO trades
         (opened_at_us, closed_at_us, entry_price_raw, exit_price_raw,
          size_raw, pnl_raw, pnl_pct_raw, exit_reason, paper)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            r.opened_at_us as i64,
            r.closed_at_us as i64,
            r.entry_price_raw,
            r.exit_price_raw,
            r.size_raw,
            r.pnl_raw,
            r.pnl_pct_raw,
            r.exit_reason,
            r.paper as i64,
        ],
    )?;
    Ok(())
}

fn insert_spread(conn: &Connection, s: &SpreadSnapshot) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO spreads (ts_us, spread_raw, inverted, spot_price_raw, perp_price_raw)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            s.ts_us as i64,
            s.spread_raw,
            s.inverted as i64,
            s.spot_price_raw,
            s.perp_price_raw,
        ],
    )?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Read helpers (for health/API endpoints)
// ──────────────────────────────────────────────────────────────────────────────

/// Read-only connection for query endpoints.
pub struct DbReader {
    conn: Connection,
}

impl DbReader {
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA query_only = ON;")?;
        Ok(Self { conn })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let path = std::env::var("DB_PATH").unwrap_or_else(|_| "gate-arb.db".to_string());
        Self::open(path)
    }

    /// Return the N most recent trades (newest first).
    pub fn recent_trades(&self, limit: usize) -> anyhow::Result<Vec<TradeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT opened_at_us, closed_at_us, entry_price_raw, exit_price_raw,
                    size_raw, pnl_raw, pnl_pct_raw, exit_reason, paper
             FROM trades ORDER BY closed_at_us DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(TradeRecord {
                opened_at_us: row.get::<_, i64>(0)? as u64,
                closed_at_us: row.get::<_, i64>(1)? as u64,
                entry_price_raw: row.get(2)?,
                exit_price_raw: row.get(3)?,
                size_raw: row.get(4)?,
                pnl_raw: row.get(5)?,
                pnl_pct_raw: row.get(6)?,
                exit_reason: row.get(7)?,
                paper: row.get::<_, i64>(8)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// P&L summary: total_trades, wins, total_pnl_raw.
    pub fn pnl_summary(&self) -> anyhow::Result<PnlSummary> {
        let row = self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN pnl_raw > 0 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(pnl_raw), 0)
             FROM trades",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?;
        Ok(PnlSummary {
            total_trades: row.0 as u64,
            wins: row.1 as u64,
            total_pnl_raw: row.2,
        })
    }

    /// Return the N most recent spread snapshots (newest first).
    pub fn recent_spreads(&self, limit: usize) -> anyhow::Result<Vec<SpreadSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts_us, spread_raw, inverted, spot_price_raw, perp_price_raw
             FROM spreads ORDER BY ts_us DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(SpreadSnapshot {
                ts_us: row.get::<_, i64>(0)? as u64,
                spread_raw: row.get(1)?,
                inverted: row.get::<_, i64>(2)? != 0,
                spot_price_raw: row.get(3)?,
                perp_price_raw: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PnlSummary {
    pub total_trades: u64,
    pub wins: u64,
    pub total_pnl_raw: i64,
}
