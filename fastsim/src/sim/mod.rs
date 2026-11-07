//! A bit-exact reimplementation of the Track 3 baseline: the ABIDES kernel, exchange, order book,
//! sparse mean-reverting oracle and the four `abides_fork` scenario agents.

mod agent_draws;
pub mod book;
pub mod engine;
mod latency;
mod latency_prefetch;
mod message;
pub mod oracle;
mod queue;
pub mod trace;

/// `pd.to_datetime("20210205").value` — the baseline's simulated trading day.
pub const DATE_NS: i64 = 1_612_483_200_000_000_000;
/// `str_to_ns("09:30:00")`.
pub const MKT_OPEN_OFFSET_NS: i64 = 34_200_000_000_000;
/// `str_to_ns("16:00:00")`: the ORACLE's close, distinct from the exchange's.
pub const ORACLE_CLOSE_OFFSET_NS: i64 = 57_600_000_000_000;
/// The config's `default_computation_delay`.
pub const DEFAULT_COMPUTATION_DELAY: i64 = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Bid => "BID",
            Side::Ask => "ASK",
        }
    }

    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// The fields of an ABIDES `LimitOrder` that reach a trace, a ledger or a decision.
#[derive(Clone, Copy, Debug)]
pub struct Order {
    pub order_id: i64,
    pub agent_id: u32,
    pub side: Side,
    pub limit_price: i64,
    pub quantity: i64,
    /// Only meaningful on execution reports.
    pub fill_price: i64,
}

/// Python truthiness of an optional price (`if bid:`): `None` and `0` are both false.
#[inline(always)]
pub fn truthy(p: Option<i64>) -> bool {
    matches!(p, Some(v) if v != 0)
}
