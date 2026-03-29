//! Integration tests for crates/db.
//! Uses tempfile for an in-process SQLite database — no real files left behind.

use db::{DbReader, DbWriter, PnlSummary, SpreadSnapshot, TradeRecord};
use std::time::Duration;
use tokio::time::sleep;

fn sample_trade(pnl_raw: i64, paper: bool) -> TradeRecord {
    TradeRecord {
        opened_at_us: 1_000_000,
        closed_at_us: 2_000_000,
        entry_price_raw: 50_000_00_000_000, // $50,000
        exit_price_raw: 50_010_00_000_000,
        size_raw: 100_000_000, // 1 unit
        pnl_raw,
        pnl_pct_raw: 20_000, // 0.02%
        exit_reason: "exit_spread".to_string(),
        paper,
    }
}

fn sample_spread(spread_raw: i64, inverted: bool) -> SpreadSnapshot {
    SpreadSnapshot {
        ts_us: 1_500_000,
        spread_raw,
        inverted,
        spot_price_raw: 50_000_00_000_000,
        perp_price_raw: 50_010_00_000_000,
    }
}

async fn temp_db() -> (DbWriter, tokio::task::JoinHandle<()>, String) {
    // Use a temp path unique per test
    let path = format!(
        "/tmp/gate-arb-test-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let (w, h) = DbWriter::open(&path).expect("open db");
    (w, h, path)
}

#[tokio::test]
async fn write_and_read_trade() {
    let (writer, _handle, path) = temp_db().await;

    writer.write_trade(sample_trade(1_000_000, true));
    sleep(Duration::from_millis(50)).await;
    writer.shutdown();
    sleep(Duration::from_millis(50)).await;

    let reader = DbReader::open(&path).expect("open reader");
    let trades = reader.recent_trades(10).expect("recent_trades");
    assert_eq!(trades.len(), 1, "expected 1 trade");
    assert_eq!(trades[0].pnl_raw, 1_000_000);
    assert!(trades[0].paper);
    assert_eq!(trades[0].exit_reason, "exit_spread");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn write_and_read_spread() {
    let (writer, _handle, path) = temp_db().await;

    writer.write_spread(sample_spread(5_000_000, false));
    writer.write_spread(sample_spread(-2_000_000, true));
    sleep(Duration::from_millis(50)).await;
    writer.shutdown();
    sleep(Duration::from_millis(50)).await;

    let reader = DbReader::open(&path).expect("open reader");
    let spreads = reader.recent_spreads(10).expect("recent_spreads");
    assert_eq!(spreads.len(), 2);
    // newest first
    assert!(spreads[0].inverted); // -2_000_000 inserted second

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn pnl_summary_correct() {
    let (writer, _handle, path) = temp_db().await;

    writer.write_trade(sample_trade(2_000_000, true));
    writer.write_trade(sample_trade(-500_000, true));
    writer.write_trade(sample_trade(1_000_000, false));
    sleep(Duration::from_millis(80)).await;
    writer.shutdown();
    sleep(Duration::from_millis(50)).await;

    let reader = DbReader::open(&path).expect("open reader");
    let summary = reader.pnl_summary().expect("pnl_summary");
    assert_eq!(summary.total_trades, 3);
    assert_eq!(summary.wins, 2); // two positive
    assert_eq!(summary.total_pnl_raw, 2_500_000);

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn recent_trades_limit() {
    let (writer, _handle, path) = temp_db().await;

    for i in 0..5i64 {
        writer.write_trade(sample_trade(i * 100_000, true));
    }
    sleep(Duration::from_millis(80)).await;
    writer.shutdown();
    sleep(Duration::from_millis(50)).await;

    let reader = DbReader::open(&path).expect("open reader");
    let trades = reader.recent_trades(3).expect("recent_trades");
    assert_eq!(trades.len(), 3, "limit should be respected");

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn schema_idempotent_reopen() {
    let path = format!(
        "/tmp/gate-arb-schema-test-{}.db",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // Open twice — CREATE IF NOT EXISTS should not panic
    {
        let (w, _h) = DbWriter::open(&path).expect("first open");
        w.write_trade(sample_trade(100, true));
        sleep(Duration::from_millis(30)).await;
        w.shutdown();
        sleep(Duration::from_millis(30)).await;
    }
    {
        let (w, _h) = DbWriter::open(&path).expect("second open");
        w.write_trade(sample_trade(200, false));
        sleep(Duration::from_millis(30)).await;
        w.shutdown();
        sleep(Duration::from_millis(30)).await;
    }

    let reader = DbReader::open(&path).expect("reader");
    assert_eq!(reader.recent_trades(10).expect("trades").len(), 2);

    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn write_spread_empty_db_returns_zero_pnl() {
    let (writer, _handle, path) = temp_db().await;
    writer.write_spread(sample_spread(1_000, false));
    sleep(Duration::from_millis(50)).await;
    writer.shutdown();
    sleep(Duration::from_millis(30)).await;

    let reader = DbReader::open(&path).expect("reader");
    let summary = reader.pnl_summary().expect("pnl");
    assert_eq!(summary.total_trades, 0);
    assert_eq!(summary.total_pnl_raw, 0);

    let _ = std::fs::remove_file(path);
}
