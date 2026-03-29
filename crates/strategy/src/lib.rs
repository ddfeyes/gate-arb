//! strategy — spread detection + trade signal emission.
//!
//! Paper-trading first: logs signals, does not execute.
//! Thread 2 (warm): position management, risk, funding monitor.

use db::{DbWriter, SpreadRecord, TradeRecord};
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{info, warn};

use engine::Engine;
use risk::RiskManager;
use types::{ArbitragePosition, Leg, LegKind, SpreadSignal, TradeState, SCALE};

const LEG2_TIMEOUT_US: u64 = 500_000; // 500ms

pub struct Strategy {
    pub engine: Arc<Engine<20, 20>>,
    pub paper_mode: bool,
    pub threshold_spread_raw: u64,
    /// Current active position (if any).
    position: RwLock<Option<ArbitragePosition>>,
    /// Cumulative P&L in raw units.
    cumulative_pnl: RwLock<i64>,
    /// Optional DB writer for trade persistence.
    db: Option<DbWriter>,
    /// Optional risk manager for position safety.
    risk: Option<Arc<RiskManager>>,
}

impl Strategy {
    pub fn new(engine: Arc<Engine<20, 20>>, threshold_spread_bps: u64) -> Self {
        Self {
            engine,
            paper_mode: true,
            threshold_spread_raw: threshold_spread_bps,
            position: RwLock::new(None),
            cumulative_pnl: RwLock::new(0),
            db: None,
            risk: None,
        }
    }

    /// Set the DB writer for trade persistence.
    pub fn set_db(&mut self, db: DbWriter) {
        self.db = Some(db);
    }

    /// Set the risk manager for position safety checks.
    pub fn set_risk(&mut self, risk: Arc<RiskManager>) {
        self.risk = Some(risk);
    }

    /// Called every tick from the hot path — check spread, emit signals.
    #[inline(always)]
    pub fn on_tick(&self) {
        let signal = self.engine.check_spread(self.threshold_spread_raw);

        if let Some(sig) = signal {
            if self.position.read().is_none() {
                info!(
                    "SPREAD SIGNAL: raw={} pct={} bid={} ask={}",
                    sig.spread_raw, sig.spread_pct, sig.bid_price, sig.ask_price
                );

                // Log spread snapshot for analysis
                if let Some(ref db) = self.db {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as i64;
                    db.write_spread(SpreadRecord {
                        ts: now,
                        spread_bps: sig.spread_raw as i64,
                        bid_price: sig.bid_price.raw() as i64,
                        ask_price: sig.ask_price.raw() as i64,
                    });
                }

                if !self.paper_mode {
                    // Risk check before opening
                    let can_open = self
                        .risk
                        .as_ref()
                        .map(|r| r.can_open_position())
                        .unwrap_or(true);
                    if !can_open {
                        info!("risk: blocked new position entry");
                    } else {
                        self.open_position(sig);
                        if let Some(ref risk) = self.risk {
                            risk.position_opened();
                        }
                    }
                }
            }
        }

        // Check position timeouts
        self.check_timeouts();
    }

    fn open_position(&self, sig: SpreadSignal) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        let leg1 = Leg {
            kind: LegKind::SpotBuy,
            price: sig.bid_price,
            qty: 1_000_000, // 0.01 BTC
            state: TradeState::Leg1Sent,
            sent_at_us: Some(now),
            filled_at_us: None,
        };

        let leg2 = Leg {
            kind: LegKind::PerpShort,
            price: sig.ask_price,
            qty: 1_000_000,
            state: TradeState::Idle,
            sent_at_us: None,
            filled_at_us: None,
        };

        *self.position.write() = Some(ArbitragePosition::new(leg1, leg2, now));
    }

    fn check_timeouts(&self) {
        let mut pos = self.position.write();
        if let Some(ref mut p) = *pos {
            if p.state == TradeState::Leg1Filled {
                if let Some(leg1_filled) = p.legs[0].filled_at_us {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64;
                    if now.saturating_sub(leg1_filled) > LEG2_TIMEOUT_US {
                        warn!("LEG2 timeout — emergency close LEG1");
                        p.state = TradeState::Closing;
                    }
                }
            }
        }
    }

    /// Record a completed trade to the DB. Called when both legs are closed.
    ///
    /// `exit_reason`: "spread_collapsed" | "timeout" | "manual" | "emergency"
    pub fn record_trade(
        &self,
        symbol: &str,
        entry_ts: i64,
        exit_ts: i64,
        spot_entry: i64,
        perp_entry: i64,
        spot_exit: i64,
        perp_exit: i64,
        size_usd: i64,
        pnl_usd: i64,
        exit_reason: &str,
    ) {
        // Compute pnl_pct in basis points * 100 (i.e. 10000 = 100%)
        let pnl_pct = if size_usd > 0 {
            (pnl_usd * 100_000_000 * 100) / size_usd
        } else {
            0
        };

        if let Some(ref db) = self.db {
            db.write_trade(TradeRecord {
                symbol: symbol.to_string(),
                entry_ts,
                exit_ts,
                spot_entry,
                perp_entry,
                spot_exit,
                perp_exit,
                size_usd,
                pnl_usd,
                pnl_pct,
                exit_reason: exit_reason.to_string(),
            });
        }

        // Update cumulative pnl for in-memory tracking
        *self.cumulative_pnl.write() += pnl_usd;

        // Update risk manager
        if let Some(ref risk) = self.risk {
            risk.update_pnl(pnl_usd);
            risk.position_closed();
        }

        info!(
            "TRADE CLOSED: {} pnl={} exit_reason={} total_pnl={}",
            symbol,
            pnl_usd as f64 / SCALE as f64,
            exit_reason,
            *self.cumulative_pnl.read() as f64 / SCALE as f64,
        );
    }

    pub fn get_pnl(&self) -> i64 {
        *self.cumulative_pnl.read()
    }
}
