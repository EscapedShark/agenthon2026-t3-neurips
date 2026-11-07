//! Predict, from the scenario seed alone, the RNG-only parts of a reference trace:
//! each NoiseTrader's (size, side, offset) per act and the latency of each non-self message.
//! `tools/check_rng_vs_reference.py` compares them with a unit's reference parquet files.
//!
//! Usage: seedcheck <scenario.json> <noise_draws_per_agent> <latency_draws>

use fastsim::rng::{py_round, RandomState};
use fastsim::scenario::{AgentKind, Scenario, SeedChain};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(args.len() == 4, "usage: seedcheck <scenario.json> <noise_draws> <latency_draws>");
    let sc = Scenario::from_json(&std::fs::read_to_string(&args[1]).expect("read scenario"));
    let noise_draws: usize = args[2].parse().expect("noise_draws");
    let latency_draws: usize = args[3].parse().expect("latency_draws");
    let chain = SeedChain::derive(&sc);

    let mut noise = serde_json::Map::new();
    for (idx, (spec, seed)) in sc.agents.iter().zip(&chain.agents).enumerate() {
        if spec.kind != AgentKind::NoiseTrader {
            continue;
        }
        let mut rs = RandomState::new(*seed);
        let draws: Vec<[i64; 3]> = (0..noise_draws)
            .map(|_| {
                let size = (py_round(rs.normal(spec.order_size_mean, spec.order_size_std)) as i64).max(1);
                let buy = rs.randint(0, 2);
                let offset = rs.randint(0, spec.price_offset_ticks + 1);
                [size, buy, offset]
            })
            .collect();
        noise.insert((idx + 1).to_string(), json!(draws));
    }

    let latency: Vec<i64> = match &sc.latency {
        Some(lat) => {
            let mut rs = RandomState::new(chain.latency);
            (0..latency_draws).map(|_| lat.draw(&mut rs)).collect()
        }
        None => Vec::new(),
    };

    let out = json!({
        "scenario_id": sc.scenario_id,
        "seeds": {
            "oracle": chain.oracle,
            "exchange": chain.exchange,
            "agents": chain.agents,
            "latency": chain.latency,
            "kernel": chain.kernel,
        },
        "latency_model": format!("{:?}", sc.latency.as_ref().map(|l| &l.model)),
        "noise": noise,
        "latency": latency,
    });
    println!("{out}");
}
