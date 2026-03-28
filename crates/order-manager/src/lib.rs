//! order-manager — Gate.io private WebSocket for order placement.
//!
//! Post-only maker orders only. Auth via HMAC-SHA512.
//! Thread-safe: orders sent via mpsc channel from hot path.

pub mod auth;
pub mod codec;
pub mod ring;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use ring::{InFlightOrder, OrderRing};
use std::time::{SystemTime, UNIX_EPOCH};
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
    pub fn start(self) -> (OrderSender, AckReceiver) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<OrderCmd>(64);
        let (ack_tx, ack_rx) = mpsc::channel::<OrderAck>(64);
        tokio::spawn(async move {
            if let Err(e) = self.run(cmd_rx, ack_tx).await {
                error!("OrderManager fatal: {:?}", e);
            }
        });
        (cmd_tx, ack_rx)
    }

    async fn run(
        self,
        mut cmd_rx: mpsc::Receiver<OrderCmd>,
        ack_tx: mpsc::Sender<OrderAck>,
    ) -> Result<()> {
        info!("OrderManager connecting to {}", self.ws_url);
        let (ws, _) = connect_async(self.ws_url).await?;
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
        write
            .send(Message::Text(login_msg.to_string().into()))
            .await?;
        info!("OrderManager: login sent");

        // Subscribe to spot.orders channel
        let sub_msg = serde_json::json!({
            "time": unix_secs(),
            "channel": "spot.orders",
            "event": "subscribe",
            "payload": ["BTC_USDT"]
        });
        write
            .send(Message::Text(sub_msg.to_string().into()))
            .await?;

        let mut ring = OrderRing::<32>::new();
        let mut ping_interval =
            tokio::time::interval(tokio::time::Duration::from_secs(20));

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_inbound(text.as_str(), &mut ring, &ack_tx).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Err(e)) => {
                            error!("OrderManager WS error: {:?}", e);
                            break;
                        }
                        None => {
                            warn!("OrderManager WS closed");
                            break;
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
                            info!("OrderManager: cmd channel closed, exiting");
                            break;
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

        Ok(())
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
            let _ = ack_tx.try_send(ack);
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
                let sig = hmac_sign(&self.api_secret, &sign_str);

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
                let sig = hmac_sign(&self.api_secret, &sign_str);

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

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn hmac_sign(secret: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;
    type HmacSha512 = Hmac<Sha512>;

    let mut mac = HmacSha512::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
