#[cfg(test)]
mod tests {
    use types::{Fixed64, OrderAck, OrderCmd, OrderStatus, Side};

    #[test]
    fn order_cmd_roundtrip() {
        let cmd = OrderCmd::Place {
            client_id: 42,
            symbol: "BTC_USDT",
            side: Side::Bid,
            price: Fixed64::from_raw(5_000_000_000_000),
            qty: 1_000_000,
            post_only: true,
        };
        match cmd {
            OrderCmd::Place { client_id, post_only, .. } => {
                assert_eq!(client_id, 42);
                assert!(post_only);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn order_ack_status() {
        let ack = OrderAck {
            client_id: 42,
            exchange_id: "abc123".to_string(),
            status: OrderStatus::Open,
        };
        assert_eq!(ack.client_id, 42);
        assert_eq!(ack.status, OrderStatus::Open);
    }
}
