//! Look ahead in the latency model's private NumPy stream, preserving sample order exactly.
//! Agent/oracle RNGs are independent and are never advanced here. Self-sends consume no sample.
//! A producer is either prepared before the run (an idle thread that draws nothing until the run
//! hands it the stream) or spawned lazily after sustained demand. Dropping a simulation
//! disconnects and joins it even when unused batches remain, so no thread outlives its run.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use super::latency::LatencyImpl;
use crate::rng::RandomState;

/// Samples in the producer's first block, ready soon after the run starts.
const BLOCK: usize = 2048;
/// A sleeping producer's later blocks double up to this size: fewer handoffs, each of which
/// wakes it with a system call (several microseconds inside gVisor).
const MAX_BLOCK: usize = 16384;
/// A polling producer's later blocks all have this size, and its channel holds `POLLED_DEPTH`
/// of them: its handoffs cost no system call, and while doubling blocks the producer, twice as
/// fast per sample as the run uses them, had to draw twice as many in each block and fell
/// behind at the fourth.
const POLLED_BLOCK: usize = 4096;
const POLLED_DEPTH: usize = 4;
/// Lazy producers (none prepared) start after this many samples, so short runs never spawn one.
const START_AFTER: usize = 4096;
/// With a prepared producer, samples the caller draws itself while the producer wakes up. The
/// producer draws (and discards) the same samples to reach the identical stream position.
const INLINE: usize = 1024;
/// Samples the producer draws between checks of its stop flag.
const STOP_CHECK: usize = 256;
/// How long a prepared producer sleeps between looks for room in its channel (see
/// `crate::output::POLL`).
const POLL: std::time::Duration = std::time::Duration::from_micros(20);

struct Prefetch {
    /// Taken by `start`; dropping it unstarted lets an idle producer exit.
    start: Option<SyncSender<(LatencyImpl, RandomState, usize)>>,
    receiver: Option<Receiver<Vec<i64>>>,
    recycle: SyncSender<Vec<i64>>,
    /// Set when the run is over: the producer stops within `STOP_CHECK` samples instead of
    /// finishing a block nobody will read (up to `MAX_BLOCK` samples, half a millisecond).
    stop: Arc<AtomicBool>,
    thread: Option<crate::threads::Handle<()>>,
}

impl Prefetch {
    /// A producer thread that waits, drawing nothing, until `start` hands it the stream. With
    /// `poll`, it looks for room in its channel every `POLL` instead of sleeping until the
    /// consumer wakes it (the consumer's wake is a system call on the event loop's thread).
    fn idle(poll: bool) -> Self {
        let (start, begin) = sync_channel::<(LatencyImpl, RandomState, usize)>(1);
        let (send, receiver) = sync_channel(if poll { POLLED_DEPTH } else { 2 });
        let (recycle, recycled) = sync_channel::<Vec<i64>>(if poll { POLLED_DEPTH + 2 } else { 4 });
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = crate::threads::spawn(move || {
            let Ok((model, mut rng, skip)) = begin.recv() else {
                return;
            };
            for _ in 0..skip {
                model.draw(0, 1, &mut rng);
            }
            let mut size = BLOCK;
            let mut block = vec![0; size];
            loop {
                for part in block.chunks_mut(STOP_CHECK) {
                    if stopped.load(Ordering::Relaxed) {
                        return;
                    }
                    for value in part {
                        *value = model.draw(0, 1, &mut rng);
                    }
                }
                if poll {
                    let mut pending = block;
                    loop {
                        match send.try_send(pending) {
                            Ok(()) => break,
                            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                                if stopped.load(Ordering::Relaxed) {
                                    return;
                                }
                                pending = back;
                                std::thread::sleep(POLL);
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
                        }
                    }
                } else if send.send(block).is_err() {
                    break;
                }
                size = if poll { POLLED_BLOCK } else { (size * 2).min(MAX_BLOCK) };
                block = recycled.try_recv().unwrap_or_default();
                block.resize(size, 0);
            }
        });
        Self {
            start: Some(start),
            receiver: Some(receiver),
            recycle,
            stop,
            thread: Some(thread),
        }
    }

    /// Begin producing the samples that follow the first `skip` draws of `rng`.
    fn start(&mut self, model: LatencyImpl, rng: RandomState, skip: usize) {
        let start = self.start.take().expect("producer started twice");
        start.send((model, rng, skip)).expect("latency producer exited early");
    }

    /// The next block of samples, handing `used` (a finished block) back for reuse.
    fn next_block(&mut self, used: Vec<i64>) -> Vec<i64> {
        if !used.is_empty() {
            // Never block recycling: the receiver must remain able to drain produced blocks.
            let _ = self.recycle.try_send(used);
        }
        let receiver = self.receiver.as_ref().unwrap();
        match receiver.try_recv() {
            Ok(block) => block,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                crate::marks::blocked(crate::marks::LATENCY_RECV_EMPTY, || receiver.recv()).expect("latency producer failed")
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => panic!("latency producer failed"),
        }
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.start.take());
        drop(self.receiver.take());
        self.thread.take().unwrap().join().expect("latency producer panicked");
    }
}

/// A producer that began drawing before the run (see [`LatencySampler::use_early`]).
pub(super) struct EarlyPrefetch(Prefetch);

impl EarlyPrefetch {
    /// A producer of `model`'s samples after the first `INLINE` draws of the stream seeded with
    /// `seed`, drawing from now on; none for the models the lookahead does not serve.
    pub(super) fn start(model: LatencyImpl, seed: u32) -> Option<Self> {
        if !matches!(model, LatencyImpl::LogNormal { .. } | LatencyImpl::Pareto { .. }) {
            return None;
        }
        let mut producer = Prefetch::idle(true);
        producer.start(model, RandomState::new(seed), INLINE);
        Some(EarlyPrefetch(producer))
    }
}

pub(super) struct LatencySampler {
    /// The producer's block being read, and the next sample in it (none until the run switches
    /// to the producer): kept here, so that taking a sample is one comparison and one load.
    block: Vec<i64>,
    index: usize,
    model: LatencyImpl,
    rng: RandomState,
    /// Draws left on this thread before switching to `background` (`usize::MAX`: never).
    remaining: usize,
    background: Option<Prefetch>,
    /// Lookahead is allowed without a prepared producer: spawn one after `START_AFTER` draws.
    lazy: bool,
    /// The producer started on the stream's first sample before the run (see `use_early`).
    early: bool,
}

impl LatencySampler {
    pub fn new(model: LatencyImpl, seed: u32) -> Self {
        Self {
            block: Vec::new(),
            index: 0,
            model,
            rng: RandomState::new(seed),
            remaining: usize::MAX,
            background: None,
            lazy: false,
            early: false,
        }
    }

    /// Take the samples after the first `INLINE` from `early`, a producer of this sampler's model
    /// started on its seed before the run: nothing draws from the stream before the run, so the
    /// producer continues exactly where this thread's own first draws stop, with the time the
    /// run took to set up as its head start.
    pub fn use_early(&mut self, early: EarlyPrefetch) {
        debug_assert!(self.background.is_none() && !self.lazy);
        self.background = Some(early.0);
        self.early = true;
    }

    /// Allow exact lookahead on a spare CPU. `prepare` creates the (idle) producer thread now,
    /// before the run; otherwise one is spawned only after sustained demand.
    pub fn enable_prefetch(&mut self, prepare: bool) {
        if matches!(self.model, LatencyImpl::LogNormal { .. } | LatencyImpl::Pareto { .. }) {
            if prepare {
                self.background = Some(Prefetch::idle(true));
            } else {
                self.lazy = true;
            }
        }
    }

    /// The run begins: a prepared producer receives the stream in its current state.
    pub fn start(&mut self) {
        if self.early {
            self.remaining = INLINE;
        } else if let Some(background) = &mut self.background {
            background.start(self.model.clone(), self.rng.clone(), INLINE);
            self.remaining = INLINE;
        } else if self.lazy {
            self.remaining = START_AFTER;
        }
    }

    #[inline]
    pub fn draw(&mut self, sender: u32, recipient: u32) -> i64 {
        if sender == recipient {
            return 0;
        }
        if let Some(&value) = self.block.get(self.index) {
            self.index += 1;
            return value;
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            return self.model.draw(sender, recipient, &mut self.rng);
        }
        self.next_block()
    }

    /// The draws on this thread are over, or the producer's block is used up: take its next
    /// block and its first sample.
    #[cold]
    #[inline(never)]
    fn next_block(&mut self) -> i64 {
        if self.background.is_none() {
            // Lazy: the producer continues from exactly this state.
            let mut background = Prefetch::idle(false);
            background.start(self.model.clone(), self.rng.clone(), 0);
            self.background = Some(background);
        }
        let used = std::mem::take(&mut self.block);
        self.block = self.background.as_mut().unwrap().next_block(used);
        self.index = 1;
        self.block[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Latency, LatencyModel};

    #[test]
    fn prefetched_samples_match_the_scalar_stream_across_blocks_and_self_sends() {
        for prepare in [false, true] {
        for model in [
            LatencyModel::LogNormal { mu: 8.5, sigma: 0.75 },
            LatencyModel::LogNormal { mu: 0.0, sigma: 0.0 },
            LatencyModel::Pareto { base: 100.0, alpha: 1.1 },
            LatencyModel::Pareto { base: 1.0, alpha: 1e10 },
            LatencyModel::Uniform,
            LatencyModel::Constant(-1.5),
        ] {
            for seed in [0, 17, u32::MAX] {
                let lat = Latency { model: model.clone(), min_ns: -100.0, max_ns: 1e12 };
                let mut sampler = LatencySampler::new(LatencyImpl::scenario(&lat), seed);
                let mut reference = RandomState::new(seed);
                // Start with a cached Gaussian as well as exercise the producer handoff.
                sampler.rng.standard_normal();
                reference.standard_normal();
                sampler.enable_prefetch(prepare);
                sampler.start();
                for i in 0..START_AFTER + 7 * BLOCK + 13 {
                    let recipient = u32::from(i % 11 != 0);
                    let want = if recipient == 0 { 0 } else { lat.draw(&mut reference) };
                    assert_eq!(sampler.draw(0, recipient), want, "{model:?}, seed {seed}, sample {i}");
                }
                if matches!(model, LatencyModel::LogNormal { .. } | LatencyModel::Pareto { .. }) {
                    assert!(sampler.background.is_some());
                } else {
                    assert!(sampler.background.is_none());
                    assert_eq!(sampler.rng.state(), reference.state());
                }
            }
        }
        }
    }

    /// A producer started before the run on the sampler's seed serves the stream from its first
    /// sample.
    #[test]
    fn early_producer_matches_the_scalar_stream() {
        for model in [
            LatencyModel::LogNormal { mu: 8.5, sigma: 0.75 },
            LatencyModel::Pareto { base: 100.0, alpha: 1.1 },
        ] {
            for seed in [0, 17, u32::MAX] {
                let lat = Latency { model: model.clone(), min_ns: -100.0, max_ns: 1e12 };
                let mut sampler = LatencySampler::new(LatencyImpl::scenario(&lat), seed);
                let early = EarlyPrefetch::start(LatencyImpl::scenario(&lat), seed).expect("served model");
                sampler.use_early(early);
                sampler.start();
                let mut reference = RandomState::new(seed);
                for i in 0..7 * BLOCK + 13 {
                    let recipient = u32::from(i % 11 != 0);
                    let want = if recipient == 0 { 0 } else { lat.draw(&mut reference) };
                    assert_eq!(sampler.draw(0, recipient), want, "{model:?}, seed {seed}, sample {i}");
                }
                assert_eq!(sampler.remaining, 0);
                assert!(sampler.background.is_some());
            }
        }
        let uniform = Latency { model: LatencyModel::Uniform, min_ns: 0.0, max_ns: 10.0 };
        assert!(EarlyPrefetch::start(LatencyImpl::scenario(&uniform), 1).is_none());
    }

    #[test]
    fn early_stop_joins_a_producer_with_unconsumed_batches() {
        for draws in [0, 1, INLINE, INLINE + 1, START_AFTER + 1] {
            for (prepare, start) in [(false, true), (true, false), (true, true)] {
                let mut sampler = LatencySampler::new(LatencyImpl::LogNormal {
                    mu: 8.5, sigma: 0.75, min_ns: 0.0, max_ns: 1e12,
                }, 17);
                sampler.enable_prefetch(prepare);
                if start {
                    sampler.start();
                    for _ in 0..draws {
                        std::hint::black_box(sampler.draw(0, 1));
                    }
                }
                assert_eq!(sampler.background.is_some(), prepare || draws > START_AFTER);
                drop(sampler);
            }
        }
    }
}
