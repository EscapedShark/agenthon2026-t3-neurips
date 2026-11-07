//! Round-trip the hand-written Parquet encoder through an independent reader (parquet-rs) over
//! edge cases the public units never produce: empty files, 1/7/8/9 rows, row-group boundaries,
//! all-null row groups, extreme and wrapping i64/i32 values, and every dictionary entry.

use fastsim::output::{ledger_parquet, trace_parquet};
use fastsim::rng::Mt19937;
use fastsim::sim::trace::{LedgerRow, MsgKind, TraceRow, TraceType, CHUNK_ROWS, NONE, NULL};
use fastsim::sim::Side;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::record::Field;

const KINDS: [MsgKind; 13] = [
    MsgKind::Wakeup,
    MsgKind::MarketHoursRequest,
    MsgKind::MarketClosePriceRequest,
    MsgKind::MarketHours,
    MsgKind::MarketClosePrice,
    MsgKind::MarketClosed,
    MsgKind::QuerySpread,
    MsgKind::QuerySpreadResponse,
    MsgKind::LimitOrder,
    MsgKind::CancelOrder,
    MsgKind::OrderAccepted,
    MsgKind::OrderExecuted,
    MsgKind::OrderCancelled,
];
const TRACE_KINDS: [TraceType; 6] = [
    TraceType::Submitted,
    TraceType::Accepted,
    TraceType::Filled,
    TraceType::PartialFill,
    TraceType::Cancelled,
    TraceType::Quote,
];

/// Values that stress delta encoding: steady steps, big jumps, and both ends of the range.
fn wild(rng: &mut Mt19937, i: usize, prev: i64) -> i64 {
    match rng.next_u32() % 8 {
        0 => i64::MAX,
        1 => i64::MIN + 1, // i64::MIN itself is the ledger's null sentinel
        2 => rng.next_u64() as i64,
        3 => -(i as i64),
        _ => prev.wrapping_add((rng.next_u32() % 1000) as i64 - 300),
    }
}

fn ledger_rows(n: usize, seed: u32, null_every: u32) -> Vec<LedgerRow> {
    let mut rng = Mt19937::new(seed);
    let mut prev = 1_612_483_200_000_000_000i64;
    (0..n)
        .map(|i| {
            prev = wild(&mut rng, i, prev);
            let is_null = |rng: &mut Mt19937| null_every > 0 && rng.next_u32() % null_every == 0;
            // The latency column is t_recv - t_send (wrapping at the extremes), 0 for a null t_send.
            let latency = (rng.next_u32() % 5000) as i64;
            let t_send = if is_null(&mut rng) { NULL } else { prev.wrapping_sub(latency) };
            let order_id = rng.next_u32() % NONE; // every 32-bit id but the null marker
            let order_id = if is_null(&mut rng) { NONE } else { order_id };
            let causal_parent = rng.next_u32() % NONE;
            let causal_parent = if is_null(&mut rng) { NONE } else { causal_parent };
            LedgerRow {
                t_recv: prev,
                t_send,
                message_id: i as u32 * 3 + 1,
                order_id,
                causal_parent,
                src: rng.next_u32(), // the full i32 range, including wrapping deltas
                dst: rng.next_u32() % 200,
                kind: KINDS[rng.next_u32() as usize % KINDS.len()],
            }
        })
        .collect()
}

fn opt_long(f: &Field) -> Option<i64> {
    match f {
        Field::Null => None,
        Field::Long(v) => Some(*v),
        other => panic!("expected INT64, got {other:?}"),
    }
}

/// With `FASTSIM_PARQUET_FIXTURES=<dir>`, also write each file and its expected rows (JSON) so
/// `tools/check_parquet_fixtures.py` can verify them with pyarrow, the scorer's reader.
fn export(name: &str, bytes: &[u8], expected: serde_json::Value) {
    if let Ok(dir) = std::env::var("FASTSIM_PARQUET_FIXTURES") {
        let dir = std::path::Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{name}.parquet")), bytes).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), expected.to_string()).unwrap();
    }
}

fn check_ledger(rows: &[LedgerRow], label: &str) {
    let bytes = ledger_parquet(rows);
    let null = |v: i64| if v == NULL { serde_json::Value::Null } else { v.into() };
    export(
        &format!("ledger_{}", label.replace([' ', '/', '=', '+'], "_")),
        &bytes,
        serde_json::json!({
            "t_recv_ns": rows.iter().map(|r| r.t_recv).collect::<Vec<_>>(),
            "t_send_ns": rows.iter().map(|r| null(r.t_send)).collect::<Vec<_>>(),
            "latency_ns": rows.iter().map(LedgerRow::latency).collect::<Vec<_>>(),
            "src_id": rows.iter().map(|r| r.src as i32).collect::<Vec<_>>(),
            "dst_id": rows.iter().map(|r| r.dst as i32).collect::<Vec<_>>(),
            "message_id": rows.iter().map(|r| r.message_id).collect::<Vec<_>>(),
            "msg_type": rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
            "order_id": rows.iter().map(|r| LedgerRow::id(r.order_id)).collect::<Vec<_>>(),
            "causal_parent": rows.iter().map(|r| LedgerRow::id(r.causal_parent)).collect::<Vec<_>>(),
        }),
    );
    let reader = SerializedFileReader::new(bytes::Bytes::from(bytes)).expect("readable parquet");
    assert_eq!(reader.metadata().file_metadata().num_rows() as usize, rows.len(), "{label}: footer rows");
    assert_eq!(reader.metadata().num_row_groups(), rows.len().div_ceil(CHUNK_ROWS), "{label}: row groups");
    let got: Vec<_> = reader.get_row_iter(None).expect("rows").map(|r| r.expect("row")).collect();
    assert_eq!(got.len(), rows.len(), "{label}: row count");
    for (i, (row, want)) in got.iter().zip(rows).enumerate() {
        let f: Vec<&Field> = row.get_column_iter().map(|(_, f)| f).collect();
        let null = |v: i64| (v != NULL).then_some(v);
        assert_eq!(opt_long(f[0]), Some(i as i64), "{label} row {i}: seq");
        assert_eq!(opt_long(f[1]), Some(want.t_recv), "{label} row {i}: t_recv");
        assert_eq!(opt_long(f[2]), null(want.t_send), "{label} row {i}: t_send");
        assert_eq!(opt_long(f[3]), Some(want.latency()), "{label} row {i}: latency");
        assert_eq!(f[4], &Field::Int(want.src as i32), "{label} row {i}: src");
        assert_eq!(f[5], &Field::Int(want.dst as i32), "{label} row {i}: dst");
        assert_eq!(opt_long(f[6]), Some(want.message_id as i64), "{label} row {i}: message_id");
        assert_eq!(f[7], &Field::Str(want.kind.as_str().to_string()), "{label} row {i}: msg_type");
        assert_eq!(opt_long(f[8]), LedgerRow::id(want.order_id), "{label} row {i}: order_id");
        assert_eq!(opt_long(f[9]), LedgerRow::id(want.causal_parent), "{label} row {i}: causal_parent");
    }
}

#[test]
fn ledger_round_trips_edge_sizes_and_values() {
    for (n, null_every) in [(0, 3), (1, 0), (1, 1), (7, 2), (8, 2), (9, 2), (127, 3), (128, 3), (129, 3), (1000, 5)] {
        check_ledger(&ledger_rows(n, n as u32 + 11, null_every), &format!("n={n} nulls 1/{null_every}"));
    }
    // Row-group boundaries, and a whole row group of nulls in the nullable columns.
    let mut rows = ledger_rows(CHUNK_ROWS + 3, 5, 4);
    for r in &mut rows[CHUNK_ROWS..] {
        r.t_send = NULL;
        r.order_id = NONE;
        r.causal_parent = NONE;
    }
    check_ledger(&rows, "chunk boundary + all-null group");
}

#[test]
fn trace_round_trips_every_kind_and_side() {
    let mut rng = Mt19937::new(99);
    let mut prev = 0i64;
    for n in [0usize, 1, 9, 3000, CHUNK_ROWS + 1] {
        let rows: Vec<TraceRow> = (0..n)
            .map(|i| {
                prev = wild(&mut rng, i, prev);
                TraceRow {
                    t_ns: prev,
                    order_id: if i % 5 == 0 { -1 } else { rng.next_u64() as i64 },
                    price: wild(&mut rng, i, prev),
                    size: (rng.next_u32() % 100) as i64,
                    agent_id: rng.next_u32() as i32,
                    kind: TRACE_KINDS[i % TRACE_KINDS.len()],
                    side: if rng.next_u32() % 2 == 0 { Side::Bid } else { Side::Ask },
                }
            })
            .collect();
        let bytes = trace_parquet(&rows);
        export(
            &format!("trace_n{n}"),
            &bytes,
            serde_json::json!({
                "t_ns": rows.iter().map(|r| r.t_ns).collect::<Vec<_>>(),
                "agent_id": rows.iter().map(|r| r.agent_id).collect::<Vec<_>>(),
                "msg_type": rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
                "side": rows.iter().map(|r| r.side.as_str()).collect::<Vec<_>>(),
                "price": rows.iter().map(|r| r.price).collect::<Vec<_>>(),
                "size": rows.iter().map(|r| r.size).collect::<Vec<_>>(),
                "order_id": rows.iter().map(|r| r.order_id).collect::<Vec<_>>(),
            }),
        );
        let reader = SerializedFileReader::new(bytes::Bytes::from(bytes)).expect("readable parquet");
        let got: Vec<_> = reader.get_row_iter(None).expect("rows").map(|r| r.expect("row")).collect();
        assert_eq!(got.len(), n);
        for (row, want) in got.iter().zip(&rows) {
            let f: Vec<&Field> = row.get_column_iter().map(|(_, f)| f).collect();
            assert_eq!(f[0], &Field::Long(want.t_ns));
            assert_eq!(f[1], &Field::Int(want.agent_id));
            assert_eq!(f[2], &Field::Str(want.kind.as_str().to_string()));
            assert_eq!(f[3], &Field::Str(want.side.as_str().to_string()));
            assert_eq!(f[4], &Field::Long(want.price));
            assert_eq!(f[5], &Field::Long(want.size));
            assert_eq!(f[6], &Field::Long(want.order_id));
        }
    }
}
