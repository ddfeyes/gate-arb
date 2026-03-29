//! config.rs — env-driven configuration for gate-arb.
//!
//! All tuneable parameters are read at startup from environment variables.
//! No recompile needed to change trading pair, thresholds, or ports.
//!
//! Supported env vars (with defaults):
//!   GATE_SPOT_SYMBOL          = "BTC_USDT"
//!   GATE_PERP_SYMBOL          = "BTC_USDT"
//!   GATE_SPREAD_THRESHOLD_BPS = 50         (bps, converted to raw fixed-point)
//!   GATE_FRONTEND_PORT        = 8080
//!   GATE_HEALTH_PORT          = 8081
//!   GATE_MAX_POSITION_USD     = 1000
//!   PAPER_MODE                = true       (set "false" to enable live trading)
//!
//! Backward-compat aliases (lower priority than GATE_* names):
//!   SPOT_SYMBOL, PERP_SYMBOL, FRONTEND_PORT, HEALTH_PORT

use types::SCALE;

/// Runtime configuration parsed once at startup from env vars.
#[derive(Debug, Clone)]
pub struct Config {
    /// Spot trading pair, e.g. "BTC_USDT".
    pub spot_symbol: String,
    /// Perpetual contract name, e.g. "BTC_USDT".
    pub perp_symbol: String,
    /// Spread threshold in basis points (for logging/display).
    pub spread_threshold_bps: u64,
    /// Spread threshold in raw Fixed64 units passed to Strategy.
    /// Formula: bps * reference_price_raw / 10_000
    /// Reference price is fixed at $50 000 (BTC baseline).
    pub spread_threshold_raw: u64,
    /// WebSocket port served to the frontend.
    pub frontend_port: u16,
    /// HTTP health-check port.
    pub health_port: u16,
    /// Maximum open position size in USD (risk guard).
    pub max_position_usd: u64,
    /// Paper trading mode — no real orders are placed.
    pub paper_mode: bool,
}

impl Config {
    /// Parse all config from environment variables with hard-coded defaults.
    pub fn from_env() -> Self {
        // Symbol — prefer GATE_* prefix, fall back to legacy names
        let spot_symbol = std::env::var("GATE_SPOT_SYMBOL")
            .or_else(|_| std::env::var("SPOT_SYMBOL"))
            .unwrap_or_else(|_| "BTC_USDT".into());

        let perp_symbol = std::env::var("GATE_PERP_SYMBOL")
            .or_else(|_| std::env::var("PERP_SYMBOL"))
            .unwrap_or_else(|_| "BTC_USDT".into());

        // Spread threshold in bps (default 50 bps)
        let spread_threshold_bps: u64 = std::env::var("GATE_SPREAD_THRESHOLD_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);

        // Convert bps → raw Fixed64 units using $50 000 reference price.
        // raw = bps * ref_price_raw / 10_000
        //     = bps * (50_000 * SCALE) / 10_000
        //     = bps * SCALE * 5
        // For 50 bps: 50 * 100_000_000 * 5 = 25_000_000_000 ✓
        let spread_threshold_raw = spread_threshold_bps * SCALE * 5;

        // Ports — prefer GATE_* prefix, fall back to legacy names
        let frontend_port: u16 = std::env::var("GATE_FRONTEND_PORT")
            .or_else(|_| std::env::var("FRONTEND_PORT"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);

        let health_port: u16 = std::env::var("GATE_HEALTH_PORT")
            .or_else(|_| std::env::var("HEALTH_PORT"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8081);

        // Max position size in USD (wired to risk manager in future)
        let max_position_usd: u64 = std::env::var("GATE_MAX_POSITION_USD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);

        // Paper mode — true unless PAPER_MODE=false is set explicitly
        let paper_mode = std::env::var("PAPER_MODE")
            .map(|v| v != "false")
            .unwrap_or(true);

        Self {
            spot_symbol,
            perp_symbol,
            spread_threshold_bps,
            spread_threshold_raw,
            frontend_port,
            health_port,
            max_position_usd,
            paper_mode,
        }
    }

    /// Emit a single structured startup log line showing all effective values.
    pub fn log_effective(&self) {
        tracing::info!(
            "Config: spot={} perp={} threshold={}bps frontend_port={} health_port={} \
             max_pos_usd={} paper_mode={}",
            self.spot_symbol,
            self.perp_symbol,
            self.spread_threshold_bps,
            self.frontend_port,
            self.health_port,
            self.max_position_usd,
            self.paper_mode,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize env-var tests — env is process-global and tests run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_KEYS: &[&str] = &[
        "GATE_SPOT_SYMBOL",
        "GATE_PERP_SYMBOL",
        "GATE_SPREAD_THRESHOLD_BPS",
        "GATE_FRONTEND_PORT",
        "GATE_HEALTH_PORT",
        "GATE_MAX_POSITION_USD",
        "PAPER_MODE",
        "SPOT_SYMBOL",
        "PERP_SYMBOL",
        "FRONTEND_PORT",
        "HEALTH_PORT",
    ];

    fn clear_all() {
        for key in ALL_KEYS {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn test_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_all();
        let cfg = Config::from_env();
        assert_eq!(cfg.spot_symbol, "BTC_USDT");
        assert_eq!(cfg.perp_symbol, "BTC_USDT");
        assert_eq!(cfg.spread_threshold_bps, 50);
        assert_eq!(cfg.spread_threshold_raw, 25_000_000_000);
        assert_eq!(cfg.frontend_port, 8080);
        assert_eq!(cfg.health_port, 8081);
        assert_eq!(cfg.max_position_usd, 1000);
        assert!(cfg.paper_mode);
    }

    #[test]
    fn test_gate_env_vars() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_all();
        std::env::set_var("GATE_SPOT_SYMBOL", "ETH_USDT");
        std::env::set_var("GATE_PERP_SYMBOL", "ETH_USDT");
        std::env::set_var("GATE_SPREAD_THRESHOLD_BPS", "100");
        std::env::set_var("GATE_FRONTEND_PORT", "9090");
        std::env::set_var("GATE_HEALTH_PORT", "9091");
        std::env::set_var("GATE_MAX_POSITION_USD", "5000");
        std::env::set_var("PAPER_MODE", "false");

        let cfg = Config::from_env();
        assert_eq!(cfg.spot_symbol, "ETH_USDT");
        assert_eq!(cfg.perp_symbol, "ETH_USDT");
        assert_eq!(cfg.spread_threshold_bps, 100);
        assert_eq!(cfg.spread_threshold_raw, 50_000_000_000); // 100 * SCALE * 5
        assert_eq!(cfg.frontend_port, 9090);
        assert_eq!(cfg.health_port, 9091);
        assert_eq!(cfg.max_position_usd, 5000);
        assert!(!cfg.paper_mode);
        clear_all();
    }

    #[test]
    fn test_legacy_fallback() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_all();
        std::env::set_var("SPOT_SYMBOL", "SOL_USDT");
        std::env::set_var("PERP_SYMBOL", "SOL_USDT");

        let cfg = Config::from_env();
        assert_eq!(cfg.spot_symbol, "SOL_USDT");
        assert_eq!(cfg.perp_symbol, "SOL_USDT");
        clear_all();
    }

    #[test]
    fn test_bps_to_raw_conversion() {
        // 50 bps on BTC@50k = $250 spread = 250 * SCALE raw = 25_000_000_000
        assert_eq!(50 * SCALE * 5, 25_000_000_000);
        // 10 bps → 5_000_000_000
        assert_eq!(10 * SCALE * 5, 5_000_000_000);
        // 100 bps → 50_000_000_000
        assert_eq!(100 * SCALE * 5, 50_000_000_000);
    }
}
