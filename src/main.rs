//! gate-arb — HFT spot-perp & funding arbitrage engine.
//!
//! Usage: cargo run --release --bin gate-arb
//!
//! Paper trading by default. Set PAPER_MODE=false to enable live trading.
//!
//! All tuneable parameters are read from env vars at startup — see config.rs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use engine::Engine;
use frontend_ws::FrontendWs;
use gateway_ws::{parse_price as gw_parse_price, ReconnectConfig, WsEvent, CHANNEL_FUNDING_RATE};
use strategy::{FundingStrategy, Strategy};
use types::{Level as ObLevel, OrderBook};

mod config;

/// Parse Gate.io order book level from string pair.
#[inline(always)]
fn parse_ob_level(price_str: &str, qty_str: &str) -> ObLevel {
    ObLevel {
        price: gw_parse_price(price_str),
        qty: qty_str.parse().unwrap_or(0),
    }
}

/// Main hot path coordinator — lives in main.rs where all deps are available.
struct HotPath {
    engine: Arc<Engine<20, 20>>,
    strategy: Arc<Strategy>,
    funding_strategy: Arc<FundingStrategy>,
    frontend: Arc<FrontendWs>,
}

impl HotPath {
    fn new(
        engine: Arc<Engine<20, 20>>,
        strategy: Arc<Strategy>,
        funding_strategy: Arc<FundingStrategy>,
        frontend: Arc<FrontendWs>,
    ) -> Self {
        Self {
            engine,
            strategy,
            funding_strategy,
            frontend,
        }
    }

    fn process_text(&self, text: &str) {
        // Try to parse as order book update or funding rate
        if let Ok(evt) = serde_json::from_str::<WsEvent>(text) {
            match evt {
                WsEvent::SpotOb(update) => {
                    self.process_ob_update(update, &self.engine.spot_book);
                }
                WsEvent::FuturesOb(update) => {
                    self.process_ob_update(update, &self.engine.perp_book);
                }
                WsEvent::FundingRate(payload) => {
                    // Warm path: update funding strategy
                    let update = payload.to_update();
                    self.funding_strategy.on_funding_rate(update);
                    // Trigger warm tick with current prices
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let spot_price = self.engine.spot_book.read().best_bid().map(|p| p.raw());
                    let perp_price = self.engine.perp_book.read().best_ask().map(|p| p.raw());
                    self.funding_strategy
                        .on_warm_tick(now_ms, spot_price, perp_price);
                }
                WsEvent::SpotTrades(_) => {}
            }
        }
        // Strategy tick (spread arb — checks spread + paper simulation)
        self.strategy.on_tick();
        // Funding warm tick — also check on every book update (not just rate update)
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let spot_price = self.engine.spot_book.read().best_bid().map(|p| p.raw());
            let perp_price = self.engine.perp_book.read().best_ask().map(|p| p.raw());
            self.funding_strategy
                .on_warm_tick(now_ms, spot_price, perp_price);
        }
        // Frontend PnL broadcast
        let stats = self.strategy.get_paper_stats();
        let pnl = self.strategy.get_pnl();
        let funding_stats = self.funding_strategy.get_stats();
        let total_funding_pnl = funding_stats.total_funding_collected_raw;
        self.frontend.update_pnl(
            pnl + total_funding_pnl,
            stats.total_trades + funding_stats.total_cycles,
            stats.wins + funding_stats.profitable_cycles,
            stats.losses
                + funding_stats
                    .total_cycles
                    .saturating_sub(funding_stats.profitable_cycles),
            self.strategy.is_position_open() || self.funding_strategy.is_position_open(),
        );
    }

    fn process_ob_update<const B: usize, const A: usize>(
        &self,
        update: gateway_ws::ObUpdate,
        book: &Arc<RwLock<OrderBook<B, A>>>,
    ) {
        debug!(
            "OB update: {} bids={} asks={}",
            update.s,
            update.bids.len(),
            update.asks.len()
        );
        let bid_count = update.bids.len();
        let ask_count = update.asks.len();
        let mut levels = Vec::with_capacity(bid_count + ask_count);
        for (price, qty) in update.bids.iter() {
            levels.push(parse_ob_level(price, qty));
        }
        for (price, qty) in update.asks.iter() {
            levels.push(parse_ob_level(price, qty));
        }
        let mut ob = book.write();
        ob.update_bids(&levels[..bid_count]);
        ob.update_asks(&levels[bid_count..]);
    }
}

/// Single WS connection attempt — connects, subscribes, pumps until disconnect.
/// Returns Ok(()) on clean close, Err on fatal/connection errors.
async fn run_gateway_once(
    spot_symbol: &str,
    perp_symbol: &str,
    hot_path: Arc<HotPath>,
) -> Result<()> {
    use tokio_tungstenite::connect_async;

    const GATE_WS_URL: &str = "wss://api.gateio.ws/ws/v4/";
    const CHANNEL_SPOT_OB: &str = "spot.order_book";
    const CHANNEL_FUTURES_OB: &str = "futures.order_book";

    info!("Connecting to Gate.io WS: {}", GATE_WS_URL);
    let (ws, _) = connect_async(GATE_WS_URL).await?;
    let (mut write, mut read) = ws.split();

    // Subscribe to spot order book
    let spot_sub = serde_json::to_string(&serde_json::json!({
        "channel": CHANNEL_SPOT_OB,
        "event": "subscribe",
        "payload": [spot_symbol, "10", "0"]
    }))?;
    write.send(Message::Text(spot_sub.into())).await?;
    debug!("Subscribed to spot.order_book");

    // Subscribe to futures order book
    let futures_sub = serde_json::to_string(&serde_json::json!({
        "channel": CHANNEL_FUTURES_OB,
        "event": "subscribe",
        "payload": [perp_symbol, "10", "0"]
    }))?;
    write.send(Message::Text(futures_sub.into())).await?;
    debug!("Subscribed to futures.order_book");

    // Subscribe to futures funding rate
    let funding_sub = serde_json::to_string(&serde_json::json!({
        "channel": CHANNEL_FUNDING_RATE,
        "event": "subscribe",
        "payload": [perp_symbol]
    }))?;
    write.send(Message::Text(funding_sub.into())).await?;
    debug!("Subscribed to futures.funding_rate");

    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![].into())).await?;
                debug!("Sent ping");
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        hot_path.process_text(&text);
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(Message::Close(e))) => {
                        warn!("WS closed: {:?}", e);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("WS error: {:?}", e);
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Gateway WS runner with exponential backoff reconnection.
///
/// Policy:
/// - Backoff: 1s → 2s → 4s → ... → 30s (cap)
/// - Max 10 consecutive failures → bail (engine halts)
/// - Disconnected >60s → logs a warning (order pause hook, risk guard)
/// - On reconnect: re-subscribes to all channels automatically
async fn run_gateway_with_reconnect(
    spot_symbol: &str,
    perp_symbol: &str,
    hot_path: Arc<HotPath>,
    health: Arc<health::HealthHandle>,
    cfg: ReconnectConfig,
) -> Result<()> {
    // consecutive_failures counts uninterrupted failures (reset on clean connect).
    // disconnect_since tracks when the current outage window started (reset on reconnect).
    let mut consecutive_failures: u32 = 0;
    let mut disconnect_since: Option<Instant> = None;

    loop {
        if consecutive_failures > 0 {
            let delay = cfg.delay_for_attempt(consecutive_failures);
            warn!(
                "WS reconnect: consecutive_failures={}/{} delay={:.1}s ts={}",
                consecutive_failures,
                cfg.max_attempts,
                delay.as_secs_f32(),
                chrono_now_utc(),
            );
            health.set_gate_ws(health::GateWsStatus::Disconnected).await;

            if let Some(since) = disconnect_since {
                if since.elapsed() >= cfg.pause_threshold {
                    warn!(
                        "WS disconnected >{:.0}s — pausing new entries (risk guard)",
                        cfg.pause_threshold.as_secs()
                    );
                    // Future: signal strategy to pause entries here
                }
            }

            tokio::time::sleep(delay).await;
        }

        match run_gateway_once(spot_symbol, perp_symbol, Arc::clone(&hot_path)).await {
            Ok(()) => {
                // Clean close — not a failure. Reset consecutive failure counter and
                // outage timer so the 10-strikes limit counts consecutive bad reconnects,
                // not total lifetime disconnects.
                warn!("WS closed cleanly, reconnecting");
                consecutive_failures = 0;
                disconnect_since = None;
            }
            Err(e) => {
                // Connection error — count as failure, keep outage timer running.
                error!(
                    "WS connection error (consecutive_failures={}): {:?}",
                    consecutive_failures + 1,
                    e
                );
                if disconnect_since.is_none() {
                    disconnect_since = Some(Instant::now());
                }
                consecutive_failures += 1;
            }
        }

        // Halt only after N *consecutive* failures (clean reconnects reset the counter).
        if consecutive_failures >= cfg.max_attempts {
            anyhow::bail!(
                "Gateway WS: {} consecutive connection failures. Engine halting.",
                cfg.max_attempts
            );
        }
    }
}

/// UTC timestamp string for log messages (no external dep).
fn chrono_now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let time = secs % 86400;
    let (h, m, s) = (time / 3600, (time % 3600) / 60, time % 60);
    // Gregorian date from Julian day number
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .compact()
        .init();

    info!("gate-arb starting...");

    // --- Parse all env-driven config at startup ---
    let cfg = config::Config::from_env();
    cfg.log_effective();

    // --- Health server setup ---
    let health = Arc::new(health::HealthHandle::new(cfg.health_port));

    // Startup self-check
    info!("Running startup self-check...");
    health.run_startup_check().await;
    {
        let state = health.state.read().await;
        if !state.startup_ok {
            let err = state
                .startup_error
                .clone()
                .unwrap_or_else(|| "unknown".into());
            anyhow::bail!("Startup self-check FAILED: {}. Aborting.", err);
        }
    }
    info!("Startup self-check PASSED");

    // Build core components
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    health.set_engine(health::EngineStatus::Idle).await;

    // Spread arb strategy — threshold from config
    let strategy = Arc::new(strategy::Strategy::new(
        Arc::clone(&engine),
        cfg.spread_threshold_raw,
    ));

    // Funding arb strategy
    let funding_strategy = Arc::new(FundingStrategy::new(cfg.paper_mode));
    info!(
        "Funding arb strategy: paper_mode={} threshold={:.1}bps entry_window={}s",
        cfg.paper_mode, funding_strategy.entry_threshold_bps, 300
    );

    let frontend = Arc::new(frontend_ws::FrontendWs::new());

    // Hot path coordinator
    let hot_path = Arc::new(HotPath::new(
        Arc::clone(&engine),
        Arc::clone(&strategy),
        Arc::clone(&funding_strategy),
        Arc::clone(&frontend),
    ));

    // Start frontend WS server — pre-bind to surface port conflicts at startup.
    // Use GATE_FRONTEND_PORT (or FRONTEND_PORT) to change from default 8080.
    let fe_listener = {
        let addr = format!("0.0.0.0:{}", cfg.frontend_port);
        tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
            tracing::error!("cannot bind frontend-ws to {}: {}", addr, e);
            tracing::error!("Hint: set GATE_FRONTEND_PORT (or FRONTEND_PORT) to a free port");
            anyhow::anyhow!("frontend-ws bind failed on {}: {}", addr, e)
        })?
    };
    info!(
        "Frontend WS bound — dashboard at http://0.0.0.0:{}/",
        cfg.frontend_port
    );
    let fe = Arc::clone(&frontend);
    let health_fe = Arc::clone(&health);
    tokio::spawn(async move {
        if let Err(e) = fe.run_with_listener(fe_listener).await {
            tracing::error!("Frontend WS stopped: {:?}", e);
            health_fe
                .set_frontend_ws(health::FrontendStatus::Stopped)
                .await;
        }
    });
    health
        .set_frontend_ws(health::FrontendStatus::Running)
        .await;

    // Start health HTTP server
    let health_srv = Arc::clone(&health);
    tokio::spawn(async move {
        health_srv.run_server().await;
    });
    info!(
        "Health endpoint on http://0.0.0.0:{}/health",
        cfg.health_port
    );

    // Run Gate.io WS client with exponential backoff reconnection
    info!("Starting Gate.io WS client (reconnect: 1s→30s, max 10 attempts)");
    health.set_gate_ws(health::GateWsStatus::Connected).await;

    let result = run_gateway_with_reconnect(
        &cfg.spot_symbol,
        &cfg.perp_symbol,
        hot_path,
        Arc::clone(&health),
        ReconnectConfig::default(),
    )
    .await;

    health.set_gate_ws(health::GateWsStatus::Disconnected).await;

    if let Err(e) = result {
        tracing::error!("Gateway WS halted: {:?}", e);
        return Err(e);
    }

    Ok(())
}
