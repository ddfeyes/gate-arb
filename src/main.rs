//! gate-arb — HFT spot-perp & funding arbitrage engine.
//!
//! Usage: cargo run --release --bin gate-arb
//!
//! Paper trading by default. Set PAPER_MODE=false to enable live trading.

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
use gateway_ws::{parse_price as gw_parse_price, WsEvent, CHANNEL_FUNDING_RATE};
use strategy::{FundingStrategy, Strategy};
use types::{Level as ObLevel, OrderBook};

mod args {
    use std::env;

    pub fn spot_symbol() -> String {
        env::var("SPOT_SYMBOL").unwrap_or_else(|_| "BTC_USDT".into())
    }
    pub fn perp_symbol() -> String {
        env::var("PERP_SYMBOL").unwrap_or_else(|_| "BTC_USDT".into())
    }
    pub fn frontend_port() -> u16 {
        env::var("FRONTEND_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080)
    }
    pub fn health_port() -> u16 {
        env::var("HEALTH_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8081)
    }
}

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

/// Reconnection config for the gateway WS loop.
struct ReconnectConfig {
    /// Initial backoff delay.
    initial_delay: Duration,
    /// Backoff multiplier (doubles each time, capped at max_delay).
    max_delay: Duration,
    /// Max consecutive reconnect attempts before giving up.
    max_attempts: u32,
    /// If disconnected longer than this, pause order placement.
    pause_threshold: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_attempts: 10,
            pause_threshold: Duration::from_secs(60),
        }
    }
}

/// Single WS connection attempt — connects, subscribes, then pumps messages until disconnect.
/// Returns Ok(()) on clean disconnect, Err on fatal errors.
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
/// Reconnect policy:
/// - Backoff: 1s → 2s → 4s → 8s → ... → 30s (max)
/// - Max 10 consecutive failures → engine halts
/// - Disconnected >60s → order placement paused (risk control)
/// - On reconnect: re-subscribes to all channels automatically
/// - Each reconnect event is logged with timestamp + attempt number
async fn run_gateway_with_reconnect(
    spot_symbol: &str,
    perp_symbol: &str,
    hot_path: Arc<HotPath>,
    health: Arc<health::HealthHandle>,
    cfg: ReconnectConfig,
) -> Result<()> {
    let mut attempt: u32 = 0;
    let mut disconnect_at: Option<Instant> = None;

    loop {
        if attempt > 0 {
            // Compute backoff: 1s * 2^(attempt-1), capped at max_delay
            let raw_delay = cfg.initial_delay.as_millis() as u64 * (1u64 << (attempt - 1).min(5));
            let delay = Duration::from_millis(raw_delay).min(cfg.max_delay);

            warn!(
                "WS reconnect: attempt={}/{} delay={}s ts={}",
                attempt,
                cfg.max_attempts,
                delay.as_secs_f32(),
                chrono_now_utc()
            );
            health.set_reconnect_attempt(attempt).await;
            health.set_gate_ws(health::GateWsStatus::Disconnected).await;

            // Check if we've been disconnected >60s → pause orders
            if let Some(da) = disconnect_at {
                if da.elapsed() >= cfg.pause_threshold {
                    warn!(
                        "WS disconnected >{}s — pausing order placement",
                        cfg.pause_threshold.as_secs()
                    );
                    health.set_order_pause(true).await;
                }
            }

            tokio::time::sleep(delay).await;
        }

        // Track when this disconnect window started (if not already set)
        if attempt > 0 && disconnect_at.is_none() {
            disconnect_at = Some(Instant::now());
        }

        match run_gateway_once(spot_symbol, perp_symbol, Arc::clone(&hot_path)).await {
            Ok(()) => {
                // Clean disconnect — reconnect
                warn!(
                    "WS connection closed cleanly, will reconnect (attempt {})",
                    attempt + 1
                );
                if disconnect_at.is_none() {
                    disconnect_at = Some(Instant::now());
                }
                attempt += 1;
            }
            Err(e) => {
                error!("WS connection error: {:?} (attempt {})", e, attempt + 1);
                if disconnect_at.is_none() {
                    disconnect_at = Some(Instant::now());
                }
                attempt += 1;
            }
        }

        if attempt > cfg.max_attempts {
            error!(
                "WS reconnect exhausted: {} consecutive failures. Engine halting.",
                cfg.max_attempts
            );
            health.set_gate_ws(health::GateWsStatus::Disconnected).await;
            anyhow::bail!(
                "Gateway WS: {} consecutive reconnect failures. Engine halted.",
                cfg.max_attempts
            );
        }

        // Successful connection resets counters
        if attempt == 0 {
            health.reset_reconnect().await;
            health.set_gate_ws(health::GateWsStatus::Connected).await;
            disconnect_at = None;
            info!("WS connected successfully");
        }
    }
}

/// Return current UTC time as a compact string for logging.
fn chrono_now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as YYYY-MM-DDTHH:MM:SSZ (no external dep, manual math)
    let s = secs;
    let days = s / 86400;
    let time = s % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let sec = time % 60;
    // Julian day → Gregorian (simple approximation for display)
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
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", yr, mo, d, h, m, sec)
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

    // --- Health server setup ---
    let health = Arc::new(health::HealthHandle::new(args::health_port()));

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

    // threshold 25B raw ≈ 50bps on BTC at $50k
    let strategy = Arc::new(strategy::Strategy::new(Arc::clone(&engine), 25_000_000_000));

    // Funding arb strategy (paper mode by default)
    let paper_mode = std::env::var("PAPER_MODE")
        .map(|v| v != "false")
        .unwrap_or(true);
    let funding_strategy = Arc::new(FundingStrategy::new(paper_mode));
    info!(
        "Funding arb strategy: paper_mode={} threshold={:.1}bps entry_window={}s",
        paper_mode, funding_strategy.entry_threshold_bps, 300
    );

    let frontend = Arc::new(frontend_ws::FrontendWs::new());

    // Hot path coordinator
    let hot_path = Arc::new(HotPath::new(
        Arc::clone(&engine),
        Arc::clone(&strategy),
        Arc::clone(&funding_strategy),
        Arc::clone(&frontend),
    ));

    // Start frontend WS server
    let fe = Arc::clone(&frontend);
    let fe_port = args::frontend_port();
    let health_fe = Arc::clone(&health);
    tokio::spawn(async move {
        if let Err(e) = fe.run(fe_port).await {
            tracing::error!("Frontend WS error: {:?}", e);
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
        args::health_port()
    );

    // Run Gate.io WS client with reconnection
    let spot_sym = args::spot_symbol();
    let perp_sym = args::perp_symbol();
    info!("Starting Gate.io WS client (with exponential backoff reconnect)");
    health.set_gate_ws(health::GateWsStatus::Connected).await;

    let reconnect_cfg = ReconnectConfig::default();
    let result = run_gateway_with_reconnect(
        &spot_sym,
        &perp_sym,
        hot_path,
        Arc::clone(&health),
        reconnect_cfg,
    )
    .await;
    health.set_gate_ws(health::GateWsStatus::Disconnected).await;

    if let Err(e) = result {
        tracing::error!("Gateway WS terminated: {:?}", e);
    }

    Ok(())
}
