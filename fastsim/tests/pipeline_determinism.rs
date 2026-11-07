use fastsim::{output, scenario::Scenario};

#[test]
fn scalar_and_prefetched_runs_emit_the_same_bytes_across_thread_counts() {
    let doc = serde_json::json!({
        "scenario_id": "parallel-determinism", "seed": 42, "horizon_ns": 10_000_000_000i64,
        "agent_mix": {"NoiseTrader": 8, "MarketMaker": 2},
        "agent_configs": [
            {"agent_type": "NoiseTrader", "count": 8, "params": {"arrival_rate_hz": 20}},
            {"agent_type": "MarketMaker", "count": 2, "params": {
                "rebalance_interval_ns": 50_000_000, "depth_levels": 4
            }}
        ],
        "latency_config": {"model": "log_normal", "params": {"mean_ns": 5000, "sigma": 0.75}},
        "exchange_config": {"symbol": "SYM", "tick_size": 1}
    });
    let sc = Scenario::from_value(&doc);
    for ledger in [false, true] {
        let scalar = output::simulate(&sc, ledger, 0);
        assert!(scalar.trace_rows > fastsim::sim::trace::CHUNK_ROWS);
        for workers in [1, 2, 3, 3] {
            let candidate = output::simulate(&sc, ledger, workers);
            assert_eq!(candidate.trace_rows, scalar.trace_rows);
            assert_eq!(candidate.ledger_rows, scalar.ledger_rows);
            assert_eq!(candidate.trace.to_bytes(), scalar.trace.to_bytes(), "trace, ledger={ledger}, workers={workers}");
            assert_eq!(
                candidate.ledger.as_ref().map(|f| f.to_bytes()),
                scalar.ledger.as_ref().map(|f| f.to_bytes()),
                "ledger, workers={workers}"
            );
        }
    }
}
