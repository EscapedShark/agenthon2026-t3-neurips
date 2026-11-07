//! The ABIDES kernel event loop plus the exchange and the four scenario agents.
//!
//! Every ordering-sensitive detail follows the Python:
//! * the queue is keyed `(deliver_at, sender, recipient, message_id)`, the tuple comparison
//!   `queue.PriorityQueue` performs; message ids come from one global counter bumped at every
//!   message construction (wakeups included), order ids from another;
//! * a message or wakeup reaching an agent that is still "in the future" is requeued at that time;
//! * the recipient's computation delay is added before `receive_message` but after `wakeup`, using
//!   the delay in force before the handler runs (the exchange resets its own inside the handler);
//! * a latency is drawn per send, in send order, and a broadcast is one message sent N times;
//! * the loop tests `current_time <= stop_time` before popping, so the first event past the stop
//!   time is still processed.

use std::collections::VecDeque;
use std::hint::select_unpredictable;

use super::agent_draws::{AgentDraws, NoiseParams};
use super::book::{Book, Outgoing, Report, Stp};
use super::latency::LatencyImpl;
use super::latency_prefetch::{EarlyPrefetch, LatencySampler};
use super::message::{Msg, Payload};
use super::oracle::{Oracle, OracleParams};
use super::queue::{EventQueue, Lane, MAX_WAKEUP_CLASSES};
use super::trace::{LedgerBuilder, LedgerRow, MsgKind, NONE, NULL, RawTrace, TraceBuilder, TraceType};
use super::{
    DATE_NS, DEFAULT_COMPUTATION_DELAY, MKT_OPEN_OFFSET_NS, ORACLE_CLOSE_OFFSET_NS, Order, Side,
    truthy,
};
use crate::rng::{RandomState, py_round};
use crate::scenario::{AgentKind, AgentSpec, LatencyModel, Scenario, SeedChain};

const EXCHANGE: u32 = 0;

/// The message slot of a wakeup, which has none (see `Sim::set_wakeup`).
const NO_MSG: u32 = u32::MAX;

/// Put `msg` in a free slot of the message slab (reusing the most recently freed one).
#[inline]
fn alloc(slab: &mut Vec<Msg>, free: &mut Vec<u32>, msg: Msg) -> u32 {
    match free.pop() {
        Some(slot) => {
            *ix_mut(slab, slot as usize) = msg;
            slot
        }
        None => {
            slab.push(msg);
            (slab.len() - 1) as u32
        }
    }
}

/// The exchange's side of the kernel while the order book works: it sends each report the book
/// emits at once (`ExchangeAgent.send_message` adds the pipeline delay), exactly as the kernel's
/// `send` would, so the book borrows nothing it needs.
struct Reports<'a> {
    heap: &'a mut EventQueue,
    slab: &'a mut Vec<Msg>,
    free: &'a mut Vec<u32>,
    next_msg_id: &'a mut u64,
    latency: &'a mut LatencySampler,
    /// `now + delay[EXCHANGE] + pipeline delay`: the send time of every report.
    sent: i64,
    causal: u32,
}

impl Reports<'_> {
    #[inline]
    fn send(&mut self, o: Outgoing) {
        let kind = match o.report {
            Report::Accepted => MsgKind::OrderAccepted,
            Report::Executed => MsgKind::OrderExecuted,
            Report::Cancelled => MsgKind::OrderCancelled,
        };
        let msg_id = *self.next_msg_id;
        *self.next_msg_id += 1;
        let deliver_at = self.sent + self.latency.draw(EXCHANGE, o.to);
        let slot = alloc(self.slab, self.free, Msg {
            payload: Payload::order(kind, o.order),
            t_send: self.sent,
            t_recv: deliver_at,
            causal_parent: self.causal,
        });
        self.heap.push_parts(Lane::Message, deliver_at, EXCHANGE, o.to, msg_id, slot);
    }
}

/// `v[i]` for an index the kernel issued itself (an agent id taken from a message key, or a slab
/// slot): unchecked in release builds, checked in debug builds.
#[inline(always)]
fn ix<T>(v: &[T], i: usize) -> &T {
    debug_assert!(i < v.len());
    // SAFETY: agent ids and slots in keys come from this kernel and are always in range.
    unsafe { v.get_unchecked(i) }
}

#[inline(always)]
fn ix_mut<T>(v: &mut [T], i: usize) -> &mut T {
    debug_assert!(i < v.len());
    // SAFETY: as for `ix`.
    unsafe { v.get_unchecked_mut(i) }
}

/// A trader's state the event loop reads and writes. Its generator lives apart (see
/// `Trader::rng_slot`): 2.5 kB each, and unused while a helper draws ahead, it would spread the
/// traders over many cache lines.
struct Trader {
    spec: AgentSpec,
    mkt_open: Option<i64>,
    mkt_close: Option<i64>,
    mkt_closed: bool,
    first_wake: bool,
    awaiting_spread: bool,
    known_bid: Option<i64>,
    known_ask: Option<i64>,
    /// MarketMaker only: `TradingAgent.orders`, in insertion (= order id) order. An order that
    /// leaves keeps its place with the id [`GONE`] until the agent next acts: a market maker
    /// cancels every order each time, and shifting the rest down for each confirmation cost more
    /// than all its other bookkeeping.
    open_orders: Vec<Order>,
    /// MomentumTrader only.
    mid_history: VecDeque<f64>,
    /// Index of the trader's generator in `Sim::noise_rngs` (noise traders) or `Sim::value_rngs`
    /// (value traders with observation noise), which is also its slot in the run's
    /// [`AgentDraws`]; `NO_SLOT` for traders that never draw.
    rng_slot: u32,
    /// Its draws come from the run's [`AgentDraws`] until they run out; and how many it has taken.
    ahead: bool,
    draws_taken: u32,
}

const NO_SLOT: u32 = u32::MAX;

/// The id of an order that has left `Trader::open_orders` (real ids are never negative).
const GONE: i64 = -1;

pub struct SimOutput {
    pub trace: Vec<super::trace::TraceRow>,
    pub ledger: Vec<LedgerRow>,
}

pub struct Sim {
    start_time: i64,
    stop_time: i64,
    mkt_open: i64,
    mkt_close: i64,
    reference_price: i64,
    now: i64,
    heap: EventQueue,
    slab: Vec<Msg>,
    free: Vec<u32>,
    next_msg_id: u64,
    next_order_id: i64,
    busy_until: Vec<i64>,
    delay: Vec<i64>,
    /// The message being handled (the causal parent of what it sends), [`NONE`] before any.
    causal: u32,
    latency: LatencySampler,
    /// Noise and value traders' draws, computed ahead on a helper thread (see [`AgentDraws`]).
    draws: Option<AgentDraws>,
    // exchange
    ex_computation_delay: i64,
    ex_pipeline_delay: i64,
    stp: Option<Stp>,
    close_subs: Vec<u32>,
    book: Book,
    traders: Vec<Trader>,
    /// The generators of the traders that draw, by `Trader::rng_slot`. Lent to the run's
    /// [`AgentDraws`] (`rngs_lent`) while it draws for them.
    noise_rngs: Vec<RandomState>,
    value_rngs: Vec<RandomState>,
    rngs_lent: bool,
    oracle: Oracle,
    trace: RawTrace,
    ledger: LedgerBuilder,
}

/// What `Sim::run` hands back: the logged trace calls and the ledger.
pub struct Finished {
    pub trace: RawTrace,
    pub ledger: LedgerBuilder,
}

/// Every trader re-arms its wakeup `interval_ns` after each one (see `Sim::trader_wakeup`): the
/// traders of each interval form a wakeup class for the event queue (by agent id, the exchange in
/// none), up to the queue's limit on classes.
fn wakeup_classes(traders: &[Trader]) -> Vec<u8> {
    let mut intervals: Vec<i64> = Vec::new();
    let mut class_of = vec![u8::MAX];
    for t in traders {
        let class = match intervals.iter().position(|&i| i == t.spec.interval_ns) {
            Some(c) => c as u8,
            None if intervals.len() < MAX_WAKEUP_CLASSES => {
                intervals.push(t.spec.interval_ns);
                (intervals.len() - 1) as u8
            }
            None => u8::MAX,
        };
        class_of.push(class);
    }
    class_of
}

/// The market's open and close times in a run of `sc`.
pub fn market_hours(sc: &Scenario) -> (i64, i64) {
    let mkt_open = DATE_NS + MKT_OPEN_OFFSET_NS;
    (mkt_open, mkt_open + sc.horizon_ns)
}

/// A latency lookahead started on a scenario's latency stream before its simulation is built
/// (see [`Sim::with_early_latency_prefetch`]).
pub struct EarlyLatency(EarlyPrefetch);

impl EarlyLatency {
    /// Start drawing `sc`'s latency samples on a helper thread now, from where the run's own
    /// first draws will stop (on the stream `Sim::new` seeds from the same chain), if its model
    /// is one the lookahead serves (log-normal, Pareto).
    pub fn start(sc: &Scenario) -> Option<EarlyLatency> {
        let lat = sc.latency.as_ref()?;
        EarlyPrefetch::start(LatencyImpl::scenario(lat), SeedChain::derive(sc).latency).map(EarlyLatency)
    }
}

impl Sim {
    /// A simulation of `sc` recording into `trace` and `ledger` (see their streaming options).
    pub fn new(sc: &Scenario, trace: RawTrace, ledger: LedgerBuilder) -> Sim {
        let chain = SeedChain::derive(sc);
        let (mkt_open, mkt_close) = market_hours(sc);
        let oracle = Oracle::new(
            &OracleParams {
                mkt_open,
                mkt_close: DATE_NS + ORACLE_CLOSE_OFFSET_NS,
                r_bar: sc.reference_price,
                kappa: sc.oracle_kappa_per_ns,
                fund_vol: sc.oracle_fund_vol,
                megashock_lambda: sc.megashock_lambda_per_ns,
                megashock_mean: sc.megashock_mean,
                megashock_var: sc.megashock_var,
                scheduled_jump: sc
                    .scheduled_jump
                    .as_ref()
                    .map(|j| (mkt_open + j.time_ns, j.magnitude)),
            },
            chain.oracle,
            chain.first_megashock_delta,
            chain.global.clone(),
        );
        // Only noise traders and value traders with observation noise draw from their generators.
        let (mut noise_seeds, mut value_seeds) = (Vec::new(), Vec::new());
        let traders = sc
            .agents
            .iter()
            .zip(&chain.agents)
            .map(|(spec, &seed)| Trader {
                rng_slot: match spec.kind {
                    AgentKind::NoiseTrader => {
                        noise_seeds.push(seed);
                        noise_seeds.len() as u32 - 1
                    }
                    AgentKind::ValueTrader if spec.sigma_n != 0.0 => {
                        value_seeds.push(seed);
                        value_seeds.len() as u32 - 1
                    }
                    _ => NO_SLOT,
                },
                spec: spec.clone(),
                mkt_open: None,
                mkt_close: None,
                mkt_closed: false,
                first_wake: true,
                awaiting_spread: false,
                known_bid: None,
                known_ask: None,
                open_orders: Vec::new(),
                mid_history: VecDeque::new(),
                ahead: false,
                draws_taken: 0,
            })
            .collect::<Vec<_>>();
        let n_agents = traders.len() + 1;
        let stp = sc.stp_policy.as_deref().map(|p| {
            if p == "cancel_oldest" {
                Stp::CancelOldest
            } else {
                Stp::CancelNewest
            }
        });
        // With a constant latency messages arrive in about send order: keep them ordered.
        let mut heap = EventQueue::with_ordered_messages(
            sc.latency.as_ref().is_some_and(|lat| matches!(lat.model, LatencyModel::Constant(_))),
        );
        heap.set_wakeup_classes(wakeup_classes(&traders));
        Sim {
            start_time: DATE_NS,
            stop_time: mkt_close + 1_000_000_000,
            mkt_open,
            mkt_close,
            reference_price: sc.reference_price,
            now: DATE_NS,
            heap,
            slab: Vec::with_capacity(1024),
            free: Vec::with_capacity(1024),
            next_msg_id: 1,
            next_order_id: 0,
            busy_until: vec![DATE_NS; n_agents],
            delay: vec![DEFAULT_COMPUTATION_DELAY; n_agents],
            causal: NONE,
            latency: LatencySampler::new(match &sc.latency {
                Some(lat) => LatencyImpl::scenario(lat),
                None => LatencyImpl::pairwise(n_agents, chain.latency),
            }, chain.latency),
            ex_computation_delay: sc.compute_delay_ns,
            ex_pipeline_delay: sc.ack_delay_ns,
            stp,
            close_subs: Vec::new(),
            book: Book::new(),
            traders,
            noise_rngs: RandomState::new_many(&noise_seeds),
            value_rngs: RandomState::new_many(&value_seeds),
            rngs_lent: false,
            oracle,
            draws: None,
            trace,
            ledger,
        }
    }

    /// Populate the message slab and event queues before the run (see [`crate::mem`]), so the
    /// event loop's first writes to them do not fault.
    pub fn prefault(&mut self) {
        crate::mem::prefault(&mut self.slab, 1024);
        crate::mem::prefault(&mut self.free, 1024);
        self.heap.prefault(1024);
    }

    /// Take the latency samples from `early`, a lookahead started on `sc`'s latency stream before
    /// the simulation was built (see [`EarlyLatency::start`]).
    pub fn with_early_latency_prefetch(mut self, early: EarlyLatency) -> Self {
        self.latency.use_early(early.0);
        self
    }

    /// Reserve a caller-provided spare CPU for exact latency lookahead. `prepare` creates the
    /// idle producer thread now (for runs expected to draw many samples); otherwise it is spawned
    /// after sustained demand.
    pub fn with_latency_prefetch(mut self, prepare: bool) -> Self {
        self.latency.enable_prefetch(prepare);
        self
    }

    /// Have a helper thread compute the noise and value traders' draws ahead (see
    /// [`AgentDraws`]), with room for every wakeup the horizon allows.
    pub fn with_agent_draws(self) -> Self {
        let span = (self.stop_time - self.mkt_open).max(0) as f64;
        self.with_agent_draws_capped(|interval| (span / interval.max(1) as f64).ceil() as usize + 8)
    }

    /// [`Sim::with_agent_draws`] with room for `cap(interval)` draws per agent.
    fn with_agent_draws_capped(mut self, cap: impl Fn(i64) -> usize) -> Self {
        // The helper packs a noise trader's price offset into 63 bits: all of them or none.
        let noise_ahead = self
            .traders
            .iter()
            .all(|t| t.spec.kind != AgentKind::NoiseTrader || t.spec.price_offset_ticks < 1 << 60);
        // Each drawing trader's room and interval, by slot (traders are in slot order): agents of
        // different intervals are drawn for in groups, each as far as its own wakeups go.
        let mut params = Vec::new();
        let (mut noise_rounds, mut value_rounds) = (Vec::new(), Vec::new());
        for t in &mut self.traders {
            match t.spec.kind {
                AgentKind::NoiseTrader if noise_ahead => {
                    noise_rounds.push((cap(t.spec.interval_ns), t.spec.interval_ns));
                    params.push(NoiseParams {
                        mean: t.spec.order_size_mean,
                        std: t.spec.order_size_std,
                        offset_ticks: t.spec.price_offset_ticks,
                    });
                    t.ahead = true;
                }
                AgentKind::ValueTrader if t.rng_slot != NO_SLOT => {
                    value_rounds.push((cap(t.spec.interval_ns), t.spec.interval_ns));
                    t.ahead = true;
                }
                _ => {}
            }
        }
        let noise = if noise_ahead { std::mem::take(&mut self.noise_rngs) } else { Vec::new() };
        let value = std::mem::take(&mut self.value_rngs);
        self.rngs_lent = true;
        self.draws = AgentDraws::start(noise, params, &noise_rounds, value, &value_rounds);
        if self.draws.is_none() {
            // Nothing to draw: nobody draws ahead, and the (empty) generators stay here.
            self.rngs_lent = false;
            for t in &mut self.traders {
                t.ahead = false;
            }
        }
        self
    }

    /// A trader's lookahead ran out: from now on the traders draw from their generators again,
    /// each continuing where the helper stopped once its own lookahead is used up.
    #[cold]
    #[inline(never)]
    fn take_back_generators(&mut self) {
        if !std::mem::replace(&mut self.rngs_lent, false) {
            return;
        }
        let draws = self.draws.as_mut().expect("lent generators imply draws");
        let (noise, value) = draws.finals();
        if !noise.is_empty() {
            self.noise_rngs = noise;
        }
        if !value.is_empty() {
            self.value_rngs = value;
        }
    }

    // ------------------------------------------------------------------ kernel

    #[inline]
    fn alloc(&mut self, msg: Msg) -> u32 {
        alloc(&mut self.slab, &mut self.free, msg)
    }

    #[inline]
    fn new_msg_id(&mut self) -> u64 {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        id
    }

    /// `Kernel.send_message` for an already-constructed message (`msg_id`).
    #[inline]
    fn send_with_id(
        &mut self,
        msg_id: u64,
        sender: u32,
        recipient: u32,
        payload: Payload,
        delay: i64,
    ) {
        debug_assert_ne!(sender, recipient, "only a wakeup is addressed to its sender");
        let sent = self.now + *ix(&self.delay, sender as usize) + delay;
        let latency = self.latency.draw(sender, recipient);
        let deliver_at = sent + latency;
        let slot = self.alloc(Msg {
            payload,
            t_send: sent,
            t_recv: deliver_at,
            causal_parent: self.causal,
        });
        self.heap.push_parts(Lane::Message, deliver_at, sender, recipient, msg_id, slot);
    }

    #[inline]
    fn send(&mut self, sender: u32, recipient: u32, payload: Payload, delay: i64) {
        let id = self.new_msg_id();
        self.send_with_id(id, sender, recipient, payload, delay);
    }

    /// `Kernel.set_wakeup`. A wakeup is the one event an agent addresses to itself, and its key
    /// says all there is to it, so it takes no message slot.
    #[inline]
    fn set_wakeup(&mut self, agent: u32, time: i64) {
        let id = self.new_msg_id();
        self.heap.push_parts(Lane::Wakeup, time, agent, agent, id, NO_MSG);
    }

    /// Run to completion; returns the logged trace calls and the ledger. The trace is finalized
    /// by replaying the log (see [`RawTrace`]), outside the event loop. Dropping the `Sim`
    /// afterwards stops its helper threads.
    pub fn run(&mut self) -> Finished {
        self.latency.start();
        // kernel_initializing: the exchange schedules its market-close wakeup (message id 1).
        self.set_wakeup(EXCHANGE, self.mkt_close);
        // kernel_starting: every agent, in id order, asks to wake at the start time.
        for id in 0..self.busy_until.len() as u32 {
            self.set_wakeup(id, self.start_time);
        }
        self.now = self.start_time;

        while self.now <= self.stop_time {
            crate::marks::count(crate::marks::ctr::LOOP, 1);
            let Some((pos, key)) = self.heap.peek() else {
                break;
            };
            self.now = key.time;
            let r = key.recipient() as usize;
            let busy_until = *ix(&self.busy_until, r);
            if busy_until > self.now {
                self.heap.defer_busy(pos, key, busy_until);
                continue;
            }
            self.heap.pop(pos);
            // Agents message only the exchange, and the exchange only them: an event an agent
            // addresses to itself is a wakeup (see `set_wakeup`). The recipient is busy until its
            // computation delay after this event: for a message the delay in force before the
            // handler (the exchange resets its own inside it, for the next message), for a wakeup
            // the delay after it. Nothing reads `busy_until` while a handler runs.
            if key.sender() as usize == r {
                self.causal = key.msg_id as u32;
                if self.ledger.on {
                    self.ledger.push(LedgerRow {
                        t_recv: self.now,
                        t_send: NULL,
                        message_id: key.msg_id as u32,
                        order_id: NONE,
                        causal_parent: NONE,
                        src: r as u32,
                        dst: r as u32,
                        kind: MsgKind::Wakeup,
                    });
                }
                if r == 0 {
                    self.exchange_wakeup();
                } else {
                    self.trader_wakeup(r);
                }
                *ix_mut(&mut self.busy_until, r) = self.now + *ix(&self.delay, r);
            } else {
                let msg = *ix(&self.slab, key.slot as usize);
                self.free.push(key.slot);
                let kind = msg.payload.kind();
                *ix_mut(&mut self.busy_until, r) = self.now + *ix(&self.delay, r);
                self.causal = key.msg_id as u32;
                if self.ledger.on {
                    let order_id = msg.payload.order_id().map_or(NONE, |id| id as u32);
                    self.ledger.push(LedgerRow {
                        t_recv: msg.t_recv,
                        t_send: msg.t_send,
                        message_id: key.msg_id as u32,
                        order_id,
                        causal_parent: msg.causal_parent,
                        src: key.sender(),
                        dst: r as u32,
                        kind,
                    });
                }
                if r == 0 {
                    self.exchange_receive(key.sender(), msg);
                } else {
                    self.trader_receive(r, msg);
                }
            }
        }
        Finished {
            // Placeholders that allocate nothing: this still runs inside the timed window.
            trace: std::mem::replace(&mut self.trace, RawTrace::empty()),
            ledger: std::mem::replace(&mut self.ledger, LedgerBuilder::new(false)),
        }
    }

    // ------------------------------------------------------------------ exchange

    fn exchange_wakeup(&mut self) {
        if self.now >= self.mkt_close {
            // One MarketClosePriceMsg object, sent to every subscriber in subscription order.
            let id = self.new_msg_id();
            for i in 0..self.close_subs.len() {
                let to = self.close_subs[i];
                self.send_with_id(
                    id,
                    EXCHANGE,
                    to,
                    Payload::signal(MsgKind::MarketClosePrice),
                    0,
                );
            }
        }
    }

    fn exchange_receive(&mut self, sender: u32, msg: Msg) {
        self.delay[0] = self.ex_computation_delay;
        let kind = msg.payload.kind();
        if self.now > self.mkt_close && kind != MsgKind::QuerySpread {
            self.send(EXCHANGE, sender, Payload::signal(MsgKind::MarketClosed), 0);
            return;
        }
        match kind {
            MsgKind::MarketHoursRequest => {
                self.delay[0] = 0;
                let p = Payload::hours(self.mkt_open, self.mkt_close);
                self.send(EXCHANGE, sender, p, 0);
            }
            MsgKind::MarketClosePriceRequest => self.close_subs.push(sender),
            MsgKind::QuerySpread => {
                let p = Payload::spread(
                    self.book.best_bid().map(|(price, _)| price),
                    self.book.best_ask().map(|(price, _)| price),
                    self.now > self.mkt_close,
                );
                self.send(EXCHANGE, sender, p, 0);
            }
            MsgKind::LimitOrder => {
                let order = msg.payload.order_value();
                let (book, stp, mut reports) = self.book_and_reports();
                let accepted = book.handle_limit_order(order, stp, &mut |o| reports.send(o));
                if accepted {
                    match (self.book.best_bid(), self.book.best_ask()) {
                        (Some(bid), Some(ask)) => self.trace.quotes(self.now, bid, ask),
                        (bid, ask) => {
                            if let Some((p, q)) = bid {
                                self.trace.quote(self.now, Side::Bid, p, q);
                            }
                            if let Some((p, q)) = ask {
                                self.trace.quote(self.now, Side::Ask, p, q);
                            }
                        }
                    }
                }
            }
            MsgKind::CancelOrder => {
                let order = msg.payload.order_value();
                let (book, _, mut reports) = self.book_and_reports();
                book.cancel(order.side, order.limit_price, order.order_id, order.agent_id, &mut |o| reports.send(o));
            }
            _ => {}
        }
    }

    /// The order book and the kernel parts that send its reports (see [`Reports`]), borrowed
    /// apart so the book can hand over each report as it arises.
    #[inline]
    fn book_and_reports(&mut self) -> (&mut Book, Option<Stp>, Reports<'_>) {
        let sent = self.now + *ix(&self.delay, EXCHANGE as usize) + self.ex_pipeline_delay;
        let Sim { book, heap, slab, free, next_msg_id, latency, stp, causal, .. } = self;
        (book, *stp, Reports { heap, slab, free, next_msg_id, latency, sent, causal: *causal })
    }

    // ------------------------------------------------------------------ traders

    fn trader_wakeup(&mut self, r: usize) {
        let id = r as u32;
        let t = ix_mut(&mut self.traders, r - 1);
        let first = std::mem::replace(&mut t.first_wake, false);
        let need_hours = t.mkt_open.is_none();
        if first {
            self.send(
                id,
                EXCHANGE,
                Payload::signal(MsgKind::MarketClosePriceRequest),
                0,
            );
        }
        if need_hours {
            self.send(
                id,
                EXCHANGE,
                Payload::signal(MsgKind::MarketHoursRequest),
                0,
            );
        }
        let t = ix(&self.traders, r - 1);
        if !truthy(t.mkt_open) || !truthy(t.mkt_close) || t.mkt_closed {
            return;
        }
        let next = self.now + t.spec.interval_ns;
        self.set_wakeup(id, next);
        self.send(id, EXCHANGE, Payload::signal(MsgKind::QuerySpread), 0);
        let t = ix_mut(&mut self.traders, r - 1);
        t.awaiting_spread = true;
        if t.ahead
            && t.spec.kind == AgentKind::NoiseTrader
            && let Some(draws) = &self.draws
        {
            draws.prefetch_noise(t.rng_slot as usize, t.draws_taken as usize);
        }
    }

    fn trader_receive(&mut self, r: usize, msg: Msg) {
        let now = self.now;
        let t = ix_mut(&mut self.traders, r - 1);
        match msg.payload.kind() {
            // Only market hours can complete the agent's knowledge of them; then it schedules its
            // first wakeup at the open (`+ get_wake_frequency()`, which is 0).
            MsgKind::MarketHours => {
                let (open, close) = msg.payload.hours_value();
                let had_hours = t.mkt_open.is_some() && t.mkt_close.is_some();
                t.mkt_open = Some(open);
                t.mkt_close = Some(close);
                if !had_hours {
                    self.set_wakeup(r as u32, open);
                }
            }
            MsgKind::MarketClosed => t.mkt_closed = true,
            MsgKind::OrderExecuted => {
                let o = msg.payload.order_value();
                self.trace.order_event(now, TraceType::Executed, &o);
                if let Some(pos) = t.open_orders.iter().position(|x| x.order_id == o.order_id) {
                    if o.quantity >= t.open_orders[pos].quantity {
                        t.open_orders[pos].order_id = GONE;
                    } else {
                        t.open_orders[pos].quantity -= o.quantity;
                    }
                }
            }
            MsgKind::OrderAccepted => {
                self.trace
                    .order_event(now, TraceType::Accepted, &msg.payload.order_value());
            }
            MsgKind::OrderCancelled => {
                let o = msg.payload.order_value();
                self.trace.order_event(now, TraceType::Cancelled, &o);
                if let Some(pos) = t.open_orders.iter().position(|x| x.order_id == o.order_id) {
                    t.open_orders[pos].order_id = GONE;
                }
            }
            // The answer to the agent's spread query, the one message it acts on.
            MsgKind::QuerySpreadResponse => {
                let (bid, ask, mkt_closed) = msg.payload.spread_value();
                if mkt_closed {
                    t.mkt_closed = true;
                }
                t.known_bid = bid;
                t.known_ask = ask;
                if t.awaiting_spread {
                    if !t.mkt_closed {
                        self.act(r);
                    }
                    ix_mut(&mut self.traders, r - 1).awaiting_spread = false;
                }
            }
            _ => {}
        }
    }

    /// `TradingAgent.place_limit_order` (risk checks are off: `ignore_risk=True`).
    fn place(&mut self, r: usize, side: Side, limit_price: i64, quantity: i64) {
        let order_id = self.next_order_id;
        self.next_order_id += 1;
        let order = Order {
            order_id,
            agent_id: r as u32,
            side,
            limit_price,
            quantity,
            fill_price: 0,
        };
        let t = ix_mut(&mut self.traders, r - 1);
        if t.spec.kind == AgentKind::MarketMaker {
            t.open_orders.push(order);
        }
        self.send(
            r as u32,
            EXCHANGE,
            Payload::order(MsgKind::LimitOrder, order),
            0,
        );
        self.trace
            .order_event(self.now, TraceType::Submitted, &order);
    }

    fn act(&mut self, r: usize) {
        let t = ix_mut(&mut self.traders, r - 1);
        let (bid, ask) = (t.known_bid, t.known_ask);
        match t.spec.kind {
            AgentKind::NoiseTrader => {
                let slot = t.rng_slot as usize;
                let ahead = match &mut self.draws {
                    Some(draws) if t.ahead => draws.noise(slot, t.draws_taken as usize),
                    _ => None,
                };
                let (size, buy, offset) = if let Some(d) = ahead {
                    t.draws_taken += 1;
                    (d.size(), d.buy(), d.offset())
                } else {
                    if std::mem::replace(&mut t.ahead, false) {
                        self.take_back_generators();
                    }
                    let spec = &ix(&self.traders, r - 1).spec;
                    let rng = ix_mut(&mut self.noise_rngs, slot);
                    let size = (py_round(rng.normal(spec.order_size_mean, spec.order_size_std)) as i64).max(1);
                    let buy = rng.randint(0, 2) != 0;
                    let offset = rng.randint(0, spec.price_offset_ticks + 1);
                    (size, buy, offset)
                };
                let rp = self.reference_price;
                // A buy is anchored at the ask, a sell at the bid (each falling back to the other
                // side, then the reference price). Both are worked out and the side picked
                // without a branch: it is a coin toss.
                let ask_first = if truthy(ask) {
                    ask.unwrap()
                } else if truthy(bid) {
                    bid.unwrap()
                } else {
                    rp
                };
                let bid_first = if truthy(bid) {
                    bid.unwrap()
                } else if truthy(ask) {
                    ask.unwrap()
                } else {
                    rp
                };
                let (side, price) = select_unpredictable(
                    buy,
                    (Side::Bid, ask_first.wrapping_add(offset)),
                    (Side::Ask, bid_first.wrapping_sub(offset)),
                );
                self.place(r, side, price, size);
            }
            AgentKind::MarketMaker => {
                let mid = if truthy(bid) && truthy(ask) {
                    (bid.unwrap() + ask.unwrap()).div_euclid(2)
                } else {
                    self.reference_price
                };
                // cancel_all_orders(): every order the agent believes open, in insertion order.
                t.open_orders.retain(|o| o.order_id != GONE);
                let n_open = t.open_orders.len();
                for i in 0..n_open {
                    let o = ix(&self.traders, r - 1).open_orders[i];
                    self.send(
                        r as u32,
                        EXCHANGE,
                        Payload::order(MsgKind::CancelOrder, o),
                        0,
                    );
                }
                let spec = &ix(&self.traders, r - 1).spec;
                let (half, depth, size) = (
                    spec.spread_ticks.div_euclid(2),
                    spec.depth_levels,
                    spec.size_per_level,
                );
                for lvl in 0..depth {
                    self.place(r, Side::Bid, mid - half - lvl, size);
                    self.place(r, Side::Ask, mid + half + lvl, size);
                }
            }
            AgentKind::ValueTrader => {
                let mid = if truthy(bid) && truthy(ask) {
                    (bid.unwrap() + ask.unwrap()) as f64 / 2.0
                } else if truthy(bid) {
                    bid.unwrap() as f64
                } else if truthy(ask) {
                    ask.unwrap() as f64
                } else {
                    return;
                };
                let sigma_n = t.spec.sigma_n;
                let slot = t.rng_slot as usize;
                let ahead = match &mut self.draws {
                    Some(draws) if t.ahead => draws.z(slot, t.draws_taken as usize),
                    _ => None,
                };
                if ahead.is_none() && std::mem::replace(&mut t.ahead, false) {
                    self.take_back_generators();
                }
                let t = ix_mut(&mut self.traders, r - 1);
                let fundamental = match ahead {
                    Some(z) => {
                        t.draws_taken += 1;
                        self.oracle.observe_price(self.now, sigma_n, || z)
                    }
                    // Without observation noise nothing is drawn (and there is no generator).
                    None => {
                        let rngs = &mut self.value_rngs;
                        self.oracle.observe_price(self.now, sigma_n, || ix_mut(rngs, slot).standard_normal())
                    }
                };
                let size = (py_round(t.spec.order_size_mean) as i64).max(1);
                let threshold = t.spec.threshold_ticks;
                if mid < (fundamental - threshold) as f64 && truthy(ask) {
                    self.place(r, Side::Bid, ask.unwrap(), size);
                } else if mid > (fundamental + threshold) as f64 && truthy(bid) {
                    self.place(r, Side::Ask, bid.unwrap(), size);
                }
            }
            AgentKind::MomentumTrader => {
                let mid = if truthy(bid) && truthy(ask) {
                    (bid.unwrap() + ask.unwrap()) as f64 / 2.0
                } else if truthy(bid) {
                    bid.unwrap() as f64
                } else if truthy(ask) {
                    ask.unwrap() as f64
                } else {
                    return;
                };
                let lookback = t.spec.lookback;
                t.mid_history.push_back(mid);
                if t.mid_history.len() > lookback + 1 {
                    t.mid_history.pop_front();
                }
                if t.mid_history.len() <= lookback {
                    return;
                }
                let past = t.mid_history[0];
                let size = (py_round(t.spec.order_size_mean) as i64).max(1);
                let threshold = t.spec.threshold_ticks as f64;
                if mid > past + threshold && truthy(ask) {
                    self.place(r, Side::Bid, ask.unwrap(), size);
                } else if mid < past - threshold && truthy(bid) {
                    self.place(r, Side::Ask, bid.unwrap(), size);
                }
            }
        }
    }
}

/// Run one scenario to completion, keeping every trace and ledger row in memory.
pub fn run_scenario(sc: &Scenario) -> SimOutput {
    run_scenario_with(sc, |sim| sim)
}

fn run_scenario_with(sc: &Scenario, prepare: impl FnOnce(Sim) -> Sim) -> SimOutput {
    let run = prepare(Sim::new(sc, RawTrace::new(), LedgerBuilder::new(true))).run();
    let mut trace = TraceBuilder::new(EXCHANGE as i32);
    for row in &run.trace.into_rows() {
        trace.replay(row);
    }
    trace.finish();
    SimOutput {
        trace: trace.rows,
        ledger: run.ledger.rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(out: &SimOutput) -> (String, String) {
        (format!("{:?}", out.trace), format!("{:?}", out.ledger))
    }

    /// Draws computed ahead give the same run as draws taken in place, including when the
    /// lookahead runs out mid-run and agents continue from the helper's generators.
    #[test]
    fn agent_draws_ahead_match_draws_in_place() {
        let doc = serde_json::json!({
            "scenario_id": "agent-draws", "seed": 7, "horizon_ns": 3_000_000_000i64,
            "agent_configs": [
                {"agent_type": "NoiseTrader", "count": 10, "params": {"arrival_rate_hz": 25, "price_offset_ticks": 4}},
                {"agent_type": "ValueTrader", "count": 4, "params": {"arrival_rate_hz": 10}},
                {"agent_type": "MomentumTrader", "count": 2, "params": {"arrival_rate_hz": 10, "lookback": 3}},
                {"agent_type": "MarketMaker", "count": 2, "params": {"rebalance_interval_ns": 100_000_000, "depth_levels": 2}}
            ],
            "latency_config": {"model": "log_normal", "params": {"mean_ns": 5000, "sigma": 0.75}},
            "exchange_config": {"symbol": "SYM", "tick_size": 1}
        });
        let sc = Scenario::from_value(&doc);
        let plain = rows(&run_scenario(&sc));
        assert!(plain.0.len() > 10_000, "the scenario trades");
        assert_eq!(rows(&run_scenario_with(&sc, Sim::with_agent_draws)), plain);
        for cap in [0, 1, 3, 50] {
            let out = run_scenario_with(&sc, |sim| sim.with_agent_draws_capped(|_| cap));
            assert_eq!(rows(&out), plain, "lookahead of {cap} draws per agent");
        }
    }

    /// Traders of one kind at different intervals are drawn for in groups, each as far as its own
    /// wakeups go: the run is the same as with draws taken in place, also when some groups' room
    /// runs out mid-run and others' does not.
    #[test]
    fn mixed_interval_agent_draws_match_draws_in_place() {
        let doc = serde_json::json!({
            "scenario_id": "agent-draws-mixed", "seed": 11, "horizon_ns": 2_000_000_000i64,
            "agent_configs": [
                {"agent_type": "NoiseTrader", "count": 1, "params": {"arrival_rate_hz": 400, "price_offset_ticks": 4}},
                {"agent_type": "NoiseTrader", "count": 12, "params": {"arrival_rate_hz": 2, "price_offset_ticks": 6}},
                {"agent_type": "NoiseTrader", "count": 3, "params": {"arrival_rate_hz": 40, "order_size_mean": 30}},
                {"agent_type": "ValueTrader", "count": 2, "params": {"arrival_rate_hz": 50}},
                {"agent_type": "ValueTrader", "count": 5, "params": {"arrival_rate_hz": 1}},
                {"agent_type": "MarketMaker", "count": 2, "params": {"rebalance_interval_ns": 100_000_000, "depth_levels": 2}}
            ],
            "latency_config": {"model": "log_normal", "params": {"mean_ns": 5000, "sigma": 0.75}},
            "exchange_config": {"symbol": "SYM", "tick_size": 1}
        });
        let sc = Scenario::from_value(&doc);
        let plain = rows(&run_scenario(&sc));
        assert!(plain.0.len() > 10_000, "the scenario trades");
        assert_eq!(rows(&run_scenario_with(&sc, Sim::with_agent_draws)), plain);
        // Room by interval: the fast agents' runs out early, the slow agents' never.
        for fast_room in [0, 1, 7, 200] {
            let out = run_scenario_with(&sc, |sim| {
                sim.with_agent_draws_capped(|interval| if interval < 100_000_000 { fast_room } else { 1_000 })
            });
            assert_eq!(rows(&out), plain, "lookahead of {fast_room} draws for the fast groups");
        }
    }
}

