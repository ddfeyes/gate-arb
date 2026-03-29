//! db — SQLite persistence layer for gate-arb.
//!
//! Tables:
//!   - `trades`: entry/exit P&L history (written atomically on close)
//!   - `spreads`: time-series spread snapshots (written in batches)
//!
//! Hot path → cold path communication via a lock-free SPSC ring buffer.
//! Cold task drains the ring and writes to SQLite in batches.

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{error, info};

/// In-memory trade record — written to SQLite on close.
#[derive(Debug, Clone, Copy, Default)]
pub struct TradeRecord {
    pub symbol: &'static str,
    pub entry_ts: i64,   // unix micros
    pub exit_ts: i64,    // unix micros
    pub spot_entry: i64, // raw fixed-point
    pub perp_entry: i64,
    pub spot_exit: i64,
    pub perp_exit: i64,
    pub size_usd_raw: i64, // raw fixed-point (1e8 = $1)
    pub pnl_usd_raw: i64,
    pub pnl_pct_raw: i64, // basis points * 1e4
    pub exit_reason: &'static str,
}

/// Spread snapshot for time-series analysis.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpreadRecord {
    pub ts: i64, // unix micros
    pub spread_raw: i64,
    pub spread_pct_raw: i64, // basis points * 1e4
    pub symbol: &'static str,
}

/// Ring buffer — lock-free SPSC between hot and cold paths.
// Capacity MUST be power of 2.
const RING_CAP: usize = 4096;

pub struct RingBuffer<T: Copy + Default> {
    data: Vec<Mutex<Option<T>>>,
    head: parking_lot::Mutex<usize>,
    tail: parking_lot::Mutex<usize>,
}

impl<T: Copy + Default + 'static> RingBuffer<T> {
    pub fn new() -> Self {
        // Heap-allocate via Vec to avoid stack overflow from large array init.
        let mut data: Vec<Mutex<Option<T>>> = Vec::with_capacity(RING_CAP);
        for _ in 0..RING_CAP {
            data.push(Mutex::new(None));
        }
        Self {
            data,
            head: parking_lot::Mutex::new(0),
            tail: parking_lot::Mutex::new(0),
        }
    }

    /// Push from hot path — non-blocking, returns false if full.
    #[inline(always)]
    pub fn push(&self, val: T) -> bool {
        let head = *self.head.lock();
        let tail = *self.tail.lock();
        if (head.wrapping_sub(tail)) >= RING_CAP {
            return false; // ring full
        }
        *self.data[head & (RING_CAP - 1)].lock() = Some(val);
        // Release barrier so cold path sees the value
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        *self.head.lock() = head.wrapping_add(1);
        true
    }

    /// Drain all available items — called from cold path only.
    pub fn drain<F>(&self, mut f: F)
    where
        F: FnMut(T),
    {
        let head = *self.head.lock();
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        let mut tail = *self.tail.lock();
        while tail != head {
            if let Some(val) = self.data[tail & (RING_CAP - 1)].lock().take() {
                f(val);
            }
            tail = tail.wrapping_add(1);
        }
        *self.tail.lock() = tail;
    }
}

impl<T: Copy + Default + 'static> Default for RingBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Database handle — SQLite connection pool + rings.
pub struct Db {
    trades_ring: RingBuffer<TradeRecord>,
    spreads_ring: RingBuffer<SpreadRecord>,
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (or create) `gate-arb.db` and run migrations.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS trades (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, symbol TEXT NOT NULL, \
             entry_ts INTEGER NOT NULL, exit_ts INTEGER NOT NULL, \
             spot_entry INTEGER NOT NULL, perp_entry INTEGER NOT NULL, \
             spot_exit INTEGER NOT NULL, perp_exit INTEGER NOT NULL, \
             size_usd INTEGER NOT NULL, pnl_usd INTEGER NOT NULL, \
             pnl_pct INTEGER NOT NULL, exit_reason TEXT NOT NULL)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS spreads (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, \
             symbol TEXT NOT NULL, spread_raw INTEGER NOT NULL, spread_pct INTEGER NOT NULL)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_trades_symbol ON trades(symbol)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_trades_exit_ts ON trades(exit_ts)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_spreads_ts ON spreads(ts)",
            [],
        )?;

        info!("DB initialized at {:?}", path);
        Ok(Self {
            trades_ring: RingBuffer::new(),
            spreads_ring: RingBuffer::new(),
            conn: Mutex::new(conn),
        })
    }

    /// Push a trade record from hot/warm path — non-blocking.
    #[inline(always)]
    pub fn push_trade(&self, trade: TradeRecord) {
        if !self.trades_ring.push(trade) {
            tracing::warn!("trades ring full, dropping trade record");
        }
    }

    /// Push a spread snapshot from hot path — non-blocking.
    #[inline(always)]
    pub fn push_spread(&self, spread: SpreadRecord) {
        if !self.spreads_ring.push(spread) {
            tracing::warn!("spreads ring full, dropping spread record");
        }
    }

    /// Drain rings and persist to SQLite — call from cold task.
    pub fn flush(&self) {
        let conn = self.conn.lock();

        // Trades — write individually
        self.trades_ring.drain(|t| {
            if let Err(e) = conn.execute(
                "INSERT INTO trades (symbol,entry_ts,exit_ts,spot_exit,perp_exit,spot_entry,perp_entry,size_usd,pnl_usd,pnl_pct,exit_reason) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    t.symbol,
                    t.entry_ts,
                    t.exit_ts,
                    t.spot_exit,
                    t.perp_exit,
                    t.spot_entry,
                    t.perp_entry,
                    t.size_usd_raw,
                    t.pnl_usd_raw,
                    t.pnl_pct_raw,
                    t.exit_reason,
                ],
            ) {
                error!("failed to insert trade: {:?}", e);
            }
        });

        // Spreads — batch insert
        let mut batch = Vec::with_capacity(256);
        self.spreads_ring.drain(|s| batch.push(s));
        if !batch.is_empty() {
            let mut stmt = match conn.prepare(
                "INSERT INTO spreads (ts, symbol, spread_raw, spread_pct) VALUES (?1,?2,?3,?4)",
            ) {
                Ok(s) => s,
                Err(e) => {
                    error!("failed to prepare spread insert: {:?}", e);
                    return;
                }
            };
            for s in batch.drain(..) {
                if let Err(e) =
                    stmt.execute(params![s.ts, s.symbol, s.spread_raw, s.spread_pct_raw])
                {
                    error!("failed to insert spread: {:?}", e);
                }
            }
        }
    }

    /// Get last N trades as JSON-serializable structs.
    pub fn get_recent_trades(&self, limit: usize) -> Vec<TradeJson> {
        let conn = self.conn.lock();
        let mut stmt = match conn.prepare(
            "SELECT symbol,entry_ts,exit_ts,spot_entry,perp_entry,spot_exit,perp_exit,size_usd,pnl_usd,pnl_pct,exit_reason \
             FROM trades ORDER BY exit_ts DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        let mut results = Vec::new();
        let mut rows = match stmt.query(params![limit as i64]) {
            Ok(r) => r,
            Err(_) => return vec![],
        };
        while let Ok(Some(row)) = rows.next() {
            results.push(TradeJson {
                symbol: row.get(0).unwrap_or_default(),
                entry_ts: row.get(1).unwrap_or(0),
                exit_ts: row.get(2).unwrap_or(0),
                spot_entry: row.get::<_, i64>(3).unwrap_or(0),
                perp_entry: row.get::<_, i64>(4).unwrap_or(0),
                spot_exit: row.get::<_, i64>(5).unwrap_or(0),
                perp_exit: row.get::<_, i64>(6).unwrap_or(0),
                size_usd: row.get::<_, i64>(7).unwrap_or(0),
                pnl_usd: row.get::<_, i64>(8).unwrap_or(0),
                pnl_pct: row.get::<_, i64>(9).unwrap_or(0),
                exit_reason: row.get(10).unwrap_or_default(),
            });
        }
        results
    }

    /// P&L summary stats.
    pub fn get_stats(&self) -> StatsJson {
        let conn = self.conn.lock();
        let _count: i64 = conn
            .query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))
            .unwrap();

        let conn2 = self.conn.lock();
        let mut stmt = match conn2.prepare(
            "SELECT COUNT(*),COALESCE(SUM(pnl_usd),0),COALESCE(SUM(CASE WHEN pnl_usd>0 THEN 1 ELSE 0 END),0),COALESCE(AVG(exit_ts-entry_ts),0)              FROM trades",
        ) {
            Ok(s) => s,
            Err(e) => { eprintln!("get_stats: prepare error: {:?}", e); return StatsJson::default(); }
        };

        let row = stmt.query_row([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        });

        match row {
            Ok((total, total_pnl, wins, avg_dur)) => {
                eprintln!(
                    "get_stats: total={}, total_pnl={}, wins={}, avg_dur={}",
                    total, total_pnl, wins, avg_dur
                );
                let win_rate = if total > 0 {
                    (wins as f64 / total as f64 * 100.0).round()
                } else {
                    0.0
                };
                StatsJson {
                    total_trades: total,
                    total_pnl_usd: total_pnl,
                    win_rate,
                    avg_trade_duration_secs: (avg_dur / 1_000_000.0).round(),
                }
            }
            Err(e) => {
                eprintln!("get_stats: query_row error: {:?}", e);
                StatsJson::default()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeJson {
    pub symbol: String,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub spot_entry: i64,
    pub perp_entry: i64,
    pub spot_exit: i64,
    pub perp_exit: i64,
    pub size_usd: i64,
    pub pnl_usd: i64,
    pub pnl_pct: i64,
    pub exit_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatsJson {
    pub total_trades: i64,
    pub total_pnl_usd: i64,
    pub win_rate: f64,
    pub avg_trade_duration_secs: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_db_init_and_query() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Db::open(tmp.path()).unwrap();

        let trade = TradeRecord {
            symbol: "BTC_USDT",
            entry_ts: 1_700_000_000_000_000,
            exit_ts: 1_700_000_001_000_000,
            spot_entry: 50_000_000_000,
            perp_entry: 50_001_000_000,
            spot_exit: 50_010_000_000,
            perp_exit: 50_005_000_000,
            size_usd_raw: 1_000_000_000,
            pnl_usd_raw: 10_000_000,
            pnl_pct_raw: 100_000,
            exit_reason: "spread_closed",
        };
        db.push_trade(trade);
        db.flush();

        let trades = db.get_recent_trades(10);
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].symbol, "BTC_USDT");

        let stats = db.get_stats();

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].symbol, "BTC_USDT");

        let stats = db.get_stats();
        assert_eq!(stats.total_trades, 1);
    }

    #[test]
    fn test_ring_buffer_full() {
        let ring: RingBuffer<u32> = RingBuffer::new();
        for i in 0..RING_CAP {
            assert!(ring.push(i as u32));
        }
        // Ring full — next push returns false
        assert!(!ring.push(999));
    }

    #[test]
    fn test_ring_basic() {
        let ring: RingBuffer<u32> = RingBuffer::new();
        assert!(ring.push(42u32));
        let mut found = false;
        ring.drain(|v| {
            found = true;
            assert_eq!(v, 42);
        });
        assert!(found, "drain should have found 42");
    }
}
