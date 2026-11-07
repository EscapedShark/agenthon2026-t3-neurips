//! `scenario.json` -> the simulation parameters, with the exact defaults and Python coercions of
//! the baseline adapter (`baselines/abides_fork/config.py`, `_agent_kwargs`, `_interval_ns`).

use serde_json::{Map, Value};

use crate::rng::RandomState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    NoiseTrader,
    MarketMaker,
    ValueTrader,
    MomentumTrader,
}

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub kind: AgentKind,
    pub interval_ns: i64,
    // NoiseTrader
    pub order_size_mean: f64,
    pub order_size_std: f64,
    pub price_offset_ticks: i64,
    // MarketMaker
    pub spread_ticks: i64,
    pub depth_levels: i64,
    pub size_per_level: i64,
    // ValueTrader / MomentumTrader
    pub threshold_ticks: i64,
    pub sigma_n: f64,
    pub lookback: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LatencyModel {
    LogNormal { mu: f64, sigma: f64 },
    Uniform,
    Pareto { base: f64, alpha: f64 },
    Constant(f64),
}

#[derive(Debug, Clone)]
pub struct Latency {
    pub model: LatencyModel,
    pub min_ns: f64,
    pub max_ns: f64,
}

#[derive(Debug, Clone)]
pub struct ScheduledJump {
    pub time_ns: i64,
    pub magnitude: i64,
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub scenario_id: String,
    pub seed: u32,
    pub horizon_ns: i64,
    pub reference_price: i64,
    pub oracle_kappa_per_ns: f64,
    pub oracle_fund_vol: f64,
    pub megashock_lambda_per_ns: f64,
    pub megashock_mean: f64,
    pub megashock_var: f64,
    pub scheduled_jump: Option<ScheduledJump>,
    pub stp_policy: Option<String>,
    pub ack_delay_ns: i64,
    pub compute_delay_ns: i64,
    /// `None` = no `latency_config`: ABIDES's fixed pairwise line-distance model.
    pub latency: Option<Latency>,
    /// Scenario agents in construction order; agent id = index + 1 (the exchange is id 0).
    pub agents: Vec<AgentSpec>,
}

/// Python `float(x)` of a JSON scalar.
fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().expect("number"),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => s.trim().parse().expect("numeric string"),
        other => panic!("not a number: {other}"),
    }
}

/// Python `int(x)` of a JSON scalar (truncates floats toward zero).
fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().expect("number").trunc() as i64),
        Value::Bool(b) => i64::from(*b),
        Value::String(s) => s.trim().parse().expect("integer string"),
        other => panic!("not an integer: {other}"),
    }
}

fn f(map: &Map<String, Value>, key: &str, default: f64) -> f64 {
    map.get(key).map_or(default, as_f64)
}

fn i(map: &Map<String, Value>, key: &str, default: i64) -> i64 {
    map.get(key).map_or(default, as_i64)
}

/// Python truthiness of an optional JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `_interval_ns`: explicit rebalance interval, else `int(1e9 / arrival_rate_hz)`; then `max(1, .)`.
fn interval_ns(params: &Map<String, Value>) -> i64 {
    let raw = if let Some(v) = params.get("rebalance_interval_ns") {
        as_i64(v)
    } else {
        let hz = f(params, "arrival_rate_hz", 1.0);
        if hz > 0.0 { (1e9 / hz).trunc() as i64 } else { 1_000_000_000 }
    };
    raw.max(1)
}

impl Scenario {
    pub fn from_json(text: &str) -> Scenario {
        let doc: Value = serde_json::from_str(text).expect("scenario.json is not valid JSON");
        Self::from_value(&doc)
    }

    pub fn from_value(doc: &Value) -> Scenario {
        let empty = Map::new();
        let obj = |v: &Value| v.as_object().cloned().unwrap_or_default();

        let exchange = obj(&doc["exchange_config"]);
        let oracle_params = doc["oracle_config"]
            .get("params")
            .and_then(Value::as_object)
            .unwrap_or(&empty)
            .clone();

        let proto = truthy(exchange.get("protocol_enforcement"));
        let stp_policy = (proto && truthy(exchange.get("stp_policy")))
            .then(|| exchange["stp_policy"].as_str().expect("stp_policy").to_string());
        let ack_delay_ns = if proto { i(&exchange, "ack_delay_ns", 0) } else { 0 };
        let compute_delay_ns = if proto { i(&exchange, "compute_delay_ns", 0) } else { 0 };

        let kappa_per_s = f(&oracle_params, "kappa", 0.0);
        let rate_per_s = f(&oracle_params, "jump_intensity", 0.0);
        let jump_sigma = f(&oracle_params, "jump_sigma", 0.0);
        let scheduled_jump = oracle_params
            .get("scheduled_jump")
            .filter(|v| truthy(Some(v)))
            .map(|j| ScheduledJump { time_ns: as_i64(&j["time_ns"]), magnitude: as_i64(&j["magnitude"]) });

        let latency = doc.get("latency_config").filter(|v| truthy(Some(v))).map(|lc| {
            let p = lc.get("params").and_then(Value::as_object).unwrap_or(&empty);
            let model = lc.get("model").and_then(Value::as_str).unwrap_or("deterministic");
            let mean_ns = f(p, "mean_ns", 0.0);
            let sigma = f(p, "sigma", 0.0);
            let min_ns = f(p, "min_ns", 0.0);
            let max_ns = f(p, "max_ns", 1e12);
            let alpha = f(p, "alpha", 1.5);
            let model = match model {
                "log_normal" => LatencyModel::LogNormal {
                    mu: if mean_ns > 0.0 { mean_ns.ln() } else { 0.0 },
                    sigma,
                },
                "uniform" => LatencyModel::Uniform,
                "pareto" => LatencyModel::Pareto { base: if min_ns > 0.0 { min_ns } else { 1.0 }, alpha },
                _ => LatencyModel::Constant(mean_ns),
            };
            Latency { model, min_ns, max_ns }
        });

        let mut agents = Vec::new();
        for cfg in doc["agent_configs"].as_array().expect("agent_configs") {
            let params = cfg.get("params").and_then(Value::as_object).unwrap_or(&empty);
            let kind = match cfg["agent_type"].as_str().expect("agent_type") {
                "NoiseTrader" => AgentKind::NoiseTrader,
                "MarketMaker" => AgentKind::MarketMaker,
                "ValueTrader" => AgentKind::ValueTrader,
                "MomentumTrader" => AgentKind::MomentumTrader,
                other => panic!("unsupported agent_type: {other:?}"),
            };
            let size_default = match kind {
                AgentKind::NoiseTrader => 10.0,
                AgentKind::ValueTrader => 25.0,
                AgentKind::MomentumTrader => 15.0,
                AgentKind::MarketMaker => 0.0,
            };
            let spec = AgentSpec {
                kind,
                interval_ns: interval_ns(params),
                order_size_mean: f(params, "order_size_mean", size_default),
                order_size_std: f(params, "order_size_std", 2.0),
                price_offset_ticks: i(params, "price_offset_ticks", 5),
                spread_ticks: i(params, "spread_ticks", 2).max(2),
                depth_levels: i(params, "depth_levels", 3).max(1),
                size_per_level: i(params, "size_per_level", 10).max(1),
                threshold_ticks: i(params, "threshold_ticks", 2),
                sigma_n: 1000.0, // _agent_kwargs never forwards sigma_n: always the class default
                lookback: i(params, "lookback", 5).max(1) as usize,
            };
            for _ in 0..as_i64(&cfg["count"]) {
                agents.push(spec.clone());
            }
        }

        Scenario {
            scenario_id: doc["scenario_id"].as_str().map(str::to_string).unwrap_or_else(|| doc["scenario_id"].to_string()),
            seed: u32::try_from(as_i64(&doc["seed"])).expect("seed must be in [0, 2**32)"),
            horizon_ns: as_i64(&doc["horizon_ns"]),
            reference_price: i(&oracle_params, "initial_price", 100_000),
            oracle_kappa_per_ns: if kappa_per_s > 0.0 { kappa_per_s / 1e9 } else { 1.67e-16 },
            oracle_fund_vol: f(&oracle_params, "sigma", 5e-5),
            megashock_lambda_per_ns: if rate_per_s > 0.0 { rate_per_s / 1e9 } else { 2.77778e-18 },
            megashock_mean: if jump_sigma != 0.0 { jump_sigma } else { 1000.0 },
            megashock_var: 50_000.0,
            scheduled_jump,
            stp_policy,
            ack_delay_ns,
            compute_delay_ns,
            latency,
            agents,
        }
    }
}

impl Scenario {
    /// Rough `(trace rows, ledger rows)` expected from the agents' wake counts, fitted on the
    /// public scenarios (within about 0.9x-1.5x there). Only used to decide which helper threads
    /// to create before a run; any error costs time, never correctness.
    pub fn estimated_rows(&self) -> (f64, f64) {
        let (mut trace, mut ledger) = (0.0, 0.0);
        for a in &self.agents {
            let wakes = self.horizon_ns.max(0) as f64 / a.interval_ns as f64;
            let (t, m) = match a.kind {
                AgentKind::MarketMaker => (10.0 * a.depth_levels as f64, 9.0 * a.depth_levels as f64),
                AgentKind::NoiseTrader => (5.0, 6.5),
                AgentKind::ValueTrader | AgentKind::MomentumTrader => (4.0, 6.5),
            };
            trace += wakes * t;
            ledger += wakes * m;
        }
        (trace, ledger)
    }

    /// Estimated draws from the noise traders' and value traders' own generators over the run
    /// (actions and noisy observations): the work a lookahead helper takes off the event loop.
    pub fn estimated_agent_draws(&self) -> f64 {
        self.agents
            .iter()
            .filter(|a| a.kind == AgentKind::NoiseTrader || (a.kind == AgentKind::ValueTrader && a.sigma_n != 0.0))
            .map(|a| self.horizon_ns.max(0) as f64 / a.interval_ns.max(1) as f64)
            .sum()
    }
}

/// Every RandomState the baseline derives from the global generator, in `build_config` order.
pub struct SeedChain {
    pub oracle: u32,
    /// The oracle constructor's `np.random.exponential` draw of the first megashock delta.
    pub first_megashock_delta: f64,
    pub exchange: u32,
    pub agents: Vec<u32>,
    pub latency: u32,
    /// Drawn (so it advances the global stream) but unused: `abides.run` seeds the kernel with 0.
    pub kernel: u32,
    /// The global generator after `build_config`; the oracle keeps drawing megashock deltas from it.
    pub global: RandomState,
}

impl SeedChain {
    pub fn derive(sc: &Scenario) -> SeedChain {
        let mut g = RandomState::new(sc.seed);
        let draw = |g: &mut RandomState| g.randint_u64(0, 1 << 32) as u32;
        let oracle = draw(&mut g);
        let first_megashock_delta = g.exponential(1.0 / sc.megashock_lambda_per_ns);
        let exchange = draw(&mut g);
        let agents = (0..sc.agents.len()).map(|_| draw(&mut g)).collect();
        let latency = draw(&mut g);
        let kernel = draw(&mut g);
        SeedChain { oracle, first_megashock_delta, exchange, agents, latency, kernel, global: g }
    }
}

impl Latency {
    /// `ScenarioLatencyModel.get_latency` for `sender != recipient` (self-sends are 0, no draw).
    #[inline]
    pub fn draw(&self, rs: &mut RandomState) -> i64 {
        let value = match self.model {
            LatencyModel::LogNormal { mu, sigma } => rs.lognormal(mu, sigma),
            LatencyModel::Uniform => rs.uniform(self.min_ns, self.max_ns),
            LatencyModel::Pareto { base, alpha } => base * (1.0 + rs.pareto(alpha)),
            LatencyModel::Constant(v) => v,
        };
        // np.clip == minimum(maximum(x, lo), hi); then int(round(.)) with half-to-even.
        crate::rng::py_round(value.max(self.min_ns).min(self.max_ns)) as i64
    }
}
