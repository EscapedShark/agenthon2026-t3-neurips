//! Exact lookahead in the noise and value traders' private NumPy streams.
//!
//! Every action of a noise trader draws `normal(mean, std)`, `randint(0, 2)` and
//! `randint(0, offset + 1)`, in that order; every observation of a value trader with observation
//! noise draws one `standard_normal()`. Nothing else touches those generators, and none of these
//! draws depends on the market. So a helper thread computes each agent's draws ahead of the run,
//! while the event loop reads them in order.
//!
//! The agents of one kind that wake at the same interval form a group. A group's draws are stored
//! round by round (one action or observation per agent per round), with room for every wakeup the
//! horizon allows at that interval, and the helper always fills the round the run needs first: the
//! round of any group with the earliest time `round * interval`. So each group holds only the
//! rounds it can use, and a fast agent's draws never wait behind the far-off rounds of slower ones.
//! An agent that outlasts its group's rounds anyway continues on the event loop from the helper's
//! final generator state, where its draws stopped.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::rng::{RandomState, py_round};

/// One noise trader action's draws: order size, side and price offset.
#[derive(Clone, Copy)]
pub(super) struct NoiseDraw {
    size: i64,
    /// `offset << 1 | buy`.
    offset_buy: i64,
}

impl NoiseDraw {
    #[inline(always)]
    pub fn size(self) -> i64 {
        self.size
    }

    #[inline(always)]
    pub fn buy(self) -> bool {
        self.offset_buy & 1 != 0
    }

    #[inline(always)]
    pub fn offset(self) -> i64 {
        self.offset_buy >> 1
    }
}

/// The constants of a noise trader's draws.
pub(super) struct NoiseParams {
    pub mean: f64,
    pub std: f64,
    pub offset_ticks: i64,
}

/// One group's draws: `slots` agents and `rounds` rounds, round-major (the group's agent `i`, round
/// `r` at `r * slots + i`). The agents of a group act in the same bursts, each its next round, so
/// one burst's draws lie side by side.
struct Buffer<T> {
    ptr: *mut T,
    /// The allocation's capacity in elements, for freeing it.
    capacity: usize,
    slots: usize,
    rounds: usize,
    /// The time between the group's rounds (its agents' wakeup interval), which orders the helper's
    /// work across groups.
    interval: i64,
    /// Rounds written: every draw of an earlier round can be read.
    done: AtomicUsize,
}

impl<T> Buffer<T> {
    fn new(slots: usize, rounds: usize, interval: i64) -> Buffer<T> {
        // Left uninitialized: the event loop reads only what the helper has written, and the helper
        // (not this thread) then meets the fresh pages as it writes them.
        let mut v = std::mem::ManuallyDrop::new(Vec::<T>::with_capacity(slots * rounds));
        Buffer {
            ptr: v.as_mut_ptr(),
            capacity: v.capacity(),
            slots,
            rounds,
            interval,
            done: AtomicUsize::new(0),
        }
    }
}

impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {
        // SAFETY: the buffer came from `Vec::with_capacity` with this capacity (see `Buffer::new`);
        // dropped as empty, since its elements need no drop.
        unsafe { drop(Vec::from_raw_parts(self.ptr, 0, self.capacity)) };
    }
}

struct Shared {
    noise: Vec<Buffer<NoiseDraw>>,
    z: Vec<Buffer<f64>>,
    stop: AtomicBool,
    /// The generators after the last round (noise slots, value slots, in slot order), once the
    /// helper is done. Taken by [`AgentDraws::finals`].
    finals: Mutex<Option<(Vec<RandomState>, Vec<RandomState>)>>,
}

// The helper writes each draw before publishing its round with the group's `done` (release); the
// event loop reads a draw only after seeing its round published (acquire). No location is written
// twice.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// Each slot's group and place in it.
#[derive(Clone, Copy)]
struct At {
    group: u32,
    index: u32,
}

/// Group the slots by their room and interval (`rounds[slot]`), in order of first appearance.
fn groups(rounds: &[(usize, i64)]) -> (Vec<(usize, i64, usize)>, Vec<At>) {
    let mut groups: Vec<(usize, i64, usize)> = Vec::new();
    let at = rounds
        .iter()
        .map(|&(n, interval)| {
            let group = match groups.iter().position(|g| g.0 == n && g.1 == interval) {
                Some(g) => g,
                None => {
                    groups.push((n, interval, 0));
                    groups.len() - 1
                }
            };
            let index = groups[group].2;
            groups[group].2 += 1;
            At { group: group as u32, index: index as u32 }
        })
        .collect();
    (groups, at)
}

/// Where one slot's draws are, all the event loop needs to read them: its first draw, the stride
/// between its rounds (its group's slots), its group's rounds, the rounds known to be written
/// (a cached `done`), and its group's `done`.
#[derive(Clone, Copy)]
struct Slot<T> {
    first: *const T,
    stride: usize,
    rounds: usize,
    known: usize,
    done: *const AtomicUsize,
}

impl<T> Slot<T> {
    fn new(buffers: &[Buffer<T>], at: At) -> Slot<T> {
        let b = &buffers[at.group as usize];
        Slot {
            first: b.ptr.wrapping_add(at.index as usize),
            stride: b.slots,
            rounds: b.rounds,
            known: 0,
            done: &b.done,
        }
    }

    /// Draw `k` of this slot; `None` beyond its group's rounds.
    #[inline(always)]
    fn read(&mut self, k: usize) -> Option<T> {
        if k >= self.rounds {
            return None;
        }
        if k >= self.known {
            // SAFETY: the group's buffer lives as long as the lookahead (`AgentDraws::shared`).
            self.known = wait(unsafe { &*self.done }, k);
        }
        // SAFETY: in bounds, and its round is published (see `Shared`).
        Some(unsafe { self.first.add(k * self.stride).read() })
    }
}

/// Wait until round `round` of a group is published; returns the rounds published then.
#[cold]
#[inline(never)]
fn wait(done: &AtomicUsize, round: usize) -> usize {
    let mut spins = 0u32;
    loop {
        let d = done.load(Ordering::Acquire);
        if d > round {
            return d;
        }
        spins += 1;
        if spins < 1 << 12 {
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

/// The event loop's side of the lookahead.
pub(super) struct AgentDraws {
    /// Owns the buffers the slots point into.
    shared: Arc<Shared>,
    noise: Vec<Slot<NoiseDraw>>,
    z: Vec<Slot<f64>>,
    helper: Option<crate::threads::Handle<()>>,
}

impl AgentDraws {
    /// Start the helper on the noise traders' generators `noise` (the constants of their draws in
    /// `params`) and `value` (generators of value traders with observation noise). `noise_rounds`
    /// and `value_rounds` give each slot's room (the actions or observations to draw ahead) and its
    /// wakeup interval. `None` when there is nothing to draw.
    pub fn start(
        noise: Vec<RandomState>,
        params: Vec<NoiseParams>,
        noise_rounds: &[(usize, i64)],
        value: Vec<RandomState>,
        value_rounds: &[(usize, i64)],
    ) -> Option<AgentDraws> {
        if noise.is_empty() && value.is_empty() {
            return None;
        }
        debug_assert_eq!(noise.len(), params.len());
        debug_assert_eq!(noise.len(), noise_rounds.len());
        debug_assert_eq!(value.len(), value_rounds.len());
        let (noise_groups, noise_at) = groups(noise_rounds);
        let (z_groups, z_at) = groups(value_rounds);
        // Each group's generators (and constants), in the group's order.
        let mut noise_gen: Vec<Vec<(RandomState, NoiseParams)>> = noise_groups.iter().map(|g| Vec::with_capacity(g.2)).collect();
        for ((rng, p), at) in noise.into_iter().zip(params).zip(&noise_at) {
            noise_gen[at.group as usize].push((rng, p));
        }
        let mut z_gen: Vec<Vec<RandomState>> = z_groups.iter().map(|g| Vec::with_capacity(g.2)).collect();
        for (rng, at) in value.into_iter().zip(&z_at) {
            z_gen[at.group as usize].push(rng);
        }
        let shared = Arc::new(Shared {
            noise: noise_groups.iter().map(|&(rounds, interval, slots)| Buffer::new(slots, rounds, interval)).collect(),
            z: z_groups.iter().map(|&(rounds, interval, slots)| Buffer::new(slots, rounds, interval)).collect(),
            stop: AtomicBool::new(false),
            finals: Mutex::new(None),
        });
        let noise_slots = noise_at.iter().map(|&at| Slot::new(&shared.noise, at)).collect();
        let z_slots = z_at.iter().map(|&at| Slot::new(&shared.z, at)).collect();
        let producer = Arc::clone(&shared);
        let helper = crate::threads::spawn(move || produce(&producer, noise_gen, z_gen, &noise_at, &z_at));
        Some(AgentDraws { shared, noise: noise_slots, z: z_slots, helper: Some(helper) })
    }

    /// Draws of noise slot `slot`'s action number `k`; `None` beyond the lookahead.
    #[inline]
    pub fn noise(&mut self, slot: usize, k: usize) -> Option<NoiseDraw> {
        debug_assert!(slot < self.noise.len());
        // SAFETY: slots come from the traders this lookahead was started for.
        unsafe { self.noise.get_unchecked_mut(slot) }.read(k)
    }

    /// The standard normal of value slot `slot`'s observation number `k`; `None` beyond the
    /// lookahead.
    #[inline]
    pub fn z(&mut self, slot: usize, k: usize) -> Option<f64> {
        debug_assert!(slot < self.z.len());
        // SAFETY: as in `noise`.
        unsafe { self.z.get_unchecked_mut(slot) }.read(k)
    }

    /// Ask for noise slot `slot`'s action number `k` to be in cache by the time it is needed (the
    /// agent has just woken; it acts once the exchange has answered its spread query).
    #[inline(always)]
    pub fn prefetch_noise(&self, slot: usize, k: usize) {
        #[cfg(target_arch = "x86_64")]
        {
            debug_assert!(slot < self.noise.len());
            // SAFETY: as in `noise`.
            let s = unsafe { self.noise.get_unchecked(slot) };
            if k < s.rounds {
                // SAFETY: a prefetch of an address inside the buffer; it never faults or reads.
                unsafe {
                    std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(s.first.add(k * s.stride) as *const i8);
                }
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = (slot, k);
    }

    /// The generators where the lookahead stopped (noise slots, value slots), for agents that
    /// outlast it: waits for the helper to finish. Called once.
    pub fn finals(&mut self) -> (Vec<RandomState>, Vec<RandomState>) {
        if let Some(helper) = self.helper.take() {
            helper.join().expect("agent draw helper panicked");
        }
        self.shared.finals.lock().expect("agent draw finals").take().expect("the helper stores its final generators once")
    }

    /// The draws reserved, for tests: every group's slots times its rounds.
    #[cfg(test)]
    fn reserved(&self) -> usize {
        self.shared.noise.iter().map(|b| b.slots * b.rounds).sum::<usize>()
            + self.shared.z.iter().map(|b| b.slots * b.rounds).sum::<usize>()
    }
}

impl Drop for AgentDraws {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// The helper: fill every group's rounds, the one needed first next (earliest `round * interval`;
/// noise before value on a tie), then leave the generators in slot order for [`AgentDraws::finals`].
fn produce(
    shared: &Shared,
    mut noise: Vec<Vec<(RandomState, NoiseParams)>>,
    mut value: Vec<Vec<RandomState>>,
    noise_at: &[At],
    z_at: &[At],
) {
    let mut next_noise = vec![0usize; shared.noise.len()];
    let mut next_z = vec![0usize; shared.z.len()];
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let mut best: Option<(i128, bool, usize)> = None;
        for (g, b) in shared.noise.iter().enumerate() {
            if next_noise[g] < b.rounds {
                let t = next_noise[g] as i128 * b.interval as i128;
                if best.is_none_or(|(bt, _, _)| t < bt) {
                    best = Some((t, false, g));
                }
            }
        }
        for (g, b) in shared.z.iter().enumerate() {
            if next_z[g] < b.rounds {
                let t = next_z[g] as i128 * b.interval as i128;
                if best.is_none_or(|(bt, _, _)| t < bt) {
                    best = Some((t, true, g));
                }
            }
        }
        let Some((_, is_z, g)) = best else { break };
        if is_z {
            let (b, round) = (&shared.z[g], next_z[g]);
            for (i, rng) in value[g].iter_mut().enumerate() {
                let z = rng.standard_normal();
                // SAFETY: in bounds; this round is not published yet, so nothing reads it.
                unsafe { b.ptr.add(round * b.slots + i).write(z) };
            }
            next_z[g] += 1;
            b.done.store(round + 1, Ordering::Release);
        } else {
            let (b, round) = (&shared.noise[g], next_noise[g]);
            for (i, (rng, a)) in noise[g].iter_mut().enumerate() {
                let size = (py_round(rng.normal(a.mean, a.std)) as i64).max(1);
                let buy = rng.randint(0, 2) != 0;
                let offset = rng.randint(0, a.offset_ticks + 1);
                let draw = NoiseDraw { size, offset_buy: offset << 1 | buy as i64 };
                // SAFETY: in bounds; this round is not published yet, so nothing reads it.
                unsafe { b.ptr.add(round * b.slots + i).write(draw) };
            }
            next_noise[g] += 1;
            b.done.store(round + 1, Ordering::Release);
        }
    }
    // Back in slot order: each slot's generator from its group and place.
    let mut noise: Vec<std::vec::IntoIter<(RandomState, NoiseParams)>> = noise.into_iter().map(Vec::into_iter).collect();
    let mut value: Vec<std::vec::IntoIter<RandomState>> = value.into_iter().map(Vec::into_iter).collect();
    // Within a group the places follow slot order, so taking each group's next one is exact.
    let noise_finals = noise_at.iter().map(|at| noise[at.group as usize].next().expect("a generator per noise slot").0).collect();
    let value_finals = z_at.iter().map(|at| value[at.group as usize].next().expect("a generator per value slot")).collect();
    *shared.finals.lock().expect("agent draw finals") = Some((noise_finals, value_finals));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gens(n: usize, seed: u32) -> Vec<RandomState> {
        RandomState::new_many(&(0..n as u32).map(|i| seed + i).collect::<Vec<_>>())
    }

    /// Each slot's draws are its own generator's, in order, whatever its group, and the final
    /// generators continue where the lookahead stopped.
    #[test]
    fn grouped_lookahead_keeps_each_agents_stream() {
        // Slots 0 and 2 wake every 1 ms, slot 1 every 1 s, slot 3 every 10 ms.
        let noise_rounds = [(50, 1_000_000), (3, 1_000_000_000), (50, 1_000_000), (9, 10_000_000)];
        let value_rounds = [(4, 250_000_000), (40, 2_000_000)];
        let params = || {
            (0..4)
                .map(|i| NoiseParams { mean: 10.0 + i as f64, std: 2.0, offset_ticks: 3 + i })
                .collect::<Vec<_>>()
        };
        let mut draws = AgentDraws::start(gens(4, 11), params(), &noise_rounds, gens(2, 77), &value_rounds).expect("draws");
        assert_eq!(draws.reserved(), 50 + 3 + 50 + 9 + 4 + 40);
        let mut expect = gens(4, 11);
        for (slot, &(rounds, _)) in noise_rounds.iter().enumerate() {
            let p = &params()[slot];
            for k in 0..rounds {
                let rng = &mut expect[slot];
                let size = (py_round(rng.normal(p.mean, p.std)) as i64).max(1);
                let buy = rng.randint(0, 2) != 0;
                let offset = rng.randint(0, p.offset_ticks + 1);
                let d = draws.noise(slot, k).expect("within the lookahead");
                assert_eq!((d.size(), d.buy(), d.offset()), (size, buy, offset), "noise slot {slot} draw {k}");
            }
            assert!(draws.noise(slot, rounds).is_none());
        }
        let mut expect_z = gens(2, 77);
        for (slot, &(rounds, _)) in value_rounds.iter().enumerate() {
            for k in 0..rounds {
                let z = expect_z[slot].standard_normal();
                assert_eq!(draws.z(slot, k).expect("within the lookahead").to_bits(), z.to_bits(), "value slot {slot} draw {k}");
            }
            assert!(draws.z(slot, rounds).is_none());
        }
        let (noise_finals, value_finals) = draws.finals();
        for (mut a, mut b) in noise_finals.into_iter().zip(expect) {
            assert_eq!(a.standard_normal().to_bits(), b.standard_normal().to_bits());
        }
        for (mut a, mut b) in value_finals.into_iter().zip(expect_z) {
            assert_eq!(a.standard_normal().to_bits(), b.standard_normal().to_bits());
        }
    }
}
