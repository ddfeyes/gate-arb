//! order-manager — Gate.io private WebSocket for order placement.
//!
//! Post-only maker orders only. Auth via HMAC-SHA512.
//! Thread-safe: orders sent via mpsc channel from hot path.
//!
//! Reconnect: on WS error or disconnect, run() retries with exponential backoff
//! (1s → 2s → 4s … capped at 60s). The cmd channel is preserved across reconnects.

pub mod auth;
pub mod codec;
pub mod ring;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use ring::{InFlightOrder, OrderRing};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use types::{OrderAck, OrderCmd, OrderStatus};

/// Handle to send commands to the OrderManager from any thread.
pub type OrderSender = mpsc::Sender<OrderCmd>;
/// Handle to receive acks from the OrderManager on the warm thread.
pub type AckReceiver = mpsc::Receiver<OrderAck>;

pub struct OrderManager {
    api_key: String,
    api_secret: String,
    ws_url: &'static str,
}

impl OrderManager {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            ws_url: "wss://api.gateio.ws/ws/v4/",
        }
    }

    /// Returns (cmd_tx, ack_rx) and spawns the WS manager task.
    ///
    /// The spawned task reconnects automatically on disconnect with exponential
    /// backoff. cmd_tx remains valid across reconnects.
    pub fn start(self) -> (OrderSender, AckReceiver) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<OrderCmd>(64);
        let (ack_tx, ack_rx) = mpsc::channel::<OrderAck>(64);
        tokio::spawn(async move {
            self.run(cmd_rx, ack_tx).await;
        });
        (cmd_tx, ack_rx)
    }

    /// Outer reconnect loop. Retries session() on any error/disconnect.
    /// Backoff: 1s → 2s → 4s … max 60s.
    async fn run(self, mut cmd_rx: mpsc::Receiver<OrderCmd>, ack_tx: mpsc::Sender<OrderAck>) {
        let mut backoff = Duration::from_secs(1);
        let max_backoff = Duration::from_secs(60);

        loop {
            match self.session(&mut cmd_rx, &ack_tx).await {
                SessionExit::CmdChannelClosed => {
                    info!("OrderManager: cmd channel closed, shutting down");
                    return;
                }
                SessionExit::WsError(e) => {
                    error!(
                        "OrderManager WS error: {:?} — reconnecting in {:?}",
                        e, backoff
                    );
                }
                SessionExit::WsClosed => {
                    warn!("OrderManager WS closed — reconnecting in {:?}", backoff);
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    /// Inner WS session: connect → auth → subscribe → event loop.
    /// Returns the reason the session ended.
    async fn session(
        &self,
        cmd_rx: &mut mpsc::Receiver<OrderCmd>,
        ack_tx: &mpsc::Sender<OrderAck>,
    ) -> SessionExit {
        info!("OrderManager connecting to {}", self.ws_url);
        let ws = match connect_async(self.ws_url).await {
            Ok((ws, _)) => ws,
            Err(e) => return SessionExit::WsError(format!("connect failed: {:?}", e)),
        };
        let (mut write, mut read) = ws.split();

        // Authenticate
        let ts = unix_secs();
        let auth_header = auth::build_auth_header(&self.api_key, &self.api_secret, ts);
        let login_msg = serde_json::json!({
            "time": ts,
            "channel": "spot.login",
            "event": "api",
            "payload": {},
            "auth": {
                "method": "api_key",
                "KEY": auth_header.key,
                "SIGN": auth_header.signature,
                "Timestamp": auth_header.timestamp.to_string()
            }
        });
        if let Err(e) = write
            .send(Message::Text(login_msg.to_string().into()))
            .await
        {
            return SessionExit::WsError(format!("login send failed: {:?}", e));
        }
        info!("OrderManager: login sent");

        // Subscribe to spot.orders channel
        let sub_msg = serde_json::json!({
            "time": unix_secs(),
            "channel": "spot.orders",
            "event": "subscribe",
            "payload": ["BTC_USDT"]
        });
        if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
            return SessionExit::WsError(format!("subscribe send failed: {:?}", e));
        }

        let mut ring = OrderRing::<32>::new();
        let mut ping_interval = tokio::time::interval(Duration::from_secs(20));

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_inbound(text.as_str(), &mut ring, ack_tx).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Err(e)) => {
                            return SessionExit::WsError(format!("{:?}", e));
                        }
                        None => {
                            return SessionExit::WsClosed;
                        }
                        _ => {}
                    }
                }

                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            self.handle_cmd(cmd, &mut write, &mut ring).await;
                        }
                        None => {
                            return SessionExit::CmdChannelClosed;
                        }
                    }
                }

                _ = ping_interval.tick() => {
                    let ping = serde_json::json!({
                        "time": unix_secs(),
                        "channel": "spot.ping",
                        "event": ""
                    });
                    let _ = write.send(Message::Text(ping.to_string().into())).await;
                }
            }
        }
    }

    async fn handle_inbound(
        &self,
        text: &str,
        ring: &mut OrderRing<32>,
        ack_tx: &mpsc::Sender<OrderAck>,
    ) {
        if let Ok(ack) = codec::decode_order_update(text) {
            ring.update_status(ack.client_id, ack.exchange_id.clone(), ack.status);
            if matches!(ack.status, OrderStatus::Filled | OrderStatus::Cancelled) {
                ring.remove(ack.client_id);
            }
            // Use blocking send: fill acks must not be dropped.
            // The warm thread must drain the ack channel promptly (cap 64).
            if let Err(e) = ack_tx.send(ack).await {
                error!("OrderManager: ack channel closed, fill lost: {:?}", e);
            }
        }
    }

    async fn handle_cmd(
        &self,
        cmd: types::OrderCmd,
        write: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
        ring: &mut OrderRing<32>,
    ) {
        match cmd {
            types::OrderCmd::Place {
                client_id,
                symbol,
                side,
                price,
                qty,
                post_only: _,
            } => {
                let ts = unix_secs();
                let sign_str = format!("channel=spot.orders\nevent=api\nts={ts}");
                let sig = auth::hmac_sign(&self.api_secret, &sign_str);

                let json = codec::encode_place_order(codec::PlaceOrderParams {
                    channel: "spot.orders",
                    client_id,
                    symbol,
                    side,
                    price,
                    qty,
                    signature: &sig,
                    api_key: &self.api_key,
                    ts,
                });

                ring.insert(InFlightOrder {
                    client_id,
                    exchange_id: String::new(),
                    symbol,
                    side,
                    price,
                    qty,
                    status: types::OrderStatus::Open,
                });

                if let Err(e) = write.send(Message::Text(json.into())).await {
                    error!("OrderManager: send place failed: {:?}", e);
                }
            }

            types::OrderCmd::Cancel {
                client_id: _,
                exchange_id,
                symbol,
            } => {
                let ts = unix_secs();
                let sign_str = format!("channel=spot.orders\nevent=cancel\nts={ts}");
                let sig = auth::hmac_sign(&self.api_secret, &sign_str);

                let json = codec::encode_cancel_order(
                    "spot.orders",
                    &exchange_id,
                    symbol,
                    &sig,
                    &self.api_key,
                    ts,
                );

                if let Err(e) = write.send(Message::Text(json.into())).await {
                    error!("OrderManager: send cancel failed: {:?}", e);
                }
            }
        }
    }
}

/// Reason a WS session ended.
enum SessionExit {
    /// cmd channel dropped — orderly shutdown.
    CmdChannelClosed,
    /// WS protocol error.
    WsError(String),
    /// Remote closed connection cleanly.
    WsClosed,
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
