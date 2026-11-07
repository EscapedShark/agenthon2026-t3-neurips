#![cfg_attr(not(test), no_main)]

//! The submission entry point: `simulate` and `simulate-batch` (also dispatched on argv[0], so the
//! image can ship this binary under both verb names).
//!
//!   fastsim simulate --config /input/scenario.json --out /output/trace.parquet [--seed N] [--ledger MODE]
//!   fastsim simulate-batch --batch-dir /input/scenarios --out-dir /output
//!
//! `--ledger auto` (the default) writes `message_trace.parquet` unless the unit's card, found next to
//! the config, declares it not required; `always` / `never` override (testing). Batch sub-scenarios
//! always need a ledger. A single scenario encodes on `budget - 1` background threads while it
//! simulates and writes its outputs as the row groups are encoded (`FASTSIM_STREAM=0`: after the
//! run). Batch sub-scenarios run in parallel, each encoding on its own thread;
//! threads with no sub left to start take over the outputs and encoding jobs of the others.
//!
//! The C runtime calls `main` directly (`#![no_main]`): Rust's own runtime setup costs every run
//! about 35 µs of system calls (it reads `/proc/self/maps` to find the main thread's stack and
//! installs a stack-overflow handler on an alternate signal stack). Of what it does, this program
//! needs two things, which `main` does itself: standard descriptors that are open, and SIGPIPE
//! ignored. `std::env::args` does not depend on it (glibc hands the arguments to an init hook).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use fastsim::card::{cpu_budget, Card};
use fastsim::output;
use fastsim::scenario::Scenario;
use serde_json::{json, Value};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

struct RunResult {
    n_events: usize,
    trace_sha256: String,
    events: Value,
}

/// A run's output files, opened before its outputs are ready to write.
struct OutFiles {
    trace: File,
    ledger: Option<File>,
    events: File,
}

impl OutFiles {
    /// Create the directory of `trace` and the run's files in it.
    fn create(trace: &Path, ledger: bool) -> Result<OutFiles, String> {
        std::fs::create_dir_all(Self::dir(trace)).map_err(|e| format!("create output directory: {e}"))?;
        Self::create_files(trace, ledger)
    }

    /// As [`OutFiles::create`], but without checking for the directory first: it usually exists,
    /// and then each file takes one system call (several microseconds inside gVisor).
    fn create_in_place(trace: &Path, ledger: bool) -> Result<OutFiles, String> {
        Self::create_files(trace, ledger).or_else(|_| Self::create(trace, ledger))
    }

    fn create_files(trace: &Path, ledger: bool) -> Result<OutFiles, String> {
        let dir = Self::dir(trace);
        let create = |path: &Path| File::create(path).map_err(|e| format!("create {}: {e}", path.display()));
        Ok(OutFiles {
            trace: create(trace)?,
            ledger: if ledger { Some(create(&dir.join("message_trace.parquet"))?) } else { None },
            events: create(&dir.join("events.json"))?,
        })
    }

    fn dir(trace: &Path) -> &Path {
        trace.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."))
    }
}

/// Simulate a scenario on its own and write `trace.parquet` (at `out`), the ledger when
/// requested, and `events.json` next to it. `t0` only anchors the `FASTSIM_TIMING` phase log;
/// `wall_clock_sec` is the simulation's own elapsed time.
fn run_single(doc: &Value, sc: &Scenario, out: &Path, t0: Instant, ledger: bool, workers: usize, stream: bool) -> Result<(RunResult, output::Outputs), String> {
    let mode = output::Mode::Single;
    // The helper threads the run will use are created by one launcher thread while this one
    // prepares the run (see `fastsim::threads`). Inside gVisor a thread being created slows file
    // and memory-mapping calls of the others, so the files are created before.
    let prestart = || {
        fastsim::threads::prestart(output::helper_threads(sc, ledger, workers, mode));
        fastsim::marks::mark("prestart");
    };
    // With `stream` (see `streams`), the scenario creates its files first and writes the trace
    // and the ledger while it runs, hashing the trace as it goes: the work then hides behind the
    // simulation instead of following it. Otherwise it writes them after the run, as a batch sub
    // does: there the reported clock covers the simulation alone, and writing beside it would
    // only compete with it for memory bandwidth (and, on a shared core, for cycles). The bytes
    // are the same either way.
    let (outputs, files) = if stream {
        let OutFiles { trace, ledger: ledger_file, events } = OutFiles::create_in_place(out, ledger)?;
        fastsim::marks::mark("files");
        prestart();
        let mut outputs = output::simulate_with(sc, ledger, workers, mode, Some(trace), ledger_file);
        let files = OutFiles {
            trace: outputs.trace_file.take().expect("the streamed trace file comes back"),
            ledger: outputs.ledger_file.take(),
            events,
        };
        (outputs, files)
    } else {
        prestart();
        let outputs = output::simulate_with(sc, ledger, workers, mode, None, None);
        (outputs, OutFiles::create_in_place(out, ledger)?)
    };
    write_outputs(doc, outputs, files, t0)
}

/// Write a run's encoded outputs into its files and `events.json`; returns its record and the
/// encoded outputs, for the caller to free.
fn write_outputs(doc: &Value, mut outputs: output::Outputs, files: OutFiles, t0: Instant) -> Result<(RunResult, output::Outputs), String> {
    let timing = std::env::var_os("FASTSIM_TIMING").is_some();
    let phase = |name: &str| {
        if timing {
            eprintln!("[timing] {name}: {:.2} ms", t0.elapsed().as_secs_f64() * 1e3);
        }
    };
    if let Some(e) = outputs.stream_error.take() {
        return Err(format!("write outputs: {e}"));
    }
    phase("simulated + encoded");
    fastsim::marks::mark("finished");
    let OutFiles { trace: mut trace_file, ledger: ledger_file, events: mut events_file } = files;
    let streamed_digest = outputs.trace_digest.take();
    // Only the trace's digest is part of the record (`trace_sha256`); the scorer hashes the
    // ledger itself, so it is written without one. A streamed trace has most of its bytes written
    // and hashed; otherwise a large trace is hashed on a second thread while this one writes both
    // files, and a small one is hashed as it is written.
    let write_ledger = |file: Option<File>| -> Result<(), String> {
        if let (Some(encoded), Some(mut file)) = (&outputs.ledger, file) {
            output::write_parquet_to(&mut file, encoded, false).map_err(|e| format!("write ledger: {e}"))?;
        }
        Ok(())
    };
    let trace_sha256 = if let Some(digest) = streamed_digest {
        let digest = output::write_parquet_continued(&mut trace_file, &outputs.trace, digest).map_err(|e| format!("write trace: {e}"))?;
        write_ledger(ledger_file)?;
        digest
    } else if outputs.trace.len() >= 256 * 1024 {
        let hashing = AtomicBool::new(false);
        std::thread::scope(|s| -> Result<String, String> {
            let digest = s.spawn(|| {
                hashing.store(true, Ordering::Release);
                output::hash_parquet(&outputs.trace)
            });
            // Inside gVisor a new thread's start waits for a large write in progress (both need
            // the address space), which would run the hash after the write instead of beside
            // it: gb-mega's tail took 8 ms instead of 4. Writing starts once the thread runs.
            while !hashing.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            output::write_parquet_to(&mut trace_file, &outputs.trace, false).map_err(|e| format!("write trace: {e}"))?;
            write_ledger(ledger_file)?;
            Ok(digest.join().expect("hash thread panicked"))
        })?
    } else {
        let digest = output::write_parquet_to(&mut trace_file, &outputs.trace, true).map_err(|e| format!("write trace: {e}"))?;
        write_ledger(ledger_file)?;
        digest.expect("digest requested")
    };
    phase("hashed + written");
    fastsim::marks::mark("written");
    // The simulation's own elapsed time, as the baseline reports it (see `Outputs::sim_seconds`).
    let wall = outputs.sim_seconds.max(1e-9);

    let n_events = outputs.trace_rows;
    let mut events = json!({
        "scenario_id": doc["scenario_id"],
        "seed": doc["seed"],
        "n_events": n_events,
        "wall_clock_sec": wall,
        "events_per_sec": n_events as f64 / wall,
        "trace_sha256": trace_sha256,
        "peak_memory_bytes": output::peak_rss_bytes(),
        "gpu_seconds": 0.0,
    });
    if outputs.ledger.is_some() {
        events["n_messages"] = json!(outputs.ledger_rows);
    }
    let text = serde_json::to_string_pretty(&events).expect("events json") + "\n";
    events_file.write_all(text.as_bytes()).map_err(|e| format!("write events: {e}"))?;
    phase("events.json written");
    fastsim::marks::mark("events");
    Ok((RunResult { n_events, trace_sha256, events }, outputs))
}

fn load(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn simulate(args: &[String]) -> Result<(), String> {
    let t0 = Instant::now();
    let config = PathBuf::from(arg(args, "--config").ok_or("--config is required")?);
    let out = PathBuf::from(arg(args, "--out").ok_or("--out is required")?);
    let mut doc = load(&config)?;
    fastsim::marks::mark("loaded");
    if let Some(seed) = arg(args, "--seed") {
        doc["seed"] = Value::from(seed.parse::<i64>().map_err(|e| format!("--seed: {e}"))?);
    }
    let card = Card::load(config.parent().unwrap_or(Path::new(".")));
    fastsim::marks::mark("card");
    let ledger = match arg(args, "--ledger").as_deref() {
        None | Some("auto") => card.requires_message_ledger(),
        Some("always") => true,
        Some("never") => false,
        Some(other) => return Err(format!("--ledger must be auto, always or never, not {other:?}")),
    };
    let workers = cpu_budget(&card) - 1;
    let sc = Scenario::from_value(&doc);
    fastsim::marks::mark("scenario");
    let (r, outputs) = run_single(&doc, &sc, &out, t0, ledger, workers, streams())?;
    println!("{}", serde_json::to_string(&r.events).expect("events json"));
    // The encoded files are on disk. Freeing them piece by piece would only delay the exit: the
    // process ends right after, and the kernel reclaims everything at once.
    std::mem::forget(outputs);
    finish()
}

/// Every output is written and stdout flushed: end the process now instead of unwinding
/// destructors for memory the kernel reclaims anyway.
fn finish() -> ! {
    let _ = std::io::stdout().flush();
    fastsim::marks::mark("exit");
    fastsim::marks::dump();
    std::process::exit(0)
}

/// The order subs start in and the helper threads each may use, from their estimated sizes. The
/// largest start first, so no long simulation is left for last. A sub well above an even share of
/// the batch would still finish last, so it gets the helper threads of a single scenario while the
/// rest share the other CPUs.
fn batch_plan(cost: &[f64], cpus: usize) -> (Vec<usize>, Vec<usize>) {
    let mut order: Vec<usize> = (0..cost.len()).collect();
    order.sort_by(|&a, &b| cost[b].total_cmp(&cost[a]).then(a.cmp(&b)));
    let mut helpers = vec![0; cost.len()];
    let total: f64 = cost.iter().sum();
    // Creating a thread costs 40-120 us on a virtual machine, a large part of a typical sub, so
    // only a sub that is clearly the batch's critical path gets helpers.
    if cost.len() > 1 && cost[order[0]] > 1.5 * total / cpus as f64 {
        helpers[order[0]] = cpus - 1;
    }
    (order, helpers)
}

/// Something a helper thread creates ahead of its owner, unless the owner comes first.
struct Prepared<T>(Mutex<Slot<T>>);

enum Slot<T> {
    Pending,
    Ready(T),
    Taken,
}

impl<T> Prepared<T> {
    fn new() -> Self {
        Prepared(Mutex::new(Slot::Pending))
    }

    /// Run `make` unless the owner has already taken the slot. On failure the slot stays empty
    /// and the owner creates the value itself (and reports the error).
    fn prepare(&self, make: impl FnOnce() -> Option<T>) {
        let mut slot = self.0.lock().expect("prepared slot");
        if matches!(*slot, Slot::Pending) {
            if let Some(value) = make() {
                *slot = Slot::Ready(value);
            }
        }
    }

    /// The prepared value, waiting for a preparation in progress; `None` if there is none.
    fn take(&self) -> Option<T> {
        match std::mem::replace(&mut *self.0.lock().expect("prepared slot"), Slot::Taken) {
            Slot::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// Spin, then yield, until `done` holds. Only for waits of a few microseconds among threads that
/// each have a CPU of their own. The spins are counted rather than timed: early in a gVisor
/// sandbox, reading the clock is a system call.
fn wait_for(done: impl Fn() -> bool) {
    let mut spins = 0u32;
    while !done() {
        if spins < 5_000 {
            spins += 1;
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

type Parsed = Result<(Value, Scenario, f64), String>;

/// One batch's work, shared by its threads.
struct Batch {
    subs: Vec<PathBuf>,
    /// Each sub's trace path (its files go to the same directory).
    outs: Vec<PathBuf>,
    cpus: usize,
    files: Vec<Prepared<OutFiles>>,
    batch_file: Prepared<File>,
    parsed: Vec<OnceLock<Parsed>>,
    results: Vec<OnceLock<Result<RunResult, String>>>,
    next_load: AtomicUsize,
    loaded: AtomicUsize,
    next_run: AtomicUsize,
    /// Subs whose simulation is over and whose outputs wait for a thread (see `work`).
    deferred: Mutex<Vec<(usize, output::Running)>>,
    /// How many are in `deferred`, read without its lock.
    pending_tails: AtomicUsize,
    /// Subs with a result.
    done: AtomicUsize,
    /// Encoding jobs of running subs, for the threads that have no sub left to start.
    help: Arc<output::Help>,
}

impl Batch {
    fn new(subs: Vec<PathBuf>, out_dir: &Path, cpus: usize) -> Batch {
        let n = subs.len();
        // Every batch sub requires its ledger.
        let outs = subs.iter().map(|p| out_dir.join(stem(p)).join("trace.parquet")).collect();
        Batch {
            subs,
            outs,
            cpus,
            files: (0..n).map(|_| Prepared::new()).collect(),
            batch_file: Prepared::new(),
            parsed: (0..n).map(|_| OnceLock::new()).collect(),
            results: (0..n).map(|_| OnceLock::new()).collect(),
            next_load: AtomicUsize::new(0),
            loaded: AtomicUsize::new(0),
            next_run: AtomicUsize::new(0),
            deferred: Mutex::new(Vec::new()),
            pending_tails: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            help: Arc::new(output::Help::default()),
        }
    }

    /// Creating a directory is the slowest step of writing a sub's files (a new directory's
    /// metadata may need a disk read, and creations in one parent directory are serialized), so a
    /// helper creates every sub's directory and files while the subs load and simulate.
    fn prepare_outputs(&self, out_dir: &Path) {
        for (slot, out) in self.files.iter().zip(&self.outs) {
            slot.prepare(|| OutFiles::create(out, true).ok());
        }
        self.batch_file.prepare(|| File::create(out_dir.join("batch_events.json")).ok());
    }

    /// Every thread loads subs until all are parsed, then runs them in the plan's order.
    ///
    /// Simulations start first. A thread whose simulation ends while subs remain to start, but
    /// fewer than the batch has threads, leaves the sub's outputs to a thread with none left to
    /// start and starts the next sub: otherwise the last subs would start only after the outputs
    /// of the ones before, while other threads wait. A thread with no sub left to start finishes
    /// the outputs left to it and takes encoding jobs from the subs still running.
    fn work(&self) {
        let n = self.subs.len();
        loop {
            let k = self.next_load.fetch_add(1, Ordering::Relaxed);
            if k >= n {
                break;
            }
            let r = load(&self.subs[k]).map(|doc| {
                let sc = Scenario::from_value(&doc);
                let (trace, ledger) = sc.estimated_rows();
                (doc, sc, trace + ledger)
            });
            let _ = self.parsed[k].set(r);
            self.loaded.fetch_add(1, Ordering::Release);
        }
        // The plan needs every estimate; each thread derives the same one.
        wait_for(|| self.loaded.load(Ordering::Acquire) == n);
        let cost: Vec<f64> = self.parsed.iter().map(|p| p.get().and_then(|r| r.as_ref().ok()).map_or(0.0, |r| r.2)).collect();
        let (order, helpers) = batch_plan(&cost, self.cpus);
        loop {
            let k = self.next_run.fetch_add(1, Ordering::Relaxed);
            if k >= n {
                break;
            }
            let i = order[k];
            let sc = match self.parsed[i].get().expect("every sub parsed") {
                Ok((_, sc, _)) => sc,
                Err(e) => {
                    self.record(i, Err(e.clone()));
                    continue;
                }
            };
            let t0 = Instant::now();
            let running = output::start(sc, true, helpers[i], output::Mode::BatchSub, None, None, Some(Arc::clone(&self.help)));
            let unstarted = n.saturating_sub(self.next_run.load(Ordering::Relaxed));
            if unstarted > 0 && unstarted < self.cpus {
                self.deferred.lock().expect("deferred subs").push((i, running));
                self.pending_tails.fetch_add(1, Ordering::Release);
                self.help.ring();
            } else {
                self.finish(i, running, t0);
            }
        }
        self.help.enlist();
        let mut scratch = fastsim::pq::Scratch::default();
        loop {
            let deferred = self.deferred.lock().expect("deferred subs").pop();
            if let Some((i, running)) = deferred {
                self.pending_tails.fetch_sub(1, Ordering::Relaxed);
                self.finish(i, running, Instant::now());
            } else if !self.help.run_one(&mut scratch) {
                if self.done.load(Ordering::Acquire) == n {
                    break;
                }
                self.help.wait(|| {
                    self.pending_tails.load(Ordering::Acquire) > 0 || self.help.has_jobs() || self.done.load(Ordering::Acquire) == n
                });
            }
        }
    }

    /// Encode sub `i`'s outputs and write them into the files the helper prepared.
    fn finish(&self, i: usize, running: output::Running, t0: Instant) {
        let doc = match self.parsed[i].get().expect("every sub parsed") {
            Ok((doc, _, _)) => doc,
            Err(_) => unreachable!("only parsed subs run"),
        };
        let outputs = running.finish();
        let r = match self.files[i].take() {
            Some(files) => Ok(files),
            None => OutFiles::create(&self.outs[i], true),
        }
        .and_then(|files| write_outputs(doc, outputs, files, t0))
        // The encoded files are freed here, for the next sub on this thread to reuse.
        .map(|(r, _outputs)| r);
        self.record(i, r);
    }

    fn record(&self, i: usize, r: Result<RunResult, String>) {
        let _ = self.results[i].set(r);
        if self.done.fetch_add(1, Ordering::AcqRel) + 1 == self.subs.len() {
            self.help.ring();
        }
    }
}

fn stem(p: &Path) -> String {
    p.file_stem().and_then(|x| x.to_str()).unwrap_or("sub").to_string()
}

fn simulate_batch(args: &[String]) -> Result<(), String> {
    let batch_dir = PathBuf::from(arg(args, "--batch-dir").ok_or("--batch-dir is required")?);
    let out_dir = PathBuf::from(arg(args, "--out-dir").ok_or("--out-dir is required")?);
    // The container's configuration and the threads are set up before the clock, as a process
    // starts before its work: the card gives the CPU budget, and the threads wait idle until the
    // subs are listed. Nothing is read from the subs before the clock starts.
    let card = Card::load(batch_dir.parent().unwrap_or(Path::new(".")));
    let cpus = cpu_budget(&card);
    // `None` when there is nothing to run.
    let published: OnceLock<Option<(Batch, Instant)>> = OnceLock::new();
    let wait = || {
        wait_for(|| published.get().is_some());
        published.get().and_then(Option::as_ref)
    };
    let listed = std::thread::scope(|s| {
        s.spawn(|| {
            if let Some((batch, _)) = wait() {
                batch.prepare_outputs(&out_dir);
            }
        });
        for _ in 1..cpus {
            s.spawn(|| {
                if let Some((batch, _)) = wait() {
                    batch.work();
                }
            });
        }
        let listed = list_subs(&batch_dir);
        // The baseline's batch clock: every sub's load, simulation and writes, after the listing.
        let t0 = Instant::now();
        let batch = listed.as_ref().ok().map(|subs| (Batch::new(subs.clone(), &out_dir, cpus), t0));
        let _ = published.set(batch);
        if let Some((batch, _)) = published.get().and_then(Option::as_ref) {
            batch.work();
        }
        listed
    });
    let subs = listed?;
    let (batch, t0) = published.into_inner().flatten().expect("a listed batch");

    let mut per_scenario = Vec::with_capacity(subs.len());
    let mut total_events = 0usize;
    for (path, r) in subs.iter().zip(batch.results) {
        let r = r.into_inner().expect("every sub ran")?;
        total_events += r.n_events;
        per_scenario.push(json!({
            "sub": stem(path),
            "n_events": r.n_events,
            "trace_sha256": r.trace_sha256,
        }));
    }
    let wall = t0.elapsed().as_secs_f64();
    let summary = json!({
        "n_scenarios": subs.len(),
        "total_events": total_events,
        "wall_clock_sec": wall,
        "events_per_sec": total_events as f64 / wall,
        "peak_memory_bytes": output::peak_rss_bytes(),
        "gpu_seconds": 0.0,
        "per_scenario": per_scenario,
    });
    let text = serde_json::to_string_pretty(&summary).expect("batch json") + "\n";
    match batch.batch_file.take() {
        Some(mut file) => file.write_all(text.as_bytes()),
        None => output::write_file(&out_dir.join("batch_events.json"), text.as_bytes()),
    }
    .map_err(|e| format!("write batch_events: {e}"))?;
    println!("{}", serde_json::to_string(&summary).expect("batch json"));
    finish()
}

/// The batch's sub-scenario files, in name order.
fn list_subs(batch_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut subs: Vec<PathBuf> = std::fs::read_dir(batch_dir)
        .map_err(|e| format!("read {}: {e}", batch_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    subs.sort();
    if subs.is_empty() {
        return Err(format!("no sub-scenarios (*.json) in {}", batch_dir.display()));
    }
    Ok(subs)
}

/// Whether a scenario on its own writes its outputs while it runs (see `run_single`).
///
/// Development and the Final both time the whole container, and both mount the unit's card next
/// to its scenario: written as the run goes, the outputs hide behind the simulation instead of
/// following it (a fifth of the process on large units), so a single scenario always streams.
/// `FASTSIM_STREAM=0` writes after the run instead (testing); the image sets the default,
/// `FASTSIM_STREAM=1`.
fn streams() -> bool {
    std::env::var_os("FASTSIM_STREAM").is_none_or(|v| v != "0")
}

/// The process entry point (see the module notes). A test build keeps the harness's own.
#[cfg(not(test))]
#[unsafe(no_mangle)]
extern "C" fn main(_argc: std::ffi::c_int, _argv: *const *const std::ffi::c_char) -> std::ffi::c_int {
    #[cfg(unix)]
    unsafe {
        // Writes to a closed pipe fail instead of ending the process.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        // A closed standard descriptor is reopened on /dev/null, so no file this run opens can
        // take its number and receive what is printed.
        let mut fds = [0, 1, 2].map(|fd| libc::pollfd { fd, events: 0, revents: 0 });
        if libc::poll(fds.as_mut_ptr(), 3, 0) > 0 {
            for f in &fds {
                if f.revents & libc::POLLNVAL != 0 {
                    let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                    if fd != f.fd && fd >= 0 {
                        libc::dup2(fd, f.fd);
                        libc::close(fd);
                    }
                }
            }
        }
    }
    run();
    0
}

#[cfg_attr(test, allow(dead_code))]
fn run() {
    fastsim::marks::init();
    // Large buffers stay on the heap and freed memory stays mapped, so a buffer one phase of a run
    // frees (or the previous sub on a batch thread) is reused without faulting its pages in again.
    // glibc otherwise maps each allocation above 128 KiB on its own and unmaps it when freed.
    // Each thread's arena also starts with 64 MiB of usable heap: glibc grows a thread's heap one
    // mprotect call at a time otherwise (1,785 calls in the largest unit, each a system call that
    // costs microseconds inside gVisor). Untouched pages cost nothing.
    // With the heap that large, a kernel with transparent huge pages set to `always` backs it with
    // 2 MiB pages, and the simulation ran 3-7% slower natively. Huge pages are therefore off for
    // this process (gVisor rejects the call, and its memory never uses the host's THP).
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 32 << 20);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 30);
        libc::mallopt(libc::M_TOP_PAD, 64 << 20);
        libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0);
    }
    fastsim::marks::mark("malloc_cfg");
    let mut args: Vec<String> = std::env::args().collect();
    let invoked_as = Path::new(&args[0]).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    let verb = match invoked_as.as_str() {
        "simulate" | "simulate-batch" => {
            // `simulate simulate --config ...` and `simulate --config ...` are both accepted.
            if args.get(1).map(String::as_str) == Some(invoked_as.as_str()) {
                args.remove(1);
            }
            invoked_as
        }
        _ => {
            if args.len() < 2 {
                eprintln!("usage: fastsim simulate --config SCENARIO --out TRACE [--seed N] [--ledger auto|always|never]");
                eprintln!("       fastsim simulate-batch --batch-dir DIR --out-dir OUT");
                std::process::exit(2);
            }
            args.remove(1)
        }
    };
    let result = match verb.as_str() {
        "simulate" => simulate(&args),
        "simulate-batch" => simulate_batch(&args),
        other => Err(format!("unknown verb {other:?}")),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
