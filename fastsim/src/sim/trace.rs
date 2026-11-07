//! The two outputs, built exactly as `baselines/abides_fork/trace.py` derives them from ABIDES logs.
//!
//! `trace.parquet`: every event is logged at the kernel's current time, which never decreases, so
//! each timestamp group is finalized before accepting the next one (including delayed quotes).
//! Only equal-`t_ns` groups need reordering. trace.py's final
//! `sort_values(["t_ns", "order_id"], kind="stable")` puts quotes (`order_id = -1`) first and keeps
//! generation order among rows of one order (they are all logged by the owning agent). Quotes are
//! deduplicated per `(t_ns, side)`: the LAST value survives at the side's FIRST-appearance
//! position. An order's last `ORDER_EXECUTED` is `ORDER_FILLED`, earlier ones `PARTIAL_FILL`.
//!
//! `message_trace.parquet`: one row per delivered (sub)message in kernel processing order.
//!
//! Both builders can stream: once a chunk of final rows (their `chunk_rows`: `CHUNK_ROWS`, or
//! `STREAM_CHUNK_ROWS` for a single scenario, whose chunks shrink near the close, see [`Taper`])
//! has accumulated it is handed to a [`ChunkSink`] (the Parquet encoders) and the buffer is
//! recycled, so memory stays bounded and encoding overlaps the simulation. Execution labels depend
//! on the future (a later fill of the same order): a streamed chunk carries the labels known when
//! it was handed over, and the few that change afterwards are listed in [`TraceBuilder::late`].

use super::{Order, Side};

/// Rows per streamed chunk (= Parquet row group) by default (batch subs, tools).
pub const CHUNK_ROWS: usize = 1 << 14;

/// Rows per chunk for a single scenario: smaller groups leave less to encode after the run, whose
/// encoding the finish would otherwise wait for.
pub const STREAM_CHUNK_ROWS: usize = 1 << 12;

/// Logged calls per [`RawTrace`] chunk handed to its sink.
pub const RAW_CHUNK_ROWS: usize = 1 << 12;

/// The smallest chunk a [`Taper`] asks for.
pub const TAPER_MIN_ROWS: usize = 1 << 11;

/// The share of the rows still expected that a tapered chunk holds.
const TAPER_SHARE: f64 = 0.125;

/// Chunks that shrink as a run nears the close, so that little is left for the helper threads to
/// replay and encode when the simulation stops: a chunk holds an eighth of the rows still expected
/// (the rows so far, extrapolated over the simulated time left until the close), between
/// [`TAPER_MIN_ROWS`] and the stream's full chunk. The sizes depend on the logged rows alone, so
/// every run of a scenario cuts the same row groups.
#[derive(Clone, Copy, Debug)]
pub struct Taper {
    open: i64,
    close: i64,
}

impl Taper {
    pub fn new(open: i64, close: i64) -> Self {
        Taper { open, close }
    }

    /// Rows in the first chunk of a stream of `full`-row chunks expected to hold `expected` rows
    /// in all (the scenario's estimate): a small run is cut into chunks as small as the tail's
    /// from the start, so no more memory is prepared for it than its chunks use.
    pub fn first(&self, expected: f64, full: usize) -> usize {
        let (min, share) = lab_taper();
        ((expected * share) as usize).clamp(min.min(full), full)
    }

    /// Rows in the next chunk of a stream of `full`-row chunks, after `rows` rows up to time `t`.
    pub fn next(&self, rows: usize, t: i64, full: usize) -> usize {
        if t <= self.open || self.close <= self.open {
            return full;
        }
        let (min, share) = lab_taper();
        let expected = rows as f64 * ((self.close - t).max(0) as f64 / (t - self.open) as f64);
        ((expected * share) as usize).clamp(min.min(full), full)
    }
}

/// The smallest chunk and the share of the expected rows a chunk holds. Measurement builds only:
/// `FASTSIM_LAB_TAPER=min,share` (e.g. `512,0.125`) replaces them, to compare settings.
#[cfg(feature = "marks")]
fn lab_taper() -> (usize, f64) {
    static SETTING: std::sync::OnceLock<(usize, f64)> = std::sync::OnceLock::new();
    *SETTING.get_or_init(|| match std::env::var("FASTSIM_LAB_TAPER") {
        Ok(v) => {
            let (min, share) = v.split_once(',').expect("FASTSIM_LAB_TAPER=min,share");
            (min.trim().parse().expect("taper min"), share.trim().parse().expect("taper share"))
        }
        Err(_) => (TAPER_MIN_ROWS, TAPER_SHARE),
    })
}

#[cfg(not(feature = "marks"))]
#[inline(always)]
fn lab_taper() -> (usize, f64) {
    (TAPER_MIN_ROWS, TAPER_SHARE)
}

/// Receives full chunks of final rows, in order, and returns an empty buffer to refill.
pub trait ChunkSink<T>: Send {
    fn take(&mut self, rows: Vec<T>) -> Vec<T>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TraceType {
    Submitted = 0,
    Accepted = 1,
    Filled = 2,
    PartialFill = 3,
    Cancelled = 4,
    Quote = 5,
    /// An ORDER_EXECUTED as logged, before the builder labels it (see `TraceBuilder::last_exec`).
    Executed = 6,
    /// A [`RawTrace`] row only: both quote calls of one accepted order, the best bid then the best
    /// ask, which the replay makes as two calls (see [`RawTrace::quotes`]).
    Quotes = 7,
}

impl TraceType {
    /// The trace's `msg_type` vocabulary, indexed by the discriminant.
    pub const NAMES: [&'static str; 6] = [
        "ORDER_SUBMITTED",
        "ORDER_ACCEPTED",
        "ORDER_FILLED",
        "PARTIAL_FILL",
        "ORDER_CANCELLED",
        "QUOTE_UPDATE",
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TraceType::Executed => unreachable!("executions are labelled by the builder"),
            TraceType::Quotes => unreachable!("raw quote pairs are replayed as two quotes"),
            k => Self::NAMES[k as usize],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TraceRow {
    pub t_ns: i64,
    pub order_id: i64,
    pub price: i64,
    pub size: i64,
    pub agent_id: i32,
    pub kind: TraceType,
    pub side: Side,
}

/// Quotes seen at the current timestamp: side order of first appearance, last values.
#[derive(Default)]
struct QuoteBuf {
    t_ns: i64,
    n: usize,
    sides: [Option<Side>; 2],
    vals: [(i64, i64); 2],
}

pub struct TraceBuilder {
    /// Rows not yet handed to the sink (all rows when there is no sink).
    pub rows: Vec<TraceRow>,
    quotes: QuoteBuf,
    exchange_id: i32,
    group_time: Option<i64>,
    group_start: usize,
    /// Global index of `rows[0]`.
    base: usize,
    sink: Option<Box<dyn ChunkSink<TraceRow>>>,
    /// The global row of each order's latest execution so far, by order id (`u32::MAX`: none).
    /// trace.py labels an order's last execution in output order `ORDER_FILLED` and every earlier
    /// one `PARTIAL_FILL`, so each execution is labelled filled as it comes and relabels its
    /// order's previous one: the labels are final when the run ends.
    last_exec: Vec<u32>,
    /// Streaming only: rows of chunks already handed to the sink (filled when handed over) that
    /// a later execution of their order relabelled `PARTIAL_FILL`, as (chunk, row in chunk).
    pub late: Vec<(u32, u32)>,
    /// Sizes of the chunks handed to the sink, in order.
    pub sent: Vec<usize>,
    /// Rows of the next chunk handed to the sink: `full_rows`, or fewer near the close (`taper`).
    chunk_rows: usize,
    full_rows: usize,
    taper: Option<Taper>,
}

impl TraceBuilder {
    pub fn new(exchange_id: i32) -> Self {
        Self::with_rows(exchange_id, Vec::with_capacity(1 << 16), None, CHUNK_ROWS)
    }

    /// Stream chunks of `chunk_rows` final rows to `sink` instead of keeping every row.
    pub fn with_sink(exchange_id: i32, sink: Box<dyn ChunkSink<TraceRow>>, chunk_rows: usize) -> Self {
        Self::with_rows(exchange_id, Vec::with_capacity(chunk_rows + 1024), Some(sink), chunk_rows)
    }

    fn with_rows(exchange_id: i32, rows: Vec<TraceRow>, sink: Option<Box<dyn ChunkSink<TraceRow>>>, chunk_rows: usize) -> Self {
        TraceBuilder {
            rows,
            quotes: QuoteBuf::default(),
            exchange_id,
            group_time: None,
            group_start: 0,
            base: 0,
            sink,
            last_exec: Vec::new(),
            late: Vec::new(),
            sent: Vec::new(),
            chunk_rows,
            full_rows: chunk_rows,
            taper: None,
        }
    }

    /// Hand over a first chunk of `first` rows, and smaller chunks near the close (see [`Taper`]).
    pub fn with_taper(mut self, taper: Option<Taper>, first: usize) -> Self {
        self.taper = taper;
        self.chunk_rows = first.min(self.full_rows);
        self
    }

    /// Total rows so far (streamed or held).
    pub fn len(&self) -> usize {
        self.base + self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row-group sizes: the streamed chunks, then the held tail (if any).
    pub fn chunk_sizes(&self) -> Vec<usize> {
        let mut sizes = self.sent.clone();
        if !self.rows.is_empty() {
            sizes.push(self.rows.len());
        }
        sizes
    }

    #[inline]
    fn advance_time(&mut self, t_ns: i64) {
        if self.group_time != Some(t_ns) {
            debug_assert!(self.group_time.is_none_or(|previous| previous <= t_ns));
            self.finish_group();
            self.group_time = Some(t_ns);
        }
    }

    fn finish_group(&mut self) {
        if self.quotes.n > 0 {
            self.flush_quotes();
        }
        // Quotes sort first (-1), in first-side-appearance order. A stable sort also preserves
        // the lifecycle/execution order of tied rows for the same order id.
        let group = &mut self.rows[self.group_start..];
        if group.len() > 1 {
            group.sort_by_key(|r| r.order_id);
        }
        for i in self.group_start..self.rows.len() {
            if self.rows[i].kind == TraceType::Executed {
                let row = (self.base + i) as u32;
                let order = self.rows[i].order_id as usize;
                if order >= self.last_exec.len() {
                    self.last_exec.resize((order + 1).max(2 * self.last_exec.len()), u32::MAX);
                }
                let previous = std::mem::replace(&mut self.last_exec[order], row);
                if previous != u32::MAX {
                    match (previous as usize).checked_sub(self.base) {
                        Some(held) => self.rows[held].kind = TraceType::PartialFill,
                        None => self.relabel_sent(previous as usize),
                    }
                }
                self.rows[i].kind = TraceType::Filled;
            }
        }
        self.group_start = self.rows.len();
        if let Some(sink) = &mut self.sink {
            if self.rows.len() >= self.chunk_rows {
                let t = self.rows.last().map_or(0, |r| r.t_ns);
                let full = std::mem::take(&mut self.rows);
                self.base += full.len();
                self.sent.push(full.len());
                self.rows = sink.take(full);
                self.group_start = 0;
                if let Some(taper) = self.taper {
                    self.chunk_rows = taper.next(self.base, t, self.full_rows);
                }
            }
        }
    }

    /// Note that streamed row `row` is now `PARTIAL_FILL` (see `late`).
    #[cold]
    fn relabel_sent(&mut self, row: usize) {
        // Searched from the latest chunk: an order's next execution mostly follows soon.
        let mut chunk = self.sent.len();
        let mut start = self.base;
        while start > row {
            chunk -= 1;
            start -= self.sent[chunk];
        }
        self.late.push((chunk as u32, (row - start) as u32));
    }

    /// An order-lifecycle event logged by the order's owner.
    #[inline]
    pub fn order_event(&mut self, t_ns: i64, kind: TraceType, order: &Order) {
        self.advance_time(t_ns);
        let price = if kind == TraceType::Executed {
            order.fill_price
        } else {
            order.limit_price
        };
        self.rows.push(TraceRow {
            t_ns,
            order_id: order.order_id,
            price,
            size: order.quantity,
            agent_id: order.agent_id as i32,
            kind,
            side: order.side,
        });
    }

    /// A BEST_BID / BEST_ASK log line from the exchange.
    #[inline]
    pub fn quote(&mut self, t_ns: i64, side: Side, price: i64, size: i64) {
        self.advance_time(t_ns);
        let q = &mut self.quotes;
        q.t_ns = t_ns;
        let slot = match q.sides[..q.n].iter().position(|s| *s == Some(side)) {
            Some(i) => i,
            None => {
                q.sides[q.n] = Some(side);
                q.n += 1;
                q.n - 1
            }
        };
        q.vals[slot] = (price, size);
    }

    fn flush_quotes(&mut self) {
        let q = std::mem::take(&mut self.quotes);
        for i in 0..q.n {
            let (price, size) = q.vals[i];
            self.rows.push(TraceRow {
                t_ns: q.t_ns,
                order_id: -1,
                price,
                size,
                agent_id: self.exchange_id,
                kind: TraceType::Quote,
                side: q.sides[i].expect("filled slot"),
            });
        }
    }

    /// Finalize the last timestamp group. Executions are labelled as they come (see
    /// `last_exec`): the held rows carry their final labels, and the streamed ones did when they
    /// were handed over, except those in `late`.
    pub fn finish(&mut self) {
        self.finish_group();
    }
}

impl TraceBuilder {
    /// Apply one call logged by a [`RawTrace`], exactly as if it were made now.
    #[inline]
    pub fn replay(&mut self, row: &TraceRow) {
        if row.kind == TraceType::Quote {
            self.quote(row.t_ns, row.side, row.price, row.size);
        } else if row.kind == TraceType::Quotes {
            self.quote(row.t_ns, Side::Bid, row.order_id, row.price);
            self.quote(row.t_ns, Side::Ask, row.size, row.agent_id as i64);
        } else {
            self.advance_time(row.t_ns);
            self.rows.push(*row);
        }
    }
}

/// The trace as the event loop logs it: one row per `order_event` / `quote` call, in call order,
/// with quote calls as `QUOTE_UPDATE` rows. Replaying the rows into a [`TraceBuilder`] yields the
/// final trace, as trace.py derives it from the finished ABIDES logs after the run; the loop
/// itself only appends. With a sink, every `RAW_CHUNK_ROWS` rows are handed over for replay.
pub struct RawTrace {
    pub rows: Vec<TraceRow>,
    sink: Option<Box<dyn ChunkSink<TraceRow>>>,
    /// Rows already handed to the sink.
    sent: usize,
    /// Rows at which the current chunk is handed over (`usize::MAX` without a sink).
    limit: usize,
    taper: Option<Taper>,
}

impl RawTrace {
    pub fn new() -> Self {
        RawTrace {
            rows: Vec::with_capacity(1 << 16),
            sink: None,
            sent: 0,
            limit: usize::MAX,
            taper: None,
        }
    }

    /// Collect every row, with `rows` of them populated in advance (see [`crate::mem`]).
    pub fn prefaulted(rows: usize) -> Self {
        RawTrace {
            rows: crate::mem::prefaulted(rows),
            sink: None,
            sent: 0,
            limit: usize::MAX,
            taper: None,
        }
    }

    /// Hand every full chunk to `sink`, the first one at `first` rows, of which `rows` are
    /// populated now. With a `taper`, chunks shrink near the close (see [`Taper`]).
    pub fn with_sink(sink: Box<dyn ChunkSink<TraceRow>>, rows: usize, first: usize, taper: Option<Taper>) -> Self {
        let first = first.min(RAW_CHUNK_ROWS);
        let mut buffer = Vec::with_capacity(RAW_CHUNK_ROWS);
        crate::mem::prefault(&mut buffer, rows.min(first));
        RawTrace {
            rows: buffer,
            sink: Some(sink),
            sent: 0,
            limit: first,
            taper,
        }
    }

    #[inline]
    fn push(&mut self, row: TraceRow) {
        self.rows.push(row);
        if self.rows.len() >= self.limit {
            self.hand_over();
        }
    }

    /// Hand the current chunk to the sink.
    #[inline(never)]
    fn hand_over(&mut self) {
        let Some(sink) = &mut self.sink else { return };
        let t = self.rows.last().map_or(0, |r| r.t_ns);
        let full = std::mem::take(&mut self.rows);
        self.sent += full.len();
        self.rows = sink.take(full);
        if let Some(taper) = self.taper {
            self.limit = taper.next(self.sent, t, RAW_CHUNK_ROWS);
        }
    }

    /// Calls logged so far.
    pub fn len(&self) -> usize {
        self.sent + self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// An order-lifecycle event logged by the order's owner (see [`TraceBuilder::order_event`]).
    #[inline]
    pub fn order_event(&mut self, t_ns: i64, kind: TraceType, order: &Order) {
        let price = if kind == TraceType::Executed {
            order.fill_price
        } else {
            order.limit_price
        };
        self.push(TraceRow {
            t_ns,
            order_id: order.order_id,
            price,
            size: order.quantity,
            agent_id: order.agent_id as i32,
            kind,
            side: order.side,
        });
    }

    /// A BEST_BID / BEST_ASK log line from the exchange (see [`TraceBuilder::quote`]).
    #[inline]
    pub fn quote(&mut self, t_ns: i64, side: Side, price: i64, size: i64) {
        self.push(TraceRow {
            t_ns,
            order_id: -1,
            price,
            size,
            agent_id: 0,
            kind: TraceType::Quote,
            side,
        });
    }

    /// The exchange's two quote lines for an accepted order, the best bid's `(price, size)` then
    /// the best ask's, in one row: the bid in `order_id` and `price`, the ask in `size` and
    /// `agent_id` (two rows when the ask's size needs more than 32 bits).
    #[inline]
    pub fn quotes(&mut self, t_ns: i64, bid: (i64, i64), ask: (i64, i64)) {
        match i32::try_from(ask.1) {
            Ok(ask_size) => self.push(TraceRow {
                t_ns,
                order_id: bid.0,
                price: bid.1,
                size: ask.0,
                agent_id: ask_size,
                kind: TraceType::Quotes,
                side: Side::Bid,
            }),
            Err(_) => {
                self.quote(t_ns, Side::Bid, bid.0, bid.1);
                self.quote(t_ns, Side::Ask, ask.0, ask.1);
            }
        }
    }

    /// Hand the remaining rows to the sink and close it, or return every row when there is none.
    pub fn into_rows(mut self) -> Vec<TraceRow> {
        match self.sink.take() {
            Some(mut sink) => {
                if !self.rows.is_empty() {
                    sink.take(std::mem::take(&mut self.rows));
                }
                Vec::new()
            }
            None => std::mem::take(&mut self.rows),
        }
    }
}

impl RawTrace {
    /// A log with no buffer yet (a placeholder; allocates nothing).
    pub fn empty() -> Self {
        RawTrace {
            rows: Vec::new(),
            sink: None,
            sent: 0,
            limit: usize::MAX,
            taper: None,
        }
    }
}

/// Ledger message types, named as `Message.type()` (the class name) or `AGENT_WAKEUP`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MsgKind {
    Wakeup = 0,
    MarketHoursRequest,
    MarketClosePriceRequest,
    MarketHours,
    MarketClosePrice,
    MarketClosed,
    QuerySpread,
    QuerySpreadResponse,
    LimitOrder,
    CancelOrder,
    OrderAccepted,
    OrderExecuted,
    OrderCancelled,
}

impl MsgKind {
    /// The ledger's `msg_type` vocabulary, indexed by the discriminant.
    pub const NAMES: [&'static str; 13] = [
        "AGENT_WAKEUP",
        "MarketHoursRequestMsg",
        "MarketClosePriceRequestMsg",
        "MarketHoursMsg",
        "MarketClosePriceMsg",
        "MarketClosedMsg",
        "QuerySpreadMsg",
        "QuerySpreadResponseMsg",
        "LimitOrderMsg",
        "CancelOrderMsg",
        "OrderAcceptedMsg",
        "OrderExecutedMsg",
        "OrderCancelledMsg",
    ];

    pub fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

/// Null marker for the ledger's nullable integer columns.
pub const NULL: i64 = i64::MIN;

/// Null marker for a [`LedgerRow`]'s 32-bit id fields.
pub const NONE: u32 = u32::MAX;

/// One `message_trace.parquet` row as the event loop records it, in 40 bytes; `seq` is its global
/// index. `latency_ns` is derived (`t_recv - t_send`, 0 for a wakeup), and the ids are 32-bit:
/// a run within the output limit has far fewer than 2^32 messages and orders.
#[derive(Clone, Copy, Debug)]
pub struct LedgerRow {
    pub t_recv: i64,
    /// `NULL` for a wakeup.
    pub t_send: i64,
    pub message_id: u32,
    /// [`NONE`] for null.
    pub order_id: u32,
    /// [`NONE`] for null.
    pub causal_parent: u32,
    pub src: u32,
    pub dst: u32,
    pub kind: MsgKind,
}

impl LedgerRow {
    /// The `latency_ns` column.
    #[inline(always)]
    pub fn latency(&self) -> i64 {
        if self.t_send == NULL { 0 } else { self.t_recv.wrapping_sub(self.t_send) }
    }

    /// A 32-bit id field as its nullable column value.
    #[inline(always)]
    pub fn id(v: u32) -> Option<i64> {
        (v != NONE).then_some(v as i64)
    }
}

/// The message ledger, optionally streamed; `on == false` records nothing at all.
pub struct LedgerBuilder {
    pub on: bool,
    pub rows: Vec<LedgerRow>,
    /// Global `seq` of `rows[0]`.
    pub base: usize,
    /// Chunks handed to the sink.
    pub chunks: usize,
    sink: Option<Box<dyn ChunkSink<LedgerRow>>>,
    /// Rows of the next chunk handed to the sink: `full_rows`, or fewer near the close (`taper`).
    chunk_rows: usize,
    full_rows: usize,
    taper: Option<Taper>,
}

impl LedgerBuilder {
    pub fn new(on: bool) -> Self {
        LedgerBuilder {
            on,
            rows: Vec::with_capacity(if on { 1 << 16 } else { 0 }),
            base: 0,
            chunks: 0,
            sink: None,
            chunk_rows: usize::MAX,
            full_rows: usize::MAX,
            taper: None,
        }
    }

    /// Stream chunks of `chunk_rows` rows to `sink`, the first one at `first` rows, and smaller
    /// ones near the close with a `taper`.
    pub fn with_sink(sink: Box<dyn ChunkSink<LedgerRow>>, chunk_rows: usize, first: usize, taper: Option<Taper>) -> Self {
        LedgerBuilder {
            on: true,
            rows: Vec::with_capacity(chunk_rows),
            base: 0,
            chunks: 0,
            sink: Some(sink),
            chunk_rows: first.min(chunk_rows),
            full_rows: chunk_rows,
            taper,
        }
    }

    pub fn len(&self) -> usize {
        self.base + self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Populate the pages for the next `rows` rows of the current buffer (see [`crate::mem`]).
    pub fn prefault(&mut self, rows: usize) {
        crate::mem::prefault(&mut self.rows, rows);
    }

    #[inline]
    pub fn push(&mut self, row: LedgerRow) {
        self.rows.push(row);
        if self.rows.len() >= self.chunk_rows {
            self.hand_over();
        }
    }

    /// Hand the current chunk to the sink.
    #[inline(never)]
    fn hand_over(&mut self) {
        let Some(sink) = &mut self.sink else { return };
        let t = self.rows.last().map_or(0, |r| r.t_recv);
        let full = std::mem::take(&mut self.rows);
        self.base += full.len();
        self.chunks += 1;
        self.rows = sink.take(full);
        if let Some(taper) = self.taper {
            self.chunk_rows = taper.next(self.base, t, self.full_rows);
        }
    }
}
