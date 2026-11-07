//! `trace.parquet`, `message_trace.parquet` and `events.json`, in the schema the scorer reads.
//!
//! The parquet key-value metadata carries the same `pandas` block pandas writes, so
//! `pd.read_parquet` restores the dtypes the gates rely on — in particular nullable `Int64` for
//! `t_send_ns`: read back as float64, a ~1.6e18 ns timestamp loses its low bits and the ledger's
//! latency identity check fails.
//!
//! Encoding runs while the simulation does: the builders hand over each finished chunk of rows
//! (one Parquet row group), background workers encode its column chunks, and the row buffers
//! come back for reuse. Workers start only when the first chunk arrives, so small scenarios never
//! spawn a thread. A trace row group's `msg_type` column is encoded with the execution labels known
//! when the group is handed over; the few that a later execution changes are patched in place
//! when the run ends (see [`pq::ColumnChunk::set_dict_index`]).
//! Given its files, a run writes them while it continues (see [`simulate_with`]): each ledger
//! row group, and each trace row group without its `msg_type` column, goes to its file in order
//! as soon as it is encoded. The trace's `msg_type` chunks follow all the others in the file, so
//! only they and the footers are left when the run ends. The binary streams every scenario on its
//! own (`FASTSIM_STREAM=0` writes after the run instead); batch subs write after their run.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::pq::{self, ColumnChunk, ColumnType, RowGroup, Scratch};
use crate::scenario::{LatencyModel, Scenario};
use crate::sim::Side;
use crate::sim::engine::{EarlyLatency, Finished, Sim, market_hours};
use crate::sim::trace::{
    CHUNK_ROWS, ChunkSink, LedgerBuilder, LedgerRow, MsgKind, NULL, RAW_CHUNK_ROWS, RawTrace,
    STREAM_CHUNK_ROWS, Taper, TraceBuilder, TraceRow, TraceType,
};

const TRACE_COLUMNS: &[(&str, ColumnType)] = &[
    ("t_ns", ColumnType::Int64),
    ("agent_id", ColumnType::Int32),
    ("msg_type", ColumnType::Utf8),
    ("side", ColumnType::Utf8),
    ("price", ColumnType::Int64),
    ("size", ColumnType::Int64),
    ("order_id", ColumnType::Int64),
];

/// The trace's `msg_type` column: encoded last, and laid out after every other column chunk
/// (see [`pq::assemble_pieces_deferred`]).
const MSG_TYPE_COLUMN: usize = 2;

const LEDGER_COLUMNS: &[(&str, ColumnType)] = &[
    ("seq", ColumnType::Int64),
    ("t_recv_ns", ColumnType::Int64),
    ("t_send_ns", ColumnType::Int64),
    ("latency_ns", ColumnType::Int64),
    ("src_id", ColumnType::Int32),
    ("dst_id", ColumnType::Int32),
    ("message_id", ColumnType::Int64),
    ("msg_type", ColumnType::Utf8),
    ("order_id", ColumnType::Int64),
    ("causal_parent", ColumnType::Int64),
];

const SIDES: [&str; 2] = ["BID", "ASK"];

fn pandas_meta(cols: &[(&str, &str, &str)]) -> String {
    let columns: Vec<String> = cols
        .iter()
        .map(|(name, pandas_type, numpy_type)| {
            format!(
                "{{\"name\": \"{name}\", \"field_name\": \"{name}\", \"pandas_type\": \"{pandas_type}\", \
                 \"numpy_type\": \"{numpy_type}\", \"metadata\": null}}"
            )
        })
        .collect();
    format!(
        "{{\"index_columns\": [], \"column_indexes\": [], \"columns\": [{}], \
         \"creator\": {{\"library\": \"fastsim\", \"version\": \"{}\"}}, \"pandas_version\": \"1.5.3\"}}",
        columns.join(", "),
        env!("CARGO_PKG_VERSION")
    )
}

fn trace_schema() -> pq::Schema {
    pq::Schema {
        columns: TRACE_COLUMNS,
        metadata: vec![(
            "pandas".to_string(),
            pandas_meta(&[
                ("t_ns", "int64", "int64"),
                ("agent_id", "int32", "int32"),
                ("msg_type", "unicode", "string"),
                ("side", "unicode", "string"),
                ("price", "int64", "int64"),
                ("size", "int64", "int64"),
                ("order_id", "int64", "int64"),
            ]),
        )],
    }
}

fn ledger_schema() -> pq::Schema {
    pq::Schema {
        columns: LEDGER_COLUMNS,
        metadata: vec![(
            "pandas".to_string(),
            pandas_meta(&[
                ("seq", "int64", "int64"),
                ("t_recv_ns", "int64", "int64"),
                ("t_send_ns", "int64", "Int64"),
                ("latency_ns", "int64", "int64"),
                ("src_id", "int32", "int32"),
                ("dst_id", "int32", "int32"),
                ("message_id", "int64", "int64"),
                ("msg_type", "unicode", "string"),
                ("order_id", "int64", "Int64"),
                ("causal_parent", "int64", "Int64"),
            ]),
        )],
    }
}

// ---------------------------------------------------------------------------------- row groups

#[inline(always)]
fn nullable(v: i64) -> Option<i64> {
    (v != NULL).then_some(v)
}

/// Every trace column of one row group except `msg_type` (index 2), in schema order.
fn encode_trace_group(s: &mut Scratch, rows: &[TraceRow]) -> Vec<ColumnChunk> {
    vec![
        s.i64_column(rows.iter().map(|r| r.t_ns)),
        s.i32_column(rows.iter().map(|r| r.agent_id)),
        s.dict_column(&SIDES, rows.iter().map(|r| (r.side == Side::Ask) as u8)),
        s.i64_column(rows.iter().map(|r| r.price)),
        s.i64_column(rows.iter().map(|r| r.size)),
        s.i64_column(rows.iter().map(|r| r.order_id)),
    ]
}

/// The trace's `msg_type` column of one row group.
fn encode_trace_kinds(s: &mut Scratch, rows: &[TraceRow]) -> ColumnChunk {
    s.dict_column(
        &TraceType::NAMES,
        rows.iter().map(|r| {
            debug_assert!(r.kind != TraceType::Executed, "unlabelled execution");
            r.kind as u8
        }),
    )
}

fn encode_ledger_group(s: &mut Scratch, first_seq: usize, rows: &[LedgerRow]) -> Vec<ColumnChunk> {
    let first = first_seq as i64;
    vec![
        s.i64_column(first..first + rows.len() as i64),
        s.i64_column(rows.iter().map(|r| r.t_recv)),
        s.nullable_i64_column(rows.iter().map(|r| nullable(r.t_send))),
        s.i64_column(rows.iter().map(LedgerRow::latency)),
        s.i32_column(rows.iter().map(|r| r.src as i32)),
        s.i32_column(rows.iter().map(|r| r.dst as i32)),
        s.i64_column(rows.iter().map(|r| r.message_id as i64)),
        s.dict_column(&MsgKind::NAMES, rows.iter().map(|r| r.kind as u8)),
        s.nullable_i64_column(rows.iter().map(|r| LedgerRow::id(r.order_id))),
        s.nullable_i64_column(rows.iter().map(|r| LedgerRow::id(r.causal_parent))),
    ]
}

// ---------------------------------------------------------------------------------- pipeline

enum Job {
    Trace {
        index: usize,
        rows: Vec<TraceRow>,
    },
    Ledger {
        index: usize,
        first_seq: usize,
        rows: Vec<LedgerRow>,
    },
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    pending: usize,
    closed: bool,
    trace: Vec<Option<Vec<ColumnChunk>>>,
    ledger: Vec<Option<Vec<ColumnChunk>>>,
    /// The trace's `msg_type` chunks, kept apart: they are written after all the others.
    kinds: Vec<Option<ColumnChunk>>,
    trace_free: Vec<Vec<TraceRow>>,
    ledger_free: Vec<Vec<LedgerRow>>,
    /// Ledger buffers for a worker to add to `ledger_free`: how many, the rows of each to
    /// populate, and their capacity (see [`Pipeline::stock_ledger_buffers`]).
    ledger_stock: (usize, usize, usize),
    /// Workers waiting for a job.
    idle: usize,
    /// The drainer is waiting for completions.
    draining: bool,
    /// The trace replay thread has replayed and handed over its last chunk.
    replay_done: bool,
    /// How far the trace and the ledger are written while the run continues.
    trace_progress: Progress,
    ledger_progress: Progress,
}

impl State {
    fn progress(&mut self, target: Target) -> &mut Progress {
        match target {
            Target::Trace => &mut self.trace_progress,
            Target::Ledger => &mut self.ledger_progress,
        }
    }
}

/// A file written while the run continues: its row groups go out in order as they are encoded
/// (see [`Shared::stream`]).
#[derive(Default)]
struct Progress {
    /// Row groups are written as they complete (until a write fails).
    on: bool,
    /// Row groups before this index are written.
    next: usize,
    /// A thread is writing this file's row groups; the others leave theirs to it.
    writing: bool,
    /// The file's leading magic is written.
    magic_written: bool,
    /// The first failed write, reported when the pipeline drains.
    error: Option<std::io::Error>,
}

/// The files a run can write while it continues.
#[derive(Clone, Copy)]
enum Target {
    Trace,
    Ledger,
}

/// A streamed file, with the SHA-256 of what it holds when its digest is part of the record
/// (the trace's).
struct Streamed {
    file: std::fs::File,
    digest: Option<Sha256>,
}

impl Streamed {
    /// Write `pieces` after what the file holds, adding them to the digest.
    fn write<'a>(&mut self, pieces: impl Iterator<Item = &'a [u8]>) -> std::io::Result<()> {
        let Streamed { file, digest } = self;
        write_all_pieces(
            file,
            pieces.inspect(|piece| {
                if let Some(d) = digest {
                    d.update(piece);
                }
            }),
        )
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signals new jobs (to workers) and completed jobs (to the finisher).
    cv: Condvar,
    /// `state.jobs.len()`, kept current under the lock, for a polling worker to watch without it.
    queued: AtomicUsize,
    /// The pipeline is closing (set with `State::closed`), for polling workers.
    closing: AtomicBool,
    /// Workers poll for jobs instead of sleeping between them (see [`POLL`]).
    poll: bool,
    /// The files written while the run continues (see [`Shared::stream`]).
    trace_out: Mutex<Option<Streamed>>,
    ledger_out: Mutex<Option<Streamed>>,
    /// Threads of the batch that lend a hand with queued jobs (see [`Help`]).
    help: Option<Arc<Help>>,
}

/// Threads of a batch that have no sub left to start take encoding jobs from the subs still
/// running, so the last subs' tails are split between CPUs that would otherwise wait. A sub's
/// pipeline queues a job for them only while one is idle; the sub runs whatever they have not
/// taken itself when it drains.
#[derive(Default)]
pub struct Help {
    /// The pipelines of the running subs.
    pipelines: Mutex<Vec<Arc<Shared>>>,
    /// Threads that help (they help until the batch ends).
    idle: AtomicUsize,
    /// Jobs queued for them and not taken yet.
    queued: AtomicUsize,
    /// Threads asleep in [`Help::wait`], guarded by the lock they sleep on.
    sleepers: Mutex<usize>,
    bell: Condvar,
}

impl Help {
    /// The calling thread has no sub left to start and takes jobs from now on.
    pub fn enlist(&self) {
        self.idle.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether a queued job waits for a helper.
    pub fn has_jobs(&self) -> bool {
        self.queued.load(Ordering::Acquire) > 0
    }

    /// Wake the threads waiting for work; call after making work available.
    pub fn ring(&self) {
        let sleepers = self.sleepers.lock().expect("help sleepers");
        if *sleepers > 0 {
            drop(sleepers);
            self.bell.notify_all();
        }
    }

    /// Wait until `ready` holds: a short spin, then asleep until [`Help::ring`]. Helpers do
    /// not spin for long, since that takes cycles from the busy threads when CPUs are short
    /// (or share a core).
    pub fn wait(&self, ready: impl Fn() -> bool) {
        for _ in 0..1_000 {
            if ready() {
                return;
            }
            std::hint::spin_loop();
        }
        let mut sleepers = self.sleepers.lock().expect("help sleepers");
        *sleepers += 1;
        while !ready() {
            sleepers = self.bell.wait(sleepers).expect("help sleepers");
        }
        *sleepers -= 1;
    }

    /// Run one queued job of a running sub; `false` if there is none.
    pub fn run_one(&self, scratch: &mut Scratch) -> bool {
        if self.queued.load(Ordering::Acquire) == 0 {
            return false;
        }
        let taken = {
            let pipelines = self.pipelines.lock().expect("help pipelines");
            pipelines.iter().find_map(|shared| {
                let job = shared.state.lock().expect("state").jobs.pop_front()?;
                Some((Arc::clone(shared), job))
            })
        };
        match taken {
            Some((shared, job)) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                shared.run(job, scratch);
                true
            }
            None => false,
        }
    }
}

/// In a large single scenario the event loop hands a chunk to the replay thread and a chunk to
/// the encoder every 0.1-0.3 ms, and each hand-over to a sleeping thread costs the loop a system
/// call to wake it (about 600 per gb-mega run). A helper of such a run instead sleeps this long
/// between looks at its queue, so the loop never finds it asleep in the queue's sense and hands
/// over for free; the helper's own sleeps cost the loop nothing. Spinning instead would compete
/// with the loop for a core's resources when the two share one.
const POLL: Duration = Duration::from_micros(20);

/// The replay thread's poll: it replays and encodes nearly as fast as the loop produces in the
/// largest runs, so it looks more often.
const REPLAY_POLL: Duration = Duration::from_micros(10);

/// Looks a worker takes before sleeping for real (a run with no jobs for a while).
const IDLE_POLLS: u32 = 256;

/// Expected trace rows from which a single scenario's helpers poll (see [`POLL`]): below it,
/// the hand-overs are too few for polling to pay for its tail.
const POLL_MIN_ROWS: f64 = 100_000.0;

/// What streaming a file left behind: the file (positioned after the written row groups) and its
/// digest so far, whether its magic is written, and the first write error.
struct StreamEnd {
    out: Option<Streamed>,
    magic_written: bool,
    error: Option<std::io::Error>,
}

impl Shared {
    fn run(&self, job: Job, scratch: &mut Scratch) {
        let mut st = match job {
            Job::Trace { index, mut rows } => {
                let chunks = encode_trace_group(scratch, &rows);
                let kinds = encode_trace_kinds(scratch, &rows);
                rows.clear();
                let mut st = self.state.lock().expect("state");
                st.trace[index] = Some(chunks);
                st.kinds[index] = Some(kinds);
                st.trace_free.push(rows);
                if st.trace_progress.on {
                    drop(st);
                    self.stream(Target::Trace);
                    st = self.state.lock().expect("state");
                }
                st
            }
            Job::Ledger {
                index,
                first_seq,
                mut rows,
            } => {
                let chunks = encode_ledger_group(scratch, first_seq, &rows);
                rows.clear();
                let mut st = self.state.lock().expect("state");
                st.ledger[index] = Some(chunks);
                st.ledger_free.push(rows);
                if st.ledger_progress.on {
                    // Still pending while it writes, so the pipeline drains only after every
                    // write has finished (the same for the trace above).
                    drop(st);
                    self.stream(Target::Ledger);
                    st = self.state.lock().expect("state");
                }
                st
            }
        };
        st.pending -= 1;
        // Only a waiting drainer needs to hear of a completion. A notify is a system call even
        // when nobody waits, several microseconds each inside gVisor.
        let wake = st.draining;
        drop(st);
        if wake {
            self.cv.notify_all();
        }
    }

    /// Write the encoded row groups of `target` that follow the written ones, in order, and free
    /// their bytes. One thread writes a file at a time, outside the state lock; a group completed
    /// meanwhile is picked up by the writer before it stops, or written by whoever completed it.
    /// Trace groups hold every column but `msg_type`, which the file keeps for last.
    fn stream(&self, target: Target) {
        loop {
            let (pieces, magic) = {
                let mut guard = self.state.lock().expect("state");
                let st = &mut *guard;
                let (groups, progress) = match target {
                    Target::Trace => (&mut st.trace, &mut st.trace_progress),
                    Target::Ledger => (&mut st.ledger, &mut st.ledger_progress),
                };
                if progress.writing || !progress.on {
                    return;
                }
                let mut pieces: Vec<Vec<u8>> = Vec::new();
                while let Some(Some(group)) = groups.get_mut(progress.next) {
                    pieces.extend(group.iter_mut().map(ColumnChunk::take_bytes));
                    progress.next += 1;
                }
                if pieces.is_empty() {
                    return;
                }
                progress.writing = true;
                let magic = !progress.magic_written;
                progress.magic_written = true;
                (pieces, magic)
            };
            let result = {
                let mut out = self.out(target).lock().expect("streamed file");
                let out = out.as_mut().expect("streamed file");
                let head: &[u8] = if magic { &pq::MAGIC[..] } else { &[] };
                out.write(std::iter::once(head).chain(pieces.iter().map(Vec::as_slice)))
            };
            drop(pieces);
            let mut st = self.state.lock().expect("state");
            let progress = st.progress(target);
            progress.writing = false;
            if let Err(e) = result {
                progress.on = false;
                progress.error.get_or_insert(e);
                return;
            }
        }
    }

    fn out(&self, target: Target) -> &Mutex<Option<Streamed>> {
        match target {
            Target::Trace => &self.trace_out,
            Target::Ledger => &self.ledger_out,
        }
    }

    fn worker(&self) {
        let mut scratch = Scratch::default();
        let mut polls = 0u32;
        loop {
            let job = {
                let mut st = self.state.lock().expect("state");
                loop {
                    if st.ledger_stock.0 > 0 {
                        let (n, rows, capacity) = std::mem::take(&mut st.ledger_stock);
                        drop(st);
                        let buffers: Vec<Vec<LedgerRow>> =
                            (0..n).map(|_| crate::mem::prefaulted_beside_with_capacity(rows, capacity)).collect();
                        st = self.state.lock().expect("state");
                        st.ledger_free.extend(buffers);
                        continue;
                    }
                    if let Some(job) = st.jobs.pop_front() {
                        self.queued.store(st.jobs.len(), Ordering::Release);
                        break job;
                    }
                    if st.closed {
                        return;
                    }
                    if self.poll && polls < IDLE_POLLS {
                        // Look again in a moment (see `POLL`) rather than sleep until woken.
                        drop(st);
                        polls += 1;
                        std::thread::sleep(POLL);
                        while self.queued.load(Ordering::Acquire) == 0 && !self.closing.load(Ordering::Acquire) && polls < IDLE_POLLS {
                            polls += 1;
                            std::thread::sleep(POLL);
                        }
                        st = self.state.lock().expect("state");
                        continue;
                    }
                    st.idle += 1;
                    st = self.cv.wait(st).expect("state");
                    st.idle -= 1;
                }
            };
            polls = 0;
            self.run(job, &mut scratch);
        }
    }
}

/// Parquet encoding for one scenario's outputs, with up to `workers` background threads.
pub struct Pipeline {
    shared: Arc<Shared>,
    workers: usize,
    handles: Mutex<Vec<crate::threads::Handle<()>>>,
    /// Encodes jobs on the submitting thread when there are no workers.
    inline: Mutex<Option<Scratch>>,
    /// Rows per trace and ledger chunk (buffers are sized for it).
    chunk_rows: usize,
    /// The simulation has ended (see [`TraceSink`]).
    run_over: AtomicBool,
}

impl Pipeline {
    /// `start` creates the workers now rather than on the first background job. With `trace_out`
    /// and `ledger_out` files, row groups are written to them as they complete (see
    /// [`Shared::stream`]), the trace's with their SHA-256 kept as they go.
    pub fn new(
        workers: usize,
        start: bool,
        poll: bool,
        chunk_rows: usize,
        trace_out: Option<std::fs::File>,
        ledger_out: Option<std::fs::File>,
        help: Option<Arc<Help>>,
    ) -> Arc<Pipeline> {
        let pipeline = Arc::new(Pipeline {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    trace_progress: Progress { on: trace_out.is_some(), ..Progress::default() },
                    ledger_progress: Progress { on: ledger_out.is_some(), ..Progress::default() },
                    ..State::default()
                }),
                cv: Condvar::new(),
                queued: AtomicUsize::new(0),
                closing: AtomicBool::new(false),
                poll,
                trace_out: Mutex::new(trace_out.map(|file| Streamed { file, digest: Some(Sha256::new()) })),
                ledger_out: Mutex::new(ledger_out.map(|file| Streamed { file, digest: None })),
                help,
            }),
            workers,
            handles: Mutex::new(Vec::new()),
            inline: Mutex::new(None),
            chunk_rows,
            run_over: AtomicBool::new(false),
        });
        if start && workers > 0 {
            pipeline.ensure_workers();
        }
        if let Some(help) = &pipeline.shared.help {
            help.pipelines.lock().expect("help pipelines").push(Arc::clone(&pipeline.shared));
        }
        pipeline
    }

    fn submit(&self, job: Job) {
        self.submit_with_mode(job, true);
    }

    fn submit_with_mode(&self, job: Job, background: bool) {
        {
            let mut st = self.shared.state.lock().expect("state");
            st.pending += 1;
            match &job {
                Job::Trace { index, .. } => {
                    if st.trace.len() <= *index {
                        st.trace.resize_with(index + 1, || None);
                        st.kinds.resize_with(index + 1, || None);
                    }
                }
                Job::Ledger { index, .. } => {
                    if st.ledger.len() <= *index {
                        st.ledger.resize_with(index + 1, || None);
                    }
                }
            }
            if self.workers > 0 && background {
                st.jobs.push_back(job);
                self.shared.queued.store(st.jobs.len(), Ordering::Release);
                // Busy or polling workers take the job on their own: wake one only if it sleeps
                // (a notify is a system call even when nobody waits).
                let wake = st.idle > 0 || st.draining;
                drop(st);
                self.ensure_workers();
                if wake {
                    self.shared.cv.notify_all();
                }
                return;
            }
            // Without workers of its own, a batch sub queues the job for the batch's idle threads
            // if there are any (counted under the lock, so a job is taken only once counted).
            if let Some(help) = self.shared.help.as_ref().filter(|h| background && h.idle.load(Ordering::Relaxed) > 0) {
                st.jobs.push_back(job);
                help.queued.fetch_add(1, Ordering::Release);
                drop(st);
                help.ring();
                return;
            }
        }
        let mut scratch = self.inline.lock().expect("inline scratch");
        self.shared.run(job, scratch.get_or_insert_with(Scratch::default));
    }

    /// Whether a background job may run on another thread: a worker, or an idle thread of the
    /// batch.
    fn can_offload(&self) -> bool {
        self.workers > 0 || self.shared.help.as_ref().is_some_and(|h| h.idle.load(Ordering::Relaxed) > 0)
    }

    /// Start the workers on first use, so a small scenario never creates a thread.
    fn ensure_workers(&self) {
        let mut handles = self.handles.lock().expect("handles");
        if handles.is_empty() {
            for _ in 0..self.workers {
                let shared = Arc::clone(&self.shared);
                handles.push(crate::threads::spawn(move || shared.worker()));
            }
        }
    }

    /// Add `n` ledger chunk buffers, `rows` of each populated, for the event loop to fill before
    /// any come back. Running workers populate them (the first chunk takes the loop a while to
    /// fill); otherwise this thread does.
    fn stock_ledger_buffers(&self, n: usize, rows: usize) {
        if n > 0 && !self.handles.lock().expect("handles").is_empty() {
            self.shared.state.lock().expect("state").ledger_stock = (n, rows, self.chunk_rows);
            self.shared.cv.notify_all();
            return;
        }
        let buffers: Vec<Vec<LedgerRow>> =
            (0..n).map(|_| crate::mem::prefaulted_with_capacity(rows, self.chunk_rows)).collect();
        self.shared.state.lock().expect("state").ledger_free.extend(buffers);
    }

    fn trace_buffer(&self) -> Vec<TraceRow> {
        let buf = self.shared.state.lock().expect("state").trace_free.pop();
        buf.unwrap_or_else(|| Vec::with_capacity(self.chunk_rows + 1024))
    }

    fn ledger_buffer(&self) -> Vec<LedgerRow> {
        let buf = self.shared.state.lock().expect("state").ledger_free.pop();
        buf.unwrap_or_else(|| Vec::with_capacity(self.chunk_rows))
    }

    /// Finish every submitted job, helping the workers with queued ones, stop the workers and
    /// return the encoded row groups. Every streamed write has finished by then: a job stays
    /// pending until its writes are done.
    /// Run queued jobs on this thread, or wait for completions, while `busy` holds.
    fn help_while(&self, busy: impl Fn(&State) -> bool) {
        let mut st = self.shared.state.lock().expect("state");
        while busy(&st) {
            if let Some(job) = st.jobs.pop_front() {
                self.shared.queued.store(st.jobs.len(), Ordering::Release);
                if let Some(help) = &self.shared.help {
                    help.queued.fetch_sub(1, Ordering::Relaxed);
                }
                drop(st);
                let mut scratch = self.inline.lock().expect("inline scratch");
                self.shared.run(job, scratch.get_or_insert_with(Scratch::default));
                drop(scratch);
                st = self.shared.state.lock().expect("state");
            } else {
                st.draining = true;
                st = self.shared.cv.wait(st).expect("state");
                st.draining = false;
            }
        }
    }

    /// The replay thread has handed over its last chunk (see [`spawn_replay`]).
    fn replay_finished(&self) {
        let mut st = self.shared.state.lock().expect("state");
        st.replay_done = true;
        let wake = st.draining;
        drop(st);
        if wake {
            self.shared.cv.notify_all();
        }
    }

    fn drain(&self) -> (Vec<Vec<ColumnChunk>>, Vec<Vec<ColumnChunk>>, Vec<ColumnChunk>, StreamEnd, StreamEnd) {
        self.help_while(|st| st.pending > 0);
        let mut st = self.shared.state.lock().expect("state");
        debug_assert_eq!(st.pending, 0);
        st.closed = true;
        self.shared.closing.store(true, Ordering::Release);
        let trace: Vec<Vec<ColumnChunk>> = std::mem::take(&mut st.trace)
            .into_iter()
            .map(|g| g.expect("encoded trace group"))
            .collect();
        let ledger: Vec<Vec<ColumnChunk>> = std::mem::take(&mut st.ledger)
            .into_iter()
            .map(|g| g.expect("encoded ledger group"))
            .collect();
        let kinds = std::mem::take(&mut st.kinds)
            .into_iter()
            .map(|c| c.expect("encoded msg_type column"))
            .collect();
        debug_assert!(!st.trace_progress.writing && !st.ledger_progress.writing);
        debug_assert!(!st.trace_progress.on || st.trace_progress.next == trace.len());
        debug_assert!(!st.ledger_progress.on || st.ledger_progress.next == ledger.len());
        let end = |progress: &mut Progress, out: &Mutex<Option<Streamed>>| StreamEnd {
            out: out.lock().expect("streamed file").take(),
            magic_written: progress.magic_written,
            error: progress.error.take(),
        };
        let trace_end = end(&mut st.trace_progress, &self.shared.trace_out);
        let ledger_end = end(&mut st.ledger_progress, &self.shared.ledger_out);
        drop(st);
        self.shared.cv.notify_all();
        for h in self.handles.lock().expect("handles").drain(..) {
            h.join().expect("encoder thread panicked");
        }
        if let Some(help) = &self.shared.help {
            help.pipelines.lock().expect("help pipelines").retain(|p| !Arc::ptr_eq(p, &self.shared));
        }
        (trace, ledger, kinds, trace_end, ledger_end)
    }
}

struct TraceSink {
    pipeline: Arc<Pipeline>,
    next: usize,
    /// Encode on the submitting thread (the replay thread, leaving the workers to the ledger)
    /// while the simulation runs. Once it has ended, the replay thread is what the run's outputs
    /// wait for, so the chunks it still replays are encoded by the workers.
    inline: bool,
    /// The replay thread has chunks waiting (see `spawn_replay`): while it is behind and the
    /// workers have nothing queued, it leaves a chunk's encoding to them and catches up.
    behind: Arc<AtomicBool>,
}

impl ChunkSink<TraceRow> for TraceSink {
    fn take(&mut self, rows: Vec<TraceRow>) -> Vec<TraceRow> {
        let offload = self.behind.load(Ordering::Relaxed) && self.pipeline.shared.queued.load(Ordering::Acquire) == 0;
        let inline = self.inline && !self.pipeline.run_over.load(Ordering::Relaxed) && !offload;
        self.pipeline.submit_with_mode(Job::Trace {
            index: self.next,
            rows,
        }, !inline);
        self.next += 1;
        self.pipeline.trace_buffer()
    }
}

struct LedgerSink {
    pipeline: Arc<Pipeline>,
    next: usize,
    seq: usize,
}

impl ChunkSink<LedgerRow> for LedgerSink {
    fn take(&mut self, rows: Vec<LedgerRow>) -> Vec<LedgerRow> {
        let n = rows.len();
        let job = Job::Ledger {
            index: self.next,
            first_seq: self.seq,
            rows,
        };
        crate::marks::blocked(crate::marks::LEDGER_SUBMIT, || self.pipeline.submit(job));
        self.next += 1;
        self.seq += n;
        crate::marks::blocked(crate::marks::LEDGER_BUFFER, || self.pipeline.ledger_buffer())
    }
}

/// Hands the event loop's logged trace chunks to the replay thread.
struct RawSink {
    chunks: SyncSender<Vec<TraceRow>>,
    recycled: Receiver<Vec<TraceRow>>,
}

impl ChunkSink<TraceRow> for RawSink {
    fn take(&mut self, rows: Vec<TraceRow>) -> Vec<TraceRow> {
        match self.chunks.try_send(rows) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(rows)) => {
                crate::marks::blocked(crate::marks::RAW_SEND_FULL, || self.chunks.send(rows)).expect("trace replay thread stopped")
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => panic!("trace replay thread stopped"),
        }
        self.recycled
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(RAW_CHUNK_ROWS))
    }
}

/// Where the logged trace is replayed into the final trace.
enum Replay {
    /// After the run, on the caller.
    Inline(TraceBuilder),
    /// While the run continues, on its own thread; it returns the builder when the log closes.
    Thread(crate::threads::Handle<TraceBuilder>),
}

/// `stock` chunk buffers, `stock_rows` of each populated, wait in the recycling queue, so the event
/// loop's next chunks are not fresh memory while the replay thread is still busy with earlier
/// ones. The replay thread populates them as it starts: the loop takes a while to fill its first
/// chunk.
fn spawn_replay(mut builder: TraceBuilder, stock: usize, stock_rows: usize, poll: bool, behind: Arc<AtomicBool>, pipeline: &Arc<Pipeline>) -> (RawSink, crate::threads::Handle<TraceBuilder>) {
    // Deep enough that a polling replay thread (see `POLL`), waking late by a sleep or two,
    // never has the event loop wait for a slot.
    let (chunks, incoming) = sync_channel::<Vec<TraceRow>>(16);
    let (recycle, recycled) = sync_channel::<Vec<TraceRow>>(16);
    let pipeline = Arc::clone(pipeline);
    let handle = crate::threads::spawn(move || {
        for _ in 0..stock.min(4) {
            let _ = recycle.try_send(crate::mem::prefaulted_beside_with_capacity(stock_rows, RAW_CHUNK_ROWS));
        }
        loop {
            // A chunk already waiting means this thread is behind the loop: the encoding of
            // what it replays next goes to the workers (see `TraceSink::behind`).
            let mut chunk = match incoming.try_recv() {
                Ok(chunk) => {
                    behind.store(true, Ordering::Relaxed);
                    Some(chunk)
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    behind.store(false, Ordering::Relaxed);
                    if poll {
                        // Look for the next chunk every `POLL` rather than sleep until woken.
                        loop {
                            match incoming.try_recv() {
                                Ok(chunk) => break Some(chunk),
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => break None,
                                Err(std::sync::mpsc::TryRecvError::Empty) => std::thread::sleep(REPLAY_POLL),
                            }
                        }
                    } else {
                        incoming.recv().ok()
                    }
                }
            };
            let Some(mut chunk) = chunk.take() else { break };
            for row in &chunk {
                builder.replay(row);
            }
            chunk.clear();
            let _ = recycle.try_send(chunk);
        }
        pipeline.replay_finished();
        builder
    });
    (RawSink { chunks, recycled }, handle)
}

/// A scenario's encoded outputs.
pub struct Outputs {
    /// When the trace was streamed, only its unwritten pieces (the `msg_type` column chunks and
    /// the footer), to be written into [`Outputs::trace_file`] and added to its digest.
    pub trace: pq::ParquetFile,
    pub trace_rows: usize,
    /// `None` when the ledger was not recorded. When it was streamed, only its unwritten
    /// pieces: the rest of the file, to be written into [`Outputs::ledger_file`].
    pub ledger: Option<pq::ParquetFile>,
    pub ledger_rows: usize,
    /// The file the trace was streamed into, positioned after the written row groups, and the
    /// SHA-256 of what it holds so far.
    pub trace_file: Option<std::fs::File>,
    pub trace_digest: Option<Sha256>,
    /// The file the ledger was streamed into, positioned after the written row groups.
    pub ledger_file: Option<std::fs::File>,
    /// A failed write while streaming (the file is then incomplete).
    pub stream_error: Option<std::io::Error>,
    /// Elapsed time of `Sim::run`, the span the baseline times around `abides.run(config)`:
    /// construction before it and output serialization after it are excluded.
    pub sim_seconds: f64,
}

/// Expected messages from which a latency producer is prepared before the run (most messages
/// draw a latency; the first thousand are drawn inline while the producer starts).
const PREPARED_PREFETCH_MESSAGES: f64 = 8192.0;

/// Simulate `sc` and encode its outputs, overlapping encoding with the simulation on up to
/// `workers` extra threads. `ledger = false` records no message ledger at all.
pub fn simulate(sc: &Scenario, ledger: bool, workers: usize) -> Outputs {
    simulate_with(sc, ledger, workers, Mode::Single, None, None)
}

/// How a scenario is run, which decides the preparation before its clock starts and the size of
/// its row groups. Repeats of one mode produce identical bytes whatever the thread count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// A scenario on its own: its clock is the simulation's window, so the memory the loop writes
    /// is populated beforehand (see [`crate::mem`]), and smaller row groups leave less to encode
    /// after the run.
    Single,
    /// One sub of a batch, whose clock covers every step: no preparation (it would only add work
    /// inside the same clock) and larger row groups, with less per-group overhead.
    BatchSub,
}

/// [`simulate`] in the given [`Mode`]. With `trace_file` and `ledger_file`, the outputs are
/// written into them while the run continues, as their row groups are encoded; the bytes are the
/// same either way.
pub fn simulate_with(
    sc: &Scenario,
    ledger: bool,
    workers: usize,
    mode: Mode,
    trace_file: Option<std::fs::File>,
    ledger_file: Option<std::fs::File>,
) -> Outputs {
    start(sc, ledger, workers, mode, trace_file, ledger_file, None).finish()
}

/// A run whose simulation is over and whose outputs remain to be finished (see
/// [`Running::finish`]), on this thread or another one.
pub struct Running {
    pipeline: Arc<Pipeline>,
    replay: Replay,
    run: Finished,
    ledger: bool,
    sim_seconds: f64,
}

/// How a run uses the CPUs beside its own (see [`start`]).
struct Plan {
    /// Rows per trace and ledger chunk (one Parquet row group).
    chunk_rows: usize,
    /// Latency samples are drawn ahead on a thread of their own.
    prefetch: bool,
    /// That thread is created before the run; otherwise only after sustained demand.
    prepare_prefetch: bool,
    /// The trace is replayed on a thread of its own while the run continues.
    replay_thread: bool,
    /// Encoding threads, created before the run when `encoders_needed`, otherwise on the first
    /// background job.
    encoders: usize,
    encoders_needed: bool,
    /// The noise and value traders' draws are computed ahead on a thread of their own.
    agent_draws: bool,
    est_trace: f64,
    est_ledger: f64,
}

/// Estimated agent draws from which a lookahead helper pays for its thread (see
/// [`crate::sim::engine::Sim::with_agent_draws`]).
const AGENT_DRAWS_MIN: f64 = 10_000.0;

impl Plan {
    fn new(sc: &Scenario, ledger: bool, workers: usize, mode: Mode) -> Plan {
        let chunk_rows = if mode == Mode::Single { STREAM_CHUNK_ROWS } else { CHUNK_ROWS };
        let prefetch = workers >= 2 && sc.latency.as_ref().is_some_and(|lat| {
            matches!(lat.model, LatencyModel::LogNormal { .. } | LatencyModel::Pareto { .. })
        });
        // The simulation owns one CPU. Latency lookahead comes first, then trace replay while at
        // least one encoder remains; with fewer spare CPUs the trace is replayed after the run.
        // Threads the run is expected to use are created before it starts; small runs create
        // none (encoders and lookahead still start on demand if the estimate was too low).
        let (est_trace, est_ledger) = sc.estimated_rows();
        let spare = workers - usize::from(prefetch);
        let replay_thread = spare >= 2 && est_trace >= RAW_CHUNK_ROWS as f64;
        let mut plan = Plan {
            chunk_rows,
            prefetch,
            prepare_prefetch: est_ledger >= PREPARED_PREFETCH_MESSAGES,
            replay_thread,
            encoders: spare - usize::from(replay_thread),
            encoders_needed: est_trace >= chunk_rows as f64 || (ledger && est_ledger >= chunk_rows as f64),
            // A burst of work at the start of a large run, beside the other helpers.
            agent_draws: mode == Mode::Single && workers >= 2 && sc.estimated_agent_draws() >= AGENT_DRAWS_MIN,
            est_trace,
            est_ledger,
        };
        plan.lab_override();
        plan
    }

    /// Measurement builds only: `FASTSIM_LAB_PLAN=prefetch,replay,encoders` (e.g. `1,0,1`) fixes
    /// the helper threads, to compare plans. The outputs do not depend on them.
    #[cfg(feature = "marks")]
    fn lab_override(&mut self) {
        let Ok(v) = std::env::var("FASTSIM_LAB_PLAN") else { return };
        let f: Vec<usize> = v.split(',').map(|x| x.trim().parse().expect("FASTSIM_LAB_PLAN")).collect();
        self.prefetch = self.prefetch && f[0] > 0;
        self.prepare_prefetch = self.prepare_prefetch || f[0] > 0;
        self.replay_thread = f[1] > 0;
        self.encoders = f[2];
        self.encoders_needed = f[2] > 0;
    }

    #[cfg(not(feature = "marks"))]
    #[inline(always)]
    fn lab_override(&mut self) {}
}

/// The helper threads a run of `sc` creates before it starts, for [`crate::threads::prestart`].
pub fn helper_threads(sc: &Scenario, ledger: bool, workers: usize, mode: Mode) -> usize {
    let plan = Plan::new(sc, ledger, workers, mode);
    usize::from(plan.prefetch && plan.prepare_prefetch)
        + usize::from(plan.replay_thread)
        + if plan.encoders_needed { plan.encoders } else { 0 }
        + usize::from(plan.agent_draws)
}

/// Set up and simulate `sc` as [`simulate_with`] does, leaving the outputs to
/// [`Running::finish`]. With `help`, a sub without workers of its own lends its encoding jobs to
/// the batch's idle threads (see [`Help`]).
pub fn start(
    sc: &Scenario,
    ledger: bool,
    workers: usize,
    mode: Mode,
    trace_file: Option<std::fs::File>,
    ledger_file: Option<std::fs::File>,
    help: Option<Arc<Help>>,
) -> Running {
    let populate = mode == Mode::Single;
    let Plan {
        chunk_rows,
        prefetch,
        prepare_prefetch,
        replay_thread,
        encoders,
        encoders_needed,
        agent_draws,
        est_trace,
        est_ledger,
    } = Plan::new(sc, ledger, workers, mode);
    let ledger_file = if ledger { ledger_file } else { None };
    let help = help.filter(|_| encoders == 0);
    // A scenario on its own hands over smaller chunks near the close, leaving less to finish
    // after the simulation stops (see `Taper`).
    let taper = populate.then(|| {
        let (open, close) = market_hours(sc);
        Taper::new(open, close)
    });
    crate::marks::mark("start");
    // The latency lookahead starts now, while the run is set up, rather than when it begins:
    // it then had only the run's first `INLINE` draws as its head start.
    let early_latency = if prefetch && prepare_prefetch { EarlyLatency::start(sc) } else { None };
    // Helpers of a large single scenario poll for their work (see `POLL`).
    let poll = populate && est_trace >= POLL_MIN_ROWS;
    let pipeline = Pipeline::new(encoders, encoders_needed, poll, chunk_rows, trace_file, ledger_file, help);
    crate::marks::mark("pipeline");
    // The first chunk of each stream: a small run's are as small as its tail's (see
    // `Taper::first`), the rest start whole.
    let first = |est: f64, full: usize| taper.map_or(full, |t| t.first(est, full));
    let behind = Arc::new(AtomicBool::new(false));
    let builder = TraceBuilder::with_sink(
        0,
        Box::new(TraceSink {
            pipeline: Arc::clone(&pipeline),
            next: 0,
            // The replay thread has time to encode the trace itself while the workers keep up
            // with the ledger, which costs more per row.
            inline: replay_thread && ledger,
            behind: Arc::clone(&behind),
        }),
        chunk_rows,
    )
    .with_taper(taper, first(est_trace, chunk_rows));
    // Memory the event loop will write is populated now: the first chunk of each log, plus one
    // spare for the chunk that follows (see `crate::mem`). Finished chunks come back for reuse
    // before the spare is full, so more would only add work before the run.
    // Batch subs populate nothing: their clock covers every step anyway.
    let expected = |est: f64| if populate { (est * 1.25) as usize } else { 0 };
    let (trace, replay) = if replay_thread {
        let raw_first = first(est_trace, RAW_CHUNK_ROWS);
        let chunks = expected(est_trace).div_ceil(raw_first);
        let (sink, handle) = spawn_replay(builder, chunks.saturating_sub(1).min(1), raw_first, poll, behind, &pipeline);
        (RawTrace::with_sink(Box::new(sink), expected(est_trace), raw_first, taper), Replay::Thread(handle))
    } else {
        let trace = if populate { RawTrace::prefaulted(expected(est_trace)) } else { RawTrace::new() };
        (trace, Replay::Inline(builder))
    };
    crate::marks::mark("replay_setup");
    let ledger_builder = if ledger {
        let mut builder = LedgerBuilder::with_sink(
            Box::new(LedgerSink {
                pipeline: Arc::clone(&pipeline),
                next: 0,
                seq: 0,
            }),
            chunk_rows,
            first(est_ledger, chunk_rows),
            taper,
        );
        let ledger_first = first(est_ledger, chunk_rows);
        builder.prefault(expected(est_ledger).min(ledger_first));
        let chunks = expected(est_ledger).div_ceil(ledger_first);
        pipeline.stock_ledger_buffers(chunks.saturating_sub(1).min(1), ledger_first);
        builder
    } else {
        LedgerBuilder::new(false)
    };
    crate::marks::mark("ledger_setup");
    let mut sim = Sim::new(sc, trace, ledger_builder);
    if agent_draws {
        sim = sim.with_agent_draws();
    }
    crate::marks::mark("sim_new");
    if populate {
        sim.prefault();
    }
    crate::marks::mark("prefault");
    let mut sim = match early_latency {
        Some(early) => sim.with_early_latency_prefetch(early),
        None if prefetch => sim.with_latency_prefetch(prepare_prefetch),
        None => sim,
    };
    crate::marks::mark("latency_thread");
    let started = Instant::now();
    crate::marks::hw_start();
    let run = sim.run();
    crate::marks::hw_stop();
    let sim_seconds = started.elapsed().as_secs_f64();
    pipeline.run_over.store(true, Ordering::Relaxed);
    crate::marks::mark("sim_run");
    drop(sim);
    crate::marks::mark("sim_dropped");
    Running {
        pipeline,
        replay,
        run,
        ledger,
        sim_seconds,
    }
}

impl Running {
    /// Replay the trace, encode what is left of both logs, and assemble the files.
    pub fn finish(self) -> Outputs {
        let Running {
            pipeline,
            replay,
            run,
            ledger,
            sim_seconds,
        } = self;
        let Finished {
            trace: raw,
            ledger: ledger_builder,
        } = run;
        let ledger_rows = ledger_builder.len();
        // Below one small page, creating encoder threads costs more than their work. These
        // scenarios have not streamed a full chunk, so all jobs can finish on the caller.
        let background_tail = raw.len().saturating_add(ledger_rows) >= 4096;
        // The ledger's unstreamed tail becomes its last row group. It does not wait for the
        // trace's replay, so it goes first when another thread can encode it meanwhile; on this
        // thread it comes after the trace, which is still in cache after the replay.
        let mut ledger_tail = (ledger && !ledger_builder.rows.is_empty()).then(|| Job::Ledger {
            index: ledger_builder.chunks,
            first_seq: ledger_builder.base,
            rows: ledger_builder.rows,
        });
        if let Some(job) = ledger_tail.take_if(|_| background_tail && pipeline.can_offload()) {
            pipeline.submit_with_mode(job, true);
        }

        let rows = raw.into_rows();
        let mut trace = match replay {
            Replay::Thread(handle) => {
                // This thread encodes queued chunks until the replay thread is done, rather than
                // waiting idle for it (once the run is over its chunks are encoded in the
                // background, see `TraceSink`).
                pipeline.help_while(|st| !st.replay_done);
                handle.join().expect("trace replay thread panicked")
            }
            Replay::Inline(mut builder) => {
                for row in &rows {
                    builder.replay(row);
                }
                builder
            }
        };
        crate::marks::mark("replay_joined");
        drop(rows);
        trace.finish();

        // The trace's unstreamed tail becomes its last row group.
        let trace_rows = trace.len();
        let bounds = group_bounds(&trace.chunk_sizes());
        let trace_tail_index = trace.sent.len();
        let TraceBuilder { rows: tail, late, .. } = trace;
        if !tail.is_empty() {
            pipeline.submit_with_mode(Job::Trace {
                index: trace_tail_index,
                rows: tail,
            }, background_tail);
        }
        if let Some(job) = ledger_tail {
            pipeline.submit_with_mode(job, background_tail);
        }

        crate::marks::mark("tails_queued");
        let (trace_groups, ledger_groups, mut kinds, trace_end, ledger_end) = pipeline.drain();
        crate::marks::mark("drained");
        // Executions relabelled after their row group was encoded.
        for (group, row) in late {
            kinds[group as usize].set_dict_index(row as usize, TraceType::PartialFill as u8);
        }
        let groups: Vec<RowGroup> = trace_groups
            .into_iter()
            .zip(kinds.drain(..))
            .zip(&bounds)
            .map(|((mut columns, msg_type), &(a, b))| {
                columns.insert(MSG_TYPE_COLUMN, msg_type);
                RowGroup {
                    columns,
                    num_rows: b - a,
                }
            })
            .collect();
        let trace_bytes = pq::assemble_pieces_deferred(&trace_schema(), groups, MSG_TYPE_COLUMN).after_streamed(trace_end.magic_written);
        let ledger_bytes = ledger.then(|| {
            let mut seq = 0;
            let groups: Vec<RowGroup> = ledger_groups
                .into_iter()
                .map(|columns| {
                    let n = columns[0].num_values() as usize;
                    seq += n;
                    RowGroup {
                        columns,
                        num_rows: n,
                    }
                })
                .collect();
            debug_assert_eq!(seq, ledger_rows);
            pq::assemble_pieces(&ledger_schema(), groups).after_streamed(ledger_end.magic_written)
        });
        let (trace_file, trace_digest) = match trace_end.out {
            Some(Streamed { file, digest }) => (Some(file), digest),
            None => (None, None),
        };
        Outputs {
            trace: trace_bytes,
            trace_rows,
            ledger: ledger_bytes,
            ledger_rows,
            trace_file,
            trace_digest,
            ledger_file: ledger_end.out.map(|streamed| streamed.file),
            stream_error: trace_end.error.or(ledger_end.error),
            sim_seconds,
        }
    }
}

fn group_bounds(sizes: &[usize]) -> Vec<(usize, usize)> {
    let mut start = 0;
    sizes
        .iter()
        .map(|&n| {
            let b = (start, start + n);
            start += n;
            b
        })
        .collect()
}

/// Encode already-collected, labelled trace rows (tests and tools).
pub fn trace_parquet(rows: &[TraceRow]) -> Vec<u8> {
    let mut s = Scratch::default();
    let groups: Vec<RowGroup> = rows
        .chunks(CHUNK_ROWS)
        .map(|rows| {
            let mut columns = encode_trace_group(&mut s, rows);
            columns.insert(MSG_TYPE_COLUMN, encode_trace_kinds(&mut s, rows));
            RowGroup {
                columns,
                num_rows: rows.len(),
            }
        })
        .collect();
    pq::assemble_deferred(&trace_schema(), groups, MSG_TYPE_COLUMN)
}

/// Encode already-collected ledger rows (tests and tools).
pub fn ledger_parquet(rows: &[LedgerRow]) -> Vec<u8> {
    let mut s = Scratch::default();
    let groups: Vec<RowGroup> = rows
        .chunks(CHUNK_ROWS)
        .enumerate()
        .map(|(i, rows)| RowGroup {
            columns: encode_ledger_group(&mut s, i * CHUNK_ROWS, rows),
            num_rows: rows.len(),
        })
        .collect();
    pq::assemble(&ledger_schema(), groups)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// SHA-256 of `file`'s bytes, hashed piece by piece.
pub fn hash_parquet(file: &pq::ParquetFile) -> String {
    let mut hasher = Sha256::new();
    for piece in file.pieces() {
        hasher.update(piece);
    }
    hex(&hasher.finalize())
}

/// Write `file` to `path` from its pieces (vectored writes, no concatenated copy), returning the
/// SHA-256 of its bytes when `digest` is set.
pub fn write_parquet(path: &Path, file: &pq::ParquetFile, digest: bool) -> std::io::Result<Option<String>> {
    write_parquet_to(&mut std::fs::File::create(path)?, file, digest)
}

/// Write the rest of a streamed file (see [`Outputs::trace`]) after what the run wrote, adding it
/// to `digest`, the SHA-256 of those bytes; returns the SHA-256 of the whole file.
pub fn write_parquet_continued(out: &mut std::fs::File, file: &pq::ParquetFile, mut digest: Sha256) -> std::io::Result<String> {
    write_all_pieces(out, file.pieces().inspect(|piece| digest.update(piece)))?;
    Ok(hex(&digest.finalize()))
}

/// Write `pieces` in order with vectored writes.
fn write_all_pieces<'a>(out: &mut std::fs::File, pieces: impl Iterator<Item = &'a [u8]>) -> std::io::Result<()> {
    use std::io::{IoSlice, Write};
    let mut slices: Vec<IoSlice<'_>> = pieces.filter(|p| !p.is_empty()).map(IoSlice::new).collect();
    let mut rest = &mut slices[..];
    while !rest.is_empty() {
        let n = out.write_vectored(rest)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut rest, n);
    }
    Ok(())
}

/// [`write_parquet`] into an open file.
pub fn write_parquet_to(out: &mut std::fs::File, file: &pq::ParquetFile, digest: bool) -> std::io::Result<Option<String>> {
    let mut hasher = digest.then(Sha256::new);
    write_all_pieces(
        out,
        file.pieces().inspect(|piece| {
            if let Some(h) = &mut hasher {
                h.update(piece);
            }
        }),
    )?;
    Ok(hasher.map(|h| hex(&h.finalize())))
}

fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push(HEX[(b >> 4) as usize] as char);
        hex.push(HEX[(b & 15) as usize] as char);
    }
    hex
}

pub fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

/// Peak resident set size of this process, in bytes.
pub fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    let maxrss = usage.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        maxrss
    } else {
        maxrss * 1024
    }
}
