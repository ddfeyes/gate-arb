//! funding.rs — Funding rate arbitrage strategy.
//!
//! Strategy: short perp + buy spot **before** the funding snapshot,
//! collect positive funding payment, exit after snapshot.
//!
//! Gate.io pays funding every 8 hours (00:00, 08:00, 16:00 UTC).
//! We enter when: rate > ENTRY_THRESHOLD_BPS AND time < ENTRY_WINDOW_SECS before snapshot.
//! We exit when: snapshot has passed (funding collected).
//!
//! Thread 2 (warm): called from warm path, not hot path.

use parking_lot::RwLock;
use tracing::{info, warn};

use types::{FundingRateUpdate, SCALE};

/// Enter position this many seconds before funding snapshot.
const ENTRY_WINDOW_SECS: u64 = 300; // 5 minutes before

/// Minimum funding rate in bps to trigger entry.
/// 0.5 bps = 0.005% per cycle (aggressive; raise to 1.0 in production).
const ENTRY_THRESHOLD_BPS: f64 = 0.5;

/// Maximum hold time after snapshot (exit deadline).
const MAX_HOLD_AFTER_SNAPSHOT_SECS: u64 = 120; // 2 minutes

/// Gate.io funding interval: every 8 hours.
const FUNDING_INTERVAL_SECS: u64 = 8 * 3600;

/// Funding state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingState {
    /// No position, waiting for next funding opportunity.
    Idle,
    /// Entered position, waiting for funding snapshot.
    Entered,
    /// Snapshot passed, holding briefly to confirm collection.
    Collecting,
    /// Exiting position (paper: instant).
    Exiting,
    /// Trade complete.
    Done,
}

/// Statistics for the funding strategy.
#[derive(Debug, Clone, Default)]
pub struct FundingStats {
    pub total_cycles: u64,
    pub profitable_cycles: u64,
    pub total_funding_collected_raw: i64,
    pub last_rate_bps: f64,
    pub last_entry_at_ms: Option<u64>,
    pub last_exit_at_ms: Option<u64>,
}

/// One active funding arb position (paper or live).
#[derive(Debug, Clone)]
struct FundingPosition {
    /// Funding rate at entry (rate_raw).
    entry_rate_raw: i64,
    /// Expected snapshot timestamp in ms.
    snapshot_at_ms: u64,
    /// Entry timestamp ms.
    entered_at_ms: u64,
    /// Spot buy price at entry (raw).
    spot_entry_price: u64,
    /// Perp short price at entry (raw).
    perp_entry_price: u64,
    /// State.
    state: FundingState,
}

/// Funding arb strategy — lives on warm thread.
pub struct FundingStrategy {
    pub paper_mode: bool,
    pub entry_threshold_bps: f64,
    /// Latest funding rate from WS.
    latest_rate: RwLock<Option<FundingRateUpdate>>,
    /// Active position (if any).
    position: RwLock<Option<FundingPosition>>,
    /// Cumulative stats.
    stats: RwLock<FundingStats>,
}

impl FundingStrategy {
    pub fn new(paper_mode: bool) -> Self {
        Self {
            paper_mode,
            entry_threshold_bps: ENTRY_THRESHOLD_BPS,
            latest_rate: RwLock::new(None),
            position: RwLock::new(None),
            stats: RwLock::new(FundingStats::default()),
        }
    }

    /// Update the latest funding rate — called when WS delivers a funding_rate update.
    pub fn on_funding_rate(&self, update: FundingRateUpdate) {
        let bps = update.rate_bps();
        info!(
            "FUNDING RATE: contract={} rate={:.4}% ({:.2} bps) ts={}",
            update.contract,
            update.rate_f64() * 100.0,
            bps,
            update.timestamp_ms,
        );
        {
            let mut stats = self.stats.write();
            stats.last_rate_bps = bps;
        }
        *self.latest_rate.write() = Some(update);
    }

    /// Tick from warm thread — decides entry/exit.
    ///
    /// `spot_price` and `perp_price` are current best prices in raw units (Fixed64.raw()).
    pub fn on_warm_tick(&self, now_ms: u64, spot_price: Option<u64>, perp_price: Option<u64>) {
        // Check exit first
        self.check_exit(now_ms, spot_price, perp_price);
        // Then check entry
        self.check_entry(now_ms, spot_price, perp_price);
    }

    fn next_snapshot_ms(now_ms: u64) -> u64 {
        // Gate.io snapshots at 00:00, 08:00, 16:00 UTC.
        let now_secs = now_ms / 1_000;
        let secs_in_day = now_secs % (24 * 3600);
        let next_boundary_in_day =
            ((secs_in_day / FUNDING_INTERVAL_SECS) + 1) * FUNDING_INTERVAL_SECS;
        let secs_until = if next_boundary_in_day < 24 * 3600 {
            next_boundary_in_day - secs_in_day
        } else {
            // Next day
            24 * 3600 - secs_in_day
        };
        now_ms + secs_until * 1_000
    }

    fn check_entry(&self, now_ms: u64, spot_price: Option<u64>, perp_price: Option<u64>) {
        // Don't enter if position already open
        if self.position.read().is_some() {
            return;
        }

        let rate = match &*self.latest_rate.read() {
            Some(r) => r.clone(),
            None => return,
        };

        // Only enter for positive funding (longs pay shorts → we collect as short)
        if rate.rate_bps() < self.entry_threshold_bps {
            return;
        }

        let snapshot_ms = Self::next_snapshot_ms(now_ms);
        let secs_until = snapshot_ms.saturating_sub(now_ms) / 1_000;

        if secs_until > ENTRY_WINDOW_SECS {
            // Too early to enter
            return;
        }

        if secs_until == 0 {
            // Snapshot imminent or passed — skip this cycle
            return;
        }

        let spot_px = match spot_price {
            Some(p) => p,
            None => {
                warn!("FUNDING: no spot price, skipping entry");
                return;
            }
        };
        let perp_px = match perp_price {
            Some(p) => p,
            None => {
                warn!("FUNDING: no perp price, skipping entry");
                return;
            }
        };

        let pos = FundingPosition {
            entry_rate_raw: rate.rate_raw,
            snapshot_at_ms: snapshot_ms,
            entered_at_ms: now_ms,
            spot_entry_price: spot_px,
            perp_entry_price: perp_px,
            state: FundingState::Entered,
        };

        info!(
            "FUNDING ENTRY [{}]: rate={:.2}bps spot@{:.2} perp@{:.2} snapshot_in={}s",
            if self.paper_mode { "PAPER" } else { "LIVE" },
            rate.rate_bps(),
            spot_px as f64 / SCALE as f64,
            perp_px as f64 / SCALE as f64,
            secs_until
        );

        {
            let mut stats = self.stats.write();
            stats.last_entry_at_ms = Some(now_ms);
        }
        *self.position.write() = Some(pos);
    }

    fn check_exit(&self, now_ms: u64, spot_price: Option<u64>, perp_price: Option<u64>) {
        let mut pos_guard = self.position.write();
        let pos = match pos_guard.as_mut() {
            Some(p) => p,
            None => return,
        };

        match pos.state {
            FundingState::Entered => {
                // Move to Collecting once snapshot passes
                if now_ms >= pos.snapshot_at_ms {
                    info!("FUNDING: snapshot passed — moving to Collecting");
                    pos.state = FundingState::Collecting;
                }
            }
            FundingState::Collecting => {
                // Exit 2 minutes after snapshot
                let hold_ms = now_ms.saturating_sub(pos.snapshot_at_ms);
                if hold_ms >= MAX_HOLD_AFTER_SNAPSHOT_SECS * 1_000 {
                    pos.state = FundingState::Exiting;
                    // Perform paper close
                    let spot_exit = spot_price.unwrap_or(pos.spot_entry_price);
                    let perp_exit = perp_price.unwrap_or(pos.perp_entry_price);
                    self.paper_close(pos, now_ms, spot_exit, perp_exit);
                    *pos_guard = None;
                }
            }
            FundingState::Exiting | FundingState::Done | FundingState::Idle => {}
        }
    }

    fn paper_close(&self, pos: &FundingPosition, now_ms: u64, spot_exit: u64, perp_exit: u64) {
        // P&L from price movement
        // Spot: bought at entry, sell at exit → spot_exit - spot_entry
        // Perp: shorted at entry, buy back at exit → perp_entry - perp_exit
        let qty: i64 = 1_000_000; // 0.01 BTC in 1e8 units

        let spot_pnl = (spot_exit as i64 - pos.spot_entry_price as i64) * qty / SCALE as i64;
        let perp_pnl = (pos.perp_entry_price as i64 - perp_exit as i64) * qty / SCALE as i64;

        // Funding collected: rate * notional = rate_raw * qty / SCALE
        // funding_raw is in raw-rate × qty units
        let funding_raw = pos.entry_rate_raw * qty / SCALE as i64;

        // Maker fee 2 × 0.02% per leg (open + close = 4 legs total)
        const MAKER_FEE_BPS: u64 = 2;
        let notional = (pos.spot_entry_price as i128 + pos.perp_entry_price as i128) * qty as i128;
        let fee_raw =
            ((notional * (MAKER_FEE_BPS * 4) as i128 / 10_000i128) / SCALE as i128) as i64;

        let total_pnl = spot_pnl + perp_pnl + funding_raw - fee_raw;
        let hold_secs = now_ms.saturating_sub(pos.entered_at_ms) / 1_000;

        info!(
            "FUNDING CLOSE [PAPER]: spot_pnl={:.6} perp_pnl={:.6} funding={:.6} fee={:.6} total={:.6} hold={}s",
            spot_pnl as f64 / SCALE as f64,
            perp_pnl as f64 / SCALE as f64,
            funding_raw as f64 / SCALE as f64,
            fee_raw as f64 / SCALE as f64,
            total_pnl as f64 / SCALE as f64,
            hold_secs
        );

        let mut stats = self.stats.write();
        stats.total_cycles += 1;
        if total_pnl >= 0 {
            stats.profitable_cycles += 1;
        }
        stats.total_funding_collected_raw += funding_raw;
        stats.last_exit_at_ms = Some(now_ms);
    }

    /// Get current funding stats snapshot.
    pub fn get_stats(&self) -> FundingStats {
        self.stats.read().clone()
    }

    /// Get current position state, if any.
    pub fn position_state(&self) -> Option<FundingState> {
        self.position.read().as_ref().map(|p| p.state)
    }

    /// Whether a funding position is currently open.
    pub fn is_position_open(&self) -> bool {
        self.position.read().is_some()
    }

    /// Latest known funding rate in bps (0.0 if unknown).
    pub fn latest_rate_bps(&self) -> f64 {
        self.latest_rate
            .read()
            .as_ref()
            .map(|r| r.rate_bps())
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::SCALE;

    fn make_rate(bps: f64, now_ms: u64) -> FundingRateUpdate {
        FundingRateUpdate {
            contract: "BTC_USDT".into(),
            rate_raw: (bps / 10_000.0 * SCALE as f64) as i64,
            timestamp_ms: now_ms,
        }
    }

    #[test]
    fn test_rate_bps() {
        let r = make_rate(1.0, 0);
        assert!((r.rate_bps() - 1.0).abs() < 0.001, "bps should be 1.0");
    }

    #[test]
    fn test_next_snapshot_ms() {
        // 00:00 UTC → next snapshot is 08:00 = 8*3600*1000 ms
        let now = 0u64; // midnight UTC
        let next = FundingStrategy::next_snapshot_ms(now);
        assert_eq!(
            next,
            8 * 3600 * 1_000,
            "first snapshot after midnight = 08:00"
        );
    }

    #[test]
    fn test_no_entry_below_threshold() {
        let strat = FundingStrategy::new(true);
        // Rate = 0.1 bps (below 0.5 threshold)
        let rate = make_rate(0.1, 0);
        strat.on_funding_rate(rate);
        // 4 minutes before snapshot (within entry window)
        let snapshot = FundingStrategy::next_snapshot_ms(0);
        let now = snapshot - 4 * 60 * 1_000;
        strat.on_warm_tick(now, Some(5_000_000_000_000), Some(5_000_100_000_000));
        assert!(
            !strat.is_position_open(),
            "should not enter below threshold"
        );
    }

    #[test]
    fn test_entry_within_window() {
        let strat = FundingStrategy::new(true);
        // Rate = 2.0 bps (above threshold)
        let now_ms = 0u64;
        let rate = make_rate(2.0, now_ms);
        strat.on_funding_rate(rate);
        // 3 minutes before snapshot (within 5-minute entry window)
        let snapshot = FundingStrategy::next_snapshot_ms(now_ms);
        let entry_ms = snapshot - 3 * 60 * 1_000;
        let spot = 5_000_000_000_000u64; // $50k
        let perp = 5_001_000_000_000u64; // $50k + $10 premium
        strat.on_warm_tick(entry_ms, Some(spot), Some(perp));
        assert!(
            strat.is_position_open(),
            "should enter with 2bps rate in window"
        );
    }

    #[test]
    fn test_entry_too_early() {
        let strat = FundingStrategy::new(true);
        let now_ms = 0u64;
        let rate = make_rate(2.0, now_ms);
        strat.on_funding_rate(rate);
        // 10 minutes before snapshot — outside 5-minute window
        let snapshot = FundingStrategy::next_snapshot_ms(now_ms);
        let entry_ms = snapshot - 10 * 60 * 1_000;
        strat.on_warm_tick(entry_ms, Some(5_000_000_000_000), Some(5_001_000_000_000));
        assert!(!strat.is_position_open(), "should not enter too early");
    }

    #[test]
    fn test_full_cycle() {
        let strat = FundingStrategy::new(true);
        let now_ms = 0u64;
        let rate = make_rate(5.0, now_ms); // 5 bps — positive & above threshold
        strat.on_funding_rate(rate);

        let snapshot = FundingStrategy::next_snapshot_ms(now_ms);
        // Enter 3 minutes before snapshot
        let entry_ms = snapshot - 3 * 60 * 1_000;
        strat.on_warm_tick(entry_ms, Some(5_000_000_000_000), Some(5_001_000_000_000));
        assert!(
            strat.is_position_open(),
            "position should be open after entry"
        );
        assert_eq!(strat.position_state(), Some(FundingState::Entered));

        // Advance past snapshot
        let post_snap = snapshot + 1_000;
        strat.on_warm_tick(post_snap, Some(5_000_000_000_000), Some(5_001_000_000_000));
        assert_eq!(strat.position_state(), Some(FundingState::Collecting));

        // Advance past max hold (2 minutes after snapshot)
        let exit_ms = snapshot + MAX_HOLD_AFTER_SNAPSHOT_SECS * 1_000 + 1_000;
        strat.on_warm_tick(exit_ms, Some(5_000_000_000_000), Some(5_001_000_000_000));
        assert!(
            !strat.is_position_open(),
            "position should be closed after exit"
        );

        let stats = strat.get_stats();
        assert_eq!(stats.total_cycles, 1, "should have completed 1 cycle");
    }
}
