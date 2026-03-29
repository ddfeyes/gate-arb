//! risk — position safety, drawdown limits, kill switch, and WS latency monitoring.
//!
//! Thread 2 (warm): risk checks before every new position.
//!
//! Paper-trading aware: kill switch and latency monitoring are always active;
//! drawdown/position limits are advisory in paper mode (log-only).

use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{info, warn};

/// Risk configuration — read from environment variables.
#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Max drawdown in USD (raw units, 1e8 = $1). Default: 10 USD = 1_000_000_000.
    pub max_drawdown_raw: i64,
    /// Max simultaneous positions. Default: 3.
    pub max_positions: usize,
    /// WS ping latency threshold in ms. Default: 100.
    pub ping_threshold_ms: u64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_drawdown_raw: 10_000_000_000, // $10 in 1e8 units
            max_positions: 3,
            ping_threshold_ms: 100,
        }
    }
}

impl RiskConfig {
    pub fn from_env() -> Self {
        Self {
            max_drawdown_raw: std::env::var("GATE_MAX_DRAWDOWN_USD")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| (v * 1e8_f64) as i64)
                .unwrap_or_default(),

            max_positions: std::env::var("GATE_MAX_POSITIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),

            ping_threshold_ms: std::env::var("GATE_PING_THRESHOLD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
        }
    }
}

/// Result of a risk check — always checks all conditions and returns ALL violations.
#[derive(Debug, Default)]
pub struct RiskViolations {
    pub drawdown_exceeded: bool,
    pub max_positions_exceeded: bool,
    pub high_latency: bool,
    pub kill_switch_active: bool,
}

impl RiskViolations {
    pub fn is_ok(&self) -> bool {
        !self.drawdown_exceeded
            && !self.max_positions_exceeded
            && !self.high_latency
            && !self.kill_switch_active
    }

    pub fn reason(&self) -> Option<&'static str> {
        if self.kill_switch_active {
            Some("kill_switch")
        } else if self.high_latency {
            Some("high_latency")
        } else if self.max_positions_exceeded {
            Some("max_positions")
        } else if self.drawdown_exceeded {
            Some("drawdown_exceeded")
        } else {
            None
        }
    }
}

/// Central risk manager — shared across engine, strategy, and gateway.
/// Thread-safe: uses atomics +RwLock for counters.
pub struct RiskManager {
    /// Kill switch — if set, no new positions can be opened.
    kill_switch: AtomicBool,
    /// Current number of open positions.
    position_count: AtomicU64,
    /// Peak cumulative P&L (high-water mark in raw units).
    peak_pnl: RwLock<i64>,
    /// Current cumulative P&L from strategy.
    cumulative_pnl: RwLock<i64>,
    /// Most recent WS ping latency in ms.
    last_ping_ms: AtomicU64,
    /// Config snapshot.
    config: RiskConfig,
    /// True if paper trading mode.
    paper_mode: AtomicBool,
}

impl Default for RiskManager {
    fn default() -> Self {
        Self::new(RiskConfig::default())
    }
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        info!(
            "risk: max_drawdown=${:.2} max_positions={} ping_threshold={}ms",
            config.max_drawdown_raw as f64 / 1e8,
            config.max_positions,
            config.ping_threshold_ms,
        );
        Self {
            kill_switch: AtomicBool::new(false),
            position_count: AtomicU64::new(0),
            peak_pnl: RwLock::new(0),
            cumulative_pnl: RwLock::new(0),
            last_ping_ms: AtomicU64::new(0),
            config,
            paper_mode: AtomicBool::new(true),
        }
    }

    /// Set paper mode. In paper mode, drawdown/position violations are logged but do not block.
    pub fn set_paper_mode(&self, paper: bool) {
        self.paper_mode.store(paper, Ordering::Relaxed);
        info!("risk: paper_mode={}", paper);
    }

    /// Update the current cumulative P&L and peak tracker.
    /// Called after each trade closes.
    pub fn update_pnl(&self, realized_pnl: i64) {
        let mut current = self.cumulative_pnl.write();
        *current += realized_pnl;
        let pnl = *current;

        let mut peak = self.peak_pnl.write();
        if pnl > *peak {
            *peak = pnl;
        }
    }

    /// Full P&L sync — called periodically or on state recovery to reconcile peak tracking.
    pub fn sync_cumulative_pnl(&self, pnl: i64) {
        let mut current = self.cumulative_pnl.write();
        *current = pnl;
        let mut peak = self.peak_pnl.write();
        if pnl > *peak {
            *peak = pnl;
        }
    }

    /// Record that a new position was opened.
    pub fn position_opened(&self) {
        let prev = self.position_count.fetch_add(1, Ordering::Relaxed);
        info!("risk: position opened (count={})", prev + 1);
    }

    /// Record that a position was closed.
    pub fn position_closed(&self) {
        let prev = self.position_count.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            info!("risk: position closed (count={})", prev - 1);
        }
    }

    /// Update the latest observed WS ping latency in ms.
    pub fn update_ping_latency(&self, latency_ms: u64) {
        self.last_ping_ms.store(latency_ms, Ordering::Relaxed);
    }

    /// Activate the kill switch — stops all new position entries immediately.
    /// Returns the previous state.
    pub fn activate_kill_switch(&self) -> bool {
        let prev = self.kill_switch.swap(true, Ordering::Relaxed);
        warn!("risk: KILL SWITCH ACTIVATED (was={})", prev);
        prev
    }

    /// Deactivate the kill switch (manual reset required).
    pub fn deactivate_kill_switch(&self) {
        self.kill_switch.store(false, Ordering::Relaxed);
        info!("risk: kill switch deactivated");
    }

    /// Returns true if the kill switch is currently active.
    pub fn is_kill_switch_active(&self) -> bool {
        self.kill_switch.load(Ordering::Relaxed)
    }

    /// Returns the current drawdown in raw units (positive = loss from peak).
    pub fn current_drawdown_raw(&self) -> i64 {
        let peak = *self.peak_pnl.read();
        let current = *self.cumulative_pnl.read();
        (peak - current).max(0)
    }

    /// Check all risk conditions and return violations.
    /// In paper mode, drawdown and position violations are logged but do not block.
    pub fn check(&self) -> RiskViolations {
        let kill_switch_active = self.kill_switch.load(Ordering::Relaxed);
        let high_latency =
            self.last_ping_ms.load(Ordering::Relaxed) > self.config.ping_threshold_ms;
        let positions = self.position_count.load(Ordering::Relaxed) as usize;
        let max_positions_exceeded = positions >= self.config.max_positions;

        let drawdown_exceeded = self.current_drawdown_raw() > self.config.max_drawdown_raw;

        let v = RiskViolations {
            drawdown_exceeded,
            max_positions_exceeded,
            high_latency,
            kill_switch_active,
        };

        if !v.is_ok() {
            let reason = v.reason().unwrap_or("unknown");
            if self.paper_mode.load(Ordering::Relaxed) && !kill_switch_active && !high_latency {
                // In paper mode, log but don't block on drawdown/position limits
                warn!("risk: violation detected (paper-mode log-only): {}", reason);
            } else {
                warn!("risk: VIOLATION — blocked: {}", reason);
            }
        }

        v
    }

    /// Returns true if a new position can be opened given current risk state.
    /// In paper mode, this always returns true (risk violations are advisory only).
    pub fn can_open_position(&self) -> bool {
        if self.paper_mode.load(Ordering::Relaxed) {
            // Paper mode: risk checks are advisory
            return true;
        }
        self.check().is_ok()
    }

    /// Human-readable risk status for health endpoint / logs.
    pub fn status(&self) -> RiskStatus {
        RiskStatus {
            kill_switch: self.kill_switch.load(Ordering::Relaxed),
            position_count: self.position_count.load(Ordering::Relaxed) as usize,
            max_positions: self.config.max_positions,
            drawdown_raw: self.current_drawdown_raw(),
            max_drawdown_raw: self.config.max_drawdown_raw,
            last_ping_ms: self.last_ping_ms.load(Ordering::Relaxed),
            ping_threshold_ms: self.config.ping_threshold_ms,
            cumulative_pnl: *self.cumulative_pnl.read(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RiskStatus {
    pub kill_switch: bool,
    pub position_count: usize,
    pub max_positions: usize,
    pub drawdown_raw: i64,
    pub max_drawdown_raw: i64,
    pub last_ping_ms: u64,
    pub ping_threshold_ms: u64,
    pub cumulative_pnl: i64,
}
