//! gate-arb — HFT spot-perp & funding arbitrage engine.
//!
//! Usage: cargo run --release --bin gate-arb
//!
//! Paper trading by default. Set PAPER_MODE=false to enable live trading.

use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

/// Resolved runtime config — symbols come from env vars with sensible defaults.
#[derive(Debug, Clone)]
pub struct Args {
    pub spot_symbol: String,
    pub perp_symbol: String,
    pub frontend_port: u16,
    pub gate_ws_url: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            spot_symbol: std::env::var("SPOT_SYMBOL").unwrap_or_else(|_| "BTC_USDT".into()),
            perp_symbol: std::env::var("PERP_SYMBOL").unwrap_or_else(|_| "BTC_USDT".into()),
            frontend_port: std::env::var("FRONTEND_PORT")
                .unwrap_or_else(|_| "8080".into())
                .parse()
                .unwrap_or(8080),
            gate_ws_url: std::env::var("GATE_WS_URL")
                .unwrap_or_else(|_| "wss://api.gateio.ws/ws/v4/".into()),
        }
    }
}

impl Args {
    pub fn resolve() -> Self {
        Self::default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .compact()
        .init();

    info!("gate-arb starting...");
    info!(
        "Paper mode: {}",
        std::env::var("PAPER_MODE").unwrap_or_else(|_| "true".into())
    );

    // --- DB init ---
    let db_path = std::path::PathBuf::from("gate-arb.db");
    let db = Arc::new(db::Db::open(&db_path)?);

    // Background flush task — drains rings every 5s (cold path)
    let db_flush = Arc::clone(&db);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            db_flush.flush();
        }
    });

    // Resolve config from environment
    let cfg = Args::resolve();
    info!(
        "Config — spot={} perp={} frontend_port={} gate_ws={}",
        cfg.spot_symbol, cfg.perp_symbol, cfg.frontend_port, cfg.gate_ws_url
    );

    // Build engine
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    let threshold_spread_raw: u64 = 25_000_000_000; // 50bps on BTC @ 50k
    let _strategy =
        strategy::Strategy::new(Arc::clone(&engine), threshold_spread_raw).with_db(Arc::clone(&db));

    // Build frontend broadcaster + HTTP API
    let frontend = Arc::new(frontend_ws::FrontendWs::new().with_db(Arc::clone(&db)));

    // Start frontend WS + HTTP API servers
    let fe = Arc::clone(&frontend);
    tokio::spawn(async move {
        if let Err(e) = fe.run(cfg.frontend_port).await {
            tracing::error!("Frontend WS error: {:?}", e);
        }
    });

    // Connect to Gate.io and run hot path
    let gw = gateway_ws::GatewayWs::new();
    info!("Connecting to {}", cfg.gate_ws_url);
    gw.run(&cfg.spot_symbol, &cfg.perp_symbol).await?;

    Ok(())
}
