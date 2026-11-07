//! The kernel's event queue: ABIDES's single `PriorityQueue` ordered by
//! `(time, sender, recipient, message_id)`, split into two heaps by event kind.
//!
//! Agents' schedules are aligned to the market open, so many agents wake at the same instant and
//! bursts of hundreds of messages are in flight at once; both heaps routinely hold 100+ events.
//! Each heap is 4-ary (half the depth of a binary heap) with the whole ordering key packed into one
//! `u128` — time (sign-flipped), sender, recipient, message id — so choosing the smallest of four
//! children is branch-free integer comparison. Popping the smaller of the two heads yields exactly
//! the single queue's order. Packing needs ids below 2^16 (agents) and 2^32 (messages); beyond
//! that the queue switches, once, to binary heaps comparing the full tuple.
//!
//! Messages found to address a busy agent can share one deadline. Their private heap orders by
//! sender/id/slot; one entry per agent in a small indexed heap of groups represents its head.
//! Advancing that deadline requeues the whole waiting group once, without repeatedly touching
//! every message. Only events actually encountered by the kernel are parked, preserving the
//! first-event-past-stop rule.

use std::cmp::Reverse;
use std::hint::select_unpredictable;
use std::collections::{BinaryHeap, VecDeque};

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct Key {
    pub time: i64,
    // Unsigned lexicographic (sender, recipient) comparison in one machine word.
    route: u64,
    pub msg_id: u64,
    pub slot: u32,
}

impl Key {
    #[inline]
    pub fn new(time: i64, sender: u32, recipient: u32, msg_id: u64, slot: u32) -> Self {
        Self { time, route: ((sender as u64) << 32) | recipient as u64, msg_id, slot }
    }

    #[inline]
    pub fn sender(self) -> u32 {
        (self.route >> 32) as u32
    }

    #[inline]
    pub fn recipient(self) -> u32 {
        self.route as u32
    }

    /// The order-preserving `u128` form, when every id fits. No packed key is `u128::MAX` (its id
    /// would be `u32::MAX`), which the lanes use to mean "empty" (see [`EMPTY`]).
    #[inline(always)]
    fn packed(self) -> Option<u128> {
        pack(self.time, self.sender(), self.recipient(), self.msg_id)
    }

    #[inline(always)]
    fn unpack(packed: u128, slot: u32) -> Key {
        let low = packed as u64;
        Key::new(time_of(packed), (low >> 48) as u32, (low >> 32) as u32 & 0xFFFF, low & 0xFFFF_FFFF, slot)
    }
}

/// The key a lane's `top` gives when the lane is empty: larger than every packed key.
const EMPTY: u128 = u128::MAX;

/// [`Key::packed`] of a key's parts.
#[inline(always)]
fn pack(time: i64, sender: u32, recipient: u32, msg_id: u64) -> Option<u128> {
    if (sender | recipient) > 0xFFFF || msg_id >= 0xFFFF_FFFF {
        return None;
    }
    let time = (time as u64 ^ (1 << 63)) as u128;
    Some(time << 64 | (sender as u128) << 48 | (recipient as u128) << 32 | msg_id as u128)
}

#[inline(always)]
fn time_of(packed: u128) -> i64 {
    ((packed >> 64) as u64 ^ (1 << 63)) as i64
}

/// Which structure an event is pushed into; any assignment preserves the global order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lane {
    Wakeup,
    Message,
}

/// Where the next event is, as returned by [`EventQueue::peek`]: precisely enough that removing
/// or requeueing it goes straight to the structure that holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Pos {
    Wakeup,
    /// The message ring's smallest entry.
    Ring,
    /// The root of the ring's overflow heap.
    Overflow,
    /// The message lane's heap, before it chooses its form (see `PROBE_POPS`).
    Heap,
    /// The front of the ordered message lane (constant latencies).
    Sorted,
    TupleWakeup,
    TupleMessage,
    Waiting,
}

#[derive(Default)]
struct WaitingAgent {
    time: i64,
    observed_deadline: i64,
    deferrals: u8,
    enabled: bool,
    /// The group's place in `Waiting::groups` while it has messages.
    place: Option<u32>,
    /// Its first message's full key (while it has messages).
    head: Key,
    // The recipient and deadline are shared; retain the rest of the complete ordering key.
    messages: Group,
}

impl WaitingAgent {
    fn first(&self, recipient: u32) -> Option<Key> {
        let (sender, id, slot) = Group::unpack(self.messages.first()?);
        Some(Key::new(self.time, sender, recipient, id, slot))
    }
}

/// The messages waiting for one agent, in `(sender, message id, slot)` order, each packed into a
/// `u128` (`sender << 96 | id << 32 | slot`) that compares the same way.
enum Group {
    /// Ascending: the next message first. One sender's burst arrives in order and appends at the
    /// back; a message that belongs earlier shifts the shorter side, a few entries at these sizes.
    Sorted(VecDeque<u128>),
    /// A group grown past `SORTED_GROUP` messages (no public unit's does): a heap keeps any
    /// arrival order at logarithmic cost.
    Heap(BinaryHeap<Reverse<u128>>),
}

const SORTED_GROUP: usize = 64;

impl Default for Group {
    fn default() -> Self {
        Group::Sorted(VecDeque::new())
    }
}

impl Group {
    #[inline]
    fn pack(sender: u32, id: u64, slot: u32) -> u128 {
        (sender as u128) << 96 | (id as u128) << 32 | slot as u128
    }

    #[inline]
    fn unpack(m: u128) -> (u32, u64, u32) {
        ((m >> 96) as u32, (m >> 32) as u64, m as u32)
    }

    #[inline]
    fn first(&self) -> Option<u128> {
        match self {
            Group::Sorted(sorted) => sorted.front().copied(),
            Group::Heap(heap) => heap.peek().map(|m| m.0),
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        match self {
            Group::Sorted(sorted) => sorted.is_empty(),
            Group::Heap(heap) => heap.is_empty(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        match self {
            Group::Sorted(sorted) => sorted.len(),
            Group::Heap(heap) => heap.len(),
        }
    }

    #[inline]
    fn push(&mut self, m: u128) {
        match self {
            Group::Sorted(sorted) if sorted.len() < SORTED_GROUP => {
                if sorted.back().is_none_or(|&last| last < m) {
                    sorted.push_back(m);
                } else {
                    let at = sorted.partition_point(|&x| x < m);
                    sorted.insert(at, m);
                }
            }
            Group::Sorted(sorted) => {
                let mut heap: BinaryHeap<Reverse<u128>> = sorted.drain(..).map(Reverse).collect();
                heap.push(Reverse(m));
                *self = Group::Heap(heap);
            }
            Group::Heap(heap) => heap.push(Reverse(m)),
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<u128> {
        match self {
            Group::Sorted(sorted) => sorted.pop_front(),
            Group::Heap(heap) => {
                let m = heap.pop().map(|m| m.0);
                if heap.is_empty() {
                    *self = Group::default();
                }
                m
            }
        }
    }
}

/// The waiting groups: one entry per agent with parked messages, in a binary min-heap of agent
/// ids ordered by their groups' heads. Each group knows its place, so a new head or deadline moves
/// its one entry instead of adding another. A run rarely has more than two groups at once.
#[derive(Default)]
struct Waiting {
    agents: Vec<WaitingAgent>,
    groups: Vec<u32>,
}

impl Waiting {
    fn can_park(&mut self, recipient: u32, until: i64) -> bool {
        // Keep the original full-width tuple path, without allocating a sparse u32-sized table.
        if recipient > 0xFFFF {
            return false;
        }
        let r = recipient as usize;
        if r >= self.agents.len() {
            self.agents.resize_with(r + 1, WaitingAgent::default);
        }
        let a = &mut self.agents[r];
        if !a.enabled {
            if a.observed_deadline != until {
                a.observed_deadline = until;
                a.deferrals = 1;
            } else {
                a.deferrals += 1;
            }
            // Isolated collisions are cheaper as an in-place heap update. A dense burst
            // identifies recipients for which the extra per-agent heap will pay for itself.
            a.enabled = a.deferrals >= 8;
            if !a.enabled {
                return false;
            }
        }
        a.messages.is_empty() || a.time == until
    }

    fn park(&mut self, key: Key, until: i64) {
        let r = key.recipient() as usize;
        if r >= self.agents.len() {
            self.agents.resize_with(r + 1, WaitingAgent::default);
        }
        let agent = &mut self.agents[r];
        // A group that already waits has this deadline (see `can_park`): its head can only get
        // smaller.
        debug_assert!(agent.messages.is_empty() || agent.time == until);
        agent.time = until;
        agent.messages.push(Group::pack(key.sender(), key.msg_id, key.slot));
        let head = agent.first(r as u32).expect("a parked message");
        match agent.place {
            None => {
                agent.head = head;
                let place = self.groups.len();
                agent.place = Some(place as u32);
                self.groups.push(r as u32);
                self.sift_up(place);
            }
            Some(place) if head < agent.head => {
                agent.head = head;
                self.sift_up(place as usize);
            }
            Some(_) => {}
        }
    }

    fn peek(&self) -> Option<Key> {
        self.groups.first().map(|&r| self.agents[r as usize].head)
    }

    /// Remove the first message of the first group.
    fn pop(&mut self) {
        let r = self.groups[0];
        let agent = &mut self.agents[r as usize];
        agent.messages.pop().expect("waiting message");
        match agent.first(r) {
            Some(head) => {
                agent.head = head;
                self.sift_down(0);
            }
            None => {
                agent.place = None;
                let last = self.groups.pop().expect("a waiting group");
                if !self.groups.is_empty() {
                    self.groups[0] = last;
                    self.agents[last as usize].place = Some(0);
                    self.sift_down(0);
                }
            }
        }
    }

    /// The first group's agent is still busy: its group waits until `until`.
    fn defer(&mut self, key: Key, until: i64) {
        debug_assert_eq!(self.peek(), Some(key));
        let agent = &mut self.agents[key.recipient() as usize];
        agent.time = until;
        agent.head.time = until;
        self.sift_down(0);
    }

    fn head_at(&self, place: usize) -> Key {
        self.agents[self.groups[place] as usize].head
    }

    fn put(&mut self, place: usize, r: u32) {
        self.groups[place] = r;
        self.agents[r as usize].place = Some(place as u32);
    }

    fn sift_up(&mut self, mut place: usize) {
        let r = self.groups[place];
        let head = self.agents[r as usize].head;
        while place > 0 {
            let parent = (place - 1) / 2;
            if self.head_at(parent) <= head {
                break;
            }
            self.put(place, self.groups[parent]);
            place = parent;
        }
        self.put(place, r);
    }

    fn sift_down(&mut self, mut place: usize) {
        let r = self.groups[place];
        let head = self.agents[r as usize].head;
        loop {
            let left = 2 * place + 1;
            if left >= self.groups.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.groups.len() && self.head_at(right) < self.head_at(left) { right } else { left };
            if head <= self.head_at(child) {
                break;
            }
            self.put(place, self.groups[child]);
            place = child;
        }
        self.put(place, r);
    }
}

/// A 4-ary min-heap of packed keys, with each event's slot in a parallel array. `keys` extends
/// at least `PAD` entries past the `len` in the heap with `u128::MAX`, so every node has four
/// children to compare and choosing the smallest stays branch-free on a partly filled bottom
/// level (on a tie the lower index, a real entry, wins).
struct Heap4 {
    keys: Vec<u128>,
    slots: Vec<u32>,
    len: usize,
}

const PAD: usize = 4;

/// `v[i]`, unchecked in release builds: every index below stays inside the heap's arrays (they
/// extend `PAD` entries past `len`, which covers a node's last child).
#[inline(always)]
fn at<T: Copy>(v: &[T], i: usize) -> T {
    debug_assert!(i < v.len());
    // SAFETY: see above; checked in debug builds.
    unsafe { *v.get_unchecked(i) }
}

#[inline(always)]
fn put<T>(v: &mut [T], i: usize, x: T) {
    debug_assert!(i < v.len());
    // SAFETY: as for `at`.
    unsafe { *v.get_unchecked_mut(i) = x }
}

impl Heap4 {
    fn with_capacity(capacity: usize) -> Self {
        Heap4 { keys: vec![u128::MAX; capacity + PAD], slots: vec![0; capacity + PAD], len: 0 }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn entries(&self) -> impl Iterator<Item = (u128, u32)> + '_ {
        self.keys[..self.len].iter().copied().zip(self.slots[..self.len].iter().copied())
    }

    /// Room for `n` entries, written now (so populated before a run).
    fn reserve(&mut self, n: usize) {
        if self.keys.len() < n + PAD {
            self.keys.resize(n + PAD, u128::MAX);
            self.slots.resize(n + PAD, 0);
        }
    }

    /// The root, `EMPTY` for an empty heap: `keys[0]` is then the padding.
    #[inline(always)]
    fn top(&self) -> (u128, u32) {
        (at(&self.keys, 0), at(&self.slots, 0))
    }

    #[inline]
    fn push(&mut self, key: u128, slot: u32) {
        if self.len + PAD >= self.keys.len() {
            self.reserve(2 * self.len + 64);
        }
        let mut i = self.len;
        self.len += 1;
        while i > 0 {
            let parent = (i - 1) / 4;
            let pk = at(&self.keys, parent);
            if key >= pk {
                break;
            }
            put(&mut self.keys, i, pk);
            { let v = at(&self.slots, parent); put(&mut self.slots, i, v); }
            i = parent;
        }
        put(&mut self.keys, i, key);
        put(&mut self.slots, i, slot);
    }

    /// Index of the smallest child of `i`; there is at least one child.
    #[inline(always)]
    fn min_child(keys: &[u128], i: usize) -> usize {
        let first = 4 * i + 1;
        let k = |j: usize| at(keys, first + j);
        let a = (k(1) < k(0)) as usize;
        let b = 2 + (k(3) < k(2)) as usize;
        first + if k(b) < k(a) { b } else { a }
    }

    /// Put `(key, slot)` into the hole at the root: descend along smallest children to a leaf,
    /// then sift up (few steps, since the moved-in key is usually large).
    #[inline]
    fn fill_root(&mut self, key: u128, slot: u32) {
        let n = self.len;
        let mut i = 0;
        while 4 * i + 1 < n {
            let c = Self::min_child(&self.keys, i);
            { let v = at(&self.keys, c); put(&mut self.keys, i, v); }
            { let v = at(&self.slots, c); put(&mut self.slots, i, v); }
            i = c;
        }
        while i > 0 {
            let parent = (i - 1) / 4;
            let pk = at(&self.keys, parent);
            if key >= pk {
                break;
            }
            put(&mut self.keys, i, pk);
            { let v = at(&self.slots, parent); put(&mut self.slots, i, v); }
            i = parent;
        }
        put(&mut self.keys, i, key);
        put(&mut self.slots, i, slot);
    }

    #[inline]
    fn pop(&mut self) {
        assert!(self.len > 0, "pop from an empty heap");
        self.len -= 1;
        let key = std::mem::replace(&mut self.keys[self.len], u128::MAX);
        let slot = self.slots[self.len];
        if self.len > 0 {
            self.fill_root(key, slot);
        }
    }

    /// Give the root a later key (a requeue) and restore the heap. A requeued event usually moves
    /// only a little later, so this sifts down from the root and stops as soon as the key fits
    /// (keys are unique, so the heap's shape never changes the order events come out in).
    #[inline]
    fn replace_top(&mut self, key: u128) {
        let slot = self.slots[0];
        let n = self.len;
        let mut i = 0;
        while 4 * i + 1 < n {
            let c = Self::min_child(&self.keys, i);
            let ck = at(&self.keys, c);
            if key < ck {
                break;
            }
            put(&mut self.keys, i, ck);
            { let v = at(&self.slots, c); put(&mut self.slots, i, v); }
            i = c;
        }
        put(&mut self.keys, i, key);
        put(&mut self.slots, i, slot);
    }
}

/// The wakeup lane. A trader re-arms its wakeup a fixed interval after the one it handles, and
/// wakeups are handled in key order, so among the agents sharing an interval (a class) each new
/// wakeup follows the class's previous one: a class keeps its wakeups in arrival order, which is
/// key order, and takes each from the front. A wakeup that does not follow its class's last one
/// (the first ones at the open, a requeue, an agent of no class) goes to a heap. The next wakeup
/// is the smallest of the class fronts and the heap's root.
struct Wakeups {
    classes: Vec<VecDeque<Entry>>,
    /// Each agent's class (`NO_CLASS`: none).
    class_of: Vec<u8>,
    heap: Heap4,
    /// The smallest wakeup: key (`EMPTY`: none), slot, and its class or `IN_HEAP`.
    min: (u128, u32, u8),
}

/// An agent in no class; also where a wakeup lies when it is in the heap.
const NO_CLASS: u8 = u8::MAX;
const IN_HEAP: u8 = u8::MAX;

/// The most classes a queue keeps (each one adds a comparison to every wakeup taken).
pub(super) const MAX_WAKEUP_CLASSES: usize = 8;

impl Wakeups {
    fn new(capacity: usize) -> Self {
        Wakeups { classes: Vec::new(), class_of: Vec::new(), heap: Heap4::with_capacity(capacity), min: (EMPTY, 0, IN_HEAP) }
    }

    /// Group agents into classes (`class_of[agent]`, `NO_CLASS` for none); before any wakeup.
    fn set_classes(&mut self, class_of: Vec<u8>) {
        debug_assert!(self.min.0 == EMPTY, "classes are set on an empty lane");
        let n = class_of.iter().filter(|&&c| c != NO_CLASS).map(|&c| c as usize + 1).max().unwrap_or(0);
        assert!(n <= MAX_WAKEUP_CLASSES, "at most {MAX_WAKEUP_CLASSES} wakeup classes");
        self.classes = (0..n)
            .map(|c| VecDeque::with_capacity(class_of.iter().filter(|&&x| x as usize == c).count() + 8))
            .collect();
        self.class_of = class_of;
    }

    /// The next wakeup, `EMPTY` if none.
    #[inline(always)]
    fn top(&self) -> (u128, u32) {
        (self.min.0, self.min.1)
    }

    #[inline]
    fn push(&mut self, key: u128, slot: u32) {
        // A packed key's recipient: the agent the wakeup is for.
        let agent = ((key as u64) >> 32) as u16 as usize;
        let class = self.class_of.get(agent).copied().unwrap_or(NO_CLASS);
        let source = match self.classes.get_mut(class as usize) {
            Some(fifo) if fifo.back().is_none_or(|last| last.key < key) => {
                fifo.push_back(Entry { key, slot });
                class
            }
            _ => {
                self.heap.push(key, slot);
                IN_HEAP
            }
        };
        if key < self.min.0 {
            self.min = (key, slot, source);
        }
    }

    #[inline]
    fn pop(&mut self) {
        let (key, _, source) = self.min;
        assert!(key != EMPTY, "pop from an empty wakeup lane");
        if source == IN_HEAP {
            self.heap.pop();
        } else {
            self.classes[source as usize].pop_front();
        }
        self.min = self.smallest();
    }

    #[inline]
    fn smallest(&self) -> (u128, u32, u8) {
        let (key, slot) = self.heap.top();
        let mut best = (key, slot, IN_HEAP);
        for (c, fifo) in self.classes.iter().enumerate() {
            if let Some(e) = fifo.front() {
                if e.key < best.0 {
                    best = (e.key, e.slot, c as u8);
                }
            }
        }
        best
    }

    /// Give the next wakeup a later key (a requeue).
    #[inline]
    fn replace_top(&mut self, key: u128) {
        let slot = self.min.1;
        self.pop();
        self.push(key, slot);
    }

    fn entries(&self) -> Vec<(u128, u32)> {
        let mut all: Vec<(u128, u32)> = self.heap.entries().collect();
        for fifo in &self.classes {
            all.extend(fifo.iter().map(|e| (e.key, e.slot)));
        }
        all
    }

    fn reserve(&mut self, n: usize) {
        self.heap.reserve(n);
    }
}

/// Packed keys kept in ascending order in `keys[head..]` (slots alongside). With a constant
/// latency, messages arrive in about the order they are sent, so a new key almost always goes at
/// the back and the smallest leaves from the front: both O(1), where a heap pays O(log n) twice.
/// A requeued front key moves only a little later, so it slides forward past the few keys it now
/// follows. Keys are unique, so this yields exactly the heap's order.
#[derive(Default)]
struct Sorted {
    keys: Vec<u128>,
    slots: Vec<u32>,
    head: usize,
}

impl Sorted {
    fn with_capacity(capacity: usize) -> Self {
        Sorted { keys: Vec::with_capacity(capacity), slots: Vec::with_capacity(capacity), head: 0 }
    }

    /// The front, `EMPTY` if none.
    #[inline(always)]
    fn top(&self) -> (u128, u32) {
        let i = self.head;
        match self.keys.get(i) {
            Some(&key) => (key, self.slots[i]),
            None => (EMPTY, 0),
        }
    }

    #[inline]
    fn push(&mut self, key: u128, slot: u32) {
        let mut i = self.keys.len();
        if i == self.head || self.keys[i - 1] < key {
            self.keys.push(key);
            self.slots.push(slot);
            return;
        }
        while i - 1 > self.head && self.keys[i - 2] > key {
            i -= 1;
        }
        self.keys.insert(i - 1, key);
        self.slots.insert(i - 1, slot);
    }

    #[inline]
    fn pop(&mut self) {
        self.head += 1;
        if self.head == self.keys.len() {
            self.keys.clear();
            self.slots.clear();
            self.head = 0;
        } else if self.head >= 4096 && 2 * self.head >= self.keys.len() {
            self.keys.drain(..self.head);
            self.slots.drain(..self.head);
            self.head = 0;
        }
    }

    /// Give the front a later key (a requeue): shift the keys it now follows one place forward.
    #[inline]
    fn replace_top(&mut self, key: u128) {
        let slot = self.slots[self.head];
        let mut i = self.head + 1;
        while i < self.keys.len() && self.keys[i] < key {
            self.keys[i - 1] = self.keys[i];
            self.slots[i - 1] = self.slots[i];
            i += 1;
        }
        self.keys[i - 1] = key;
        self.slots[i - 1] = slot;
    }

    fn entries(&self) -> impl Iterator<Item = (u128, u32)> + '_ {
        self.keys[self.head..].iter().copied().zip(self.slots[self.head..].iter().copied())
    }
}

/// Buckets in a [`Ring`] and entries per bucket.
const RING_BUCKETS: usize = 512;
const RING_CAP: usize = 8;

/// Messages bucketed by delivery time, for bursts: when many agents act at the same instant,
/// a hundred or more messages are in flight at once, due within a few latencies, and a heap pays
/// several levels per pop. Here a message goes into the bucket of its time — a ring of
/// `RING_BUCKETS` buckets `1 << shift` ns wide, starting at the bucket of the current time (see
/// `push`) — and the next one is the smallest key in the first occupied bucket. A message outside
/// the ring's span (or in a full bucket) goes to an overflow heap; the smaller of the two heads is
/// next, so the order is exactly the heap's.
struct Ring {
    shift: u32,
    buckets: Box<[Bucket; RING_BUCKETS]>,
    lens: Box<[u8; RING_BUCKETS]>,
    occupied: [u64; RING_BUCKETS / 64],
    count: usize,
    /// The ring's smallest entry: key (`EMPTY`: none), slot, and where it is
    /// (`bucket * RING_CAP + position`).
    min: (u128, u32, u32),
    overflow: Heap4,
    /// The window that measures how full the popped buckets are (see `retune`): pops left in it
    /// (0: none running), its length, the popped buckets' lengths summed, the windows still to
    /// come after it, and whether it just ended.
    window: u32,
    window_pops: u32,
    window_len: u64,
    windows_left: u8,
    retune_due: bool,
}

/// Pops in the first window after a ring is created (each later one is four times as long), the
/// windows in all, and the mean length of the popped buckets the width aims at, beyond the
/// message popped (see [`Ring::retune`]).
const RETUNE_FIRST: u32 = 16_384;
const RETUNE_WINDOWS: u8 = 2;
const RETUNE_EXCESS: f64 = 0.45;

/// A message in a [`Ring`] bucket: its key and its slot side by side, in 20 bytes, so that the
/// first three entries of a bucket share one cache line (the bucket is line-aligned). Fields are
/// read by value only: a packed field cannot be borrowed.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Entry {
    key: u128,
    slot: u32,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (key, slot) = (self.key, self.slot);
        write!(f, "Entry {{ key: {key}, slot: {slot} }}")
    }
}

/// A [`Ring`] bucket: `RING_CAP` entries (160 bytes) in three cache lines, so that a bucket of
/// up to three messages, the usual case, is one line.
#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
struct Bucket {
    entries: [Entry; RING_CAP],
}

/// What a [`Ring`] bucket holds past its entries: a key no message has.
const EMPTY_ENTRY: Entry = Entry { key: u128::MAX, slot: 0 };
const EMPTY_BUCKET: Bucket = Bucket { entries: [EMPTY_ENTRY; RING_CAP] };

impl Ring {
    fn new(shift: u32) -> Self {
        Ring {
            shift,
            buckets: vec![EMPTY_BUCKET; RING_BUCKETS].into_boxed_slice().try_into().expect("ring size"),
            lens: vec![0; RING_BUCKETS].into_boxed_slice().try_into().expect("ring size"),
            occupied: [0; RING_BUCKETS / 64],
            count: 0,
            min: (EMPTY, 0, 0),
            overflow: Heap4::with_capacity(64),
            window: 0,
            window_pops: 0,
            window_len: 0,
            windows_left: 0,
            retune_due: false,
        }
    }

    /// A ring for the messages of `heap`, `now` being the current time. Buckets are a 32nd of
    /// the median distance ahead of the pending messages (a power of two, at least 1 ns), so a
    /// burst spreads over tens of buckets and the ring spans many typical latencies; the ring
    /// then re-chooses its width from its own pops (see `retune`).
    #[cold]
    #[inline(never)]
    fn from_heap(heap: &Heap4, now: i64) -> Self {
        let mut ahead: Vec<i64> = heap.entries().map(|(k, _)| time_of(k).saturating_sub(now).max(0)).collect();
        ahead.sort_unstable();
        let median = ahead.get(ahead.len() / 2).copied().unwrap_or(0);
        let width = ((median / 32).max(1) as u64).next_power_of_two();
        let mut ring = Ring::new(width.trailing_zeros().min(40));
        for (k, s) in heap.entries() {
            ring.push(k, s, now);
        }
        ring.measure(RETUNE_FIRST, RETUNE_WINDOWS - 1);
        ring
    }

    /// Start a window of `pops` pops that measures how full the popped buckets are, `more`
    /// windows to follow it.
    fn measure(&mut self, pops: u32, more: u8) {
        self.window = pops;
        self.window_pops = pops;
        self.window_len = 0;
        self.windows_left = more;
    }

    /// Re-choose the bucket width from how full the popped buckets were over the window that
    /// just ended (`now` is the current time): the width was set from the messages pending when
    /// the ring was created, which can be an unsettled moment of the run. The width aims at
    /// popped buckets holding `RETUNE_EXCESS` messages beyond the one popped, on average (see
    /// `retuned_shift`), but stays wide enough for the ring to span nine in ten of the pending
    /// messages: narrower buckets for many messages at a time would otherwise send those due
    /// later to the overflow heap. Every message moves to a ring of the new width, whose order
    /// is the keys' order as before.
    #[cold]
    #[inline(never)]
    fn retune(&mut self, now: i64) {
        self.retune_due = false;
        let mut ahead: Vec<i64> = self.entries().iter().map(|&(k, _)| time_of(k).saturating_sub(now).max(0)).collect();
        let tenth = ahead.len() * 9 / 10;
        let far = if ahead.is_empty() { 0 } else { *ahead.select_nth_unstable(tenth).1 };
        let target = retuned_shift(self.shift, self.window_len as f64 / self.window_pops as f64);
        let shift = target.max(span_shift(far)).min(40);
        let (pops, more) = (self.window_pops, self.windows_left);
        if shift != self.shift {
            self.rebuild(shift, now);
        }
        if more > 0 {
            self.measure(pops.saturating_mul(4), more - 1);
        }
    }

    /// Move every message (the ring's and the overflow heap's) to a ring of `1 << shift` ns
    /// buckets, `now` being the current time.
    fn rebuild(&mut self, shift: u32, now: i64) {
        let mut ring = Ring::new(shift);
        for (k, s) in self.entries() {
            ring.push(k, s, now);
        }
        *self = ring;
    }

    /// The smallest message of the ring and the overflow heap together (`EMPTY` if none), and
    /// which of the two holds it.
    #[inline(always)]
    fn top(&self) -> (u128, u32, Pos) {
        let (o, os) = self.overflow.top();
        let (k, s, _) = self.min;
        if o < k { (o, os, Pos::Overflow) } else { (k, s, Pos::Ring) }
    }

    /// Add a message; `now` is the latest time an event was taken at (never decreasing, see
    /// `EventQueue::peek`). The ring's span starts at the bucket of `now`: a message within
    /// `RING_BUCKETS` buckets of it has its own unambiguous bucket, since nothing earlier than
    /// `now` is let in. Every message is normally due at `now` or later, so the span only ever
    /// moves forward and everything in the ring stays inside it.
    #[inline]
    fn push(&mut self, key: u128, slot: u32, now: i64) {
        let bucket = time_of(key) >> self.shift;
        let idx = (bucket as usize) & (RING_BUCKETS - 1);
        let n = self.lens[idx] as usize;
        // Before `now` (a time in the past) wraps to a huge offset, like one beyond the end.
        if bucket.wrapping_sub(now >> self.shift) as u64 >= RING_BUCKETS as u64 || n == RING_CAP {
            crate::marks::count(crate::marks::ctr::RING_OVERFLOW, 1);
            self.overflow.push(key, slot);
            return;
        }
        // `n < RING_CAP` here: the masks only let the compiler see it.
        self.buckets[idx].entries[n & (RING_CAP - 1)] = Entry { key, slot };
        self.lens[idx] = n as u8 + 1;
        self.occupied[idx / 64] |= 1 << (idx % 64);
        self.count += 1;
        if key < self.min.0 {
            self.min = (key, slot, (idx * RING_CAP + n) as u32);
        }
    }

    /// Remove the smallest message (of the ring and the overflow heap together).
    #[inline]
    fn pop(&mut self) {
        // The overflow heap's root is next, or the ring is empty (its smallest key is `EMPTY`).
        if self.overflow.top().0 < self.min.0 {
            self.overflow.pop();
        } else {
            self.pop_min();
        }
    }

    /// Remove the ring's smallest entry (it is not empty).
    #[inline]
    fn pop_min(&mut self) {
        let (_, _, at) = self.min;
        // A bucket and positions in it: the masks only let the compiler see they are in range.
        let (idx, pos) = ((at as usize / RING_CAP) & (RING_BUCKETS - 1), at as usize & (RING_CAP - 1));
        let last = (self.lens[idx] as usize - 1) & (RING_CAP - 1);
        if self.window != 0 {
            // The popped bucket's length, while a window measures them (see `retune`).
            self.window_len += last as u64 + 1;
            self.window -= 1;
            self.retune_due = self.window == 0;
        }
        let entries = &mut self.buckets[idx].entries;
        entries[pos] = entries[last];
        entries[last] = EMPTY_ENTRY;
        self.lens[idx] = last as u8;
        // The bucket's bit goes when it empties (whether it does is a coin toss: no branch).
        self.occupied[idx / 64] &= !(((last == 0) as u64) << (idx % 64));
        self.count -= 1;
        self.min = if self.count == 0 { (EMPTY, 0, 0) } else { self.min_from(idx) };
    }

    /// Give the smallest message (at `pos`, `Ring` or `Overflow`) a later key (a requeue).
    #[inline]
    fn replace_top(&mut self, pos: Pos, key: u128, now: i64) {
        if pos == Pos::Ring {
            let slot = self.min.1;
            self.pop_min();
            self.push(key, slot, now);
        } else {
            let (_, slot) = self.overflow.top();
            self.overflow.pop();
            self.push(key, slot, now);
        }
    }

    /// The smallest entry in the first occupied bucket at or after bucket `from` (in ring order,
    /// which is time order from the start); the ring is not empty.
    fn min_from(&self, from: usize) -> (u128, u32, u32) {
        const WORDS: usize = RING_BUCKETS / 64;
        let mut w = from / 64;
        let mut bits = self.occupied[w] & (!0u64 << (from % 64));
        let mut steps = 0;
        while bits == 0 {
            steps += 1;
            debug_assert!(steps <= WORDS, "no occupied bucket in a nonempty ring");
            w = (w + 1) % WORDS;
            bits = self.occupied[w];
        }
        let idx = (w * 64 + bits.trailing_zeros() as usize) & (RING_BUCKETS - 1);
        crate::marks::count(crate::marks::ctr::RING_MIN_FROM, 1);
        crate::marks::count(crate::marks::ctr::RING_SCAN, self.lens[idx] as u64 + steps as u64);
        let bucket = &self.buckets[idx].entries;
        let len = (self.lens[idx] as usize).min(RING_CAP);
        // Slots past `len` hold `EMPTY_ENTRY`, whose key is larger than any: the first three
        // slots (one cache line) compare without a branch, and a bucket rarely holds more.
        let (k0, k1, k2) = (bucket[0].key, bucket[1].key, bucket[2].key);
        let first = (k1 < k0) as usize;
        let k01 = select_unpredictable(k1 < k0, k1, k0);
        let mut best = select_unpredictable(k2 < k01, 2, first);
        for i in 3..len {
            let (ki, kb) = (bucket[i].key, bucket[best].key);
            if ki < kb {
                best = i;
            }
        }
        let e = bucket[best];
        (e.key, e.slot, (idx * RING_CAP + best) as u32)
    }

    fn entries(&self) -> Vec<(u128, u32)> {
        let mut all: Vec<(u128, u32)> = self.overflow.entries().collect();
        for idx in 0..RING_BUCKETS {
            for &e in &self.buckets[idx].entries[..self.lens[idx] as usize] {
                all.push((e.key, e.slot));
            }
        }
        all
    }
}

/// The new shift of a ring of `1 << shift` ns buckets whose popped buckets held `mean` messages
/// on average: the messages beyond the one popped (at least 0.01) are taken to scale with the
/// width, which is scaled to bring them to `RETUNE_EXCESS` and rounded to a power of two in log
/// space. `RETUNE_EXCESS` measured fastest on the public units (0.3, 0.45, 0.6 and 0.8 tried).
fn retuned_shift(shift: u32, mean: f64) -> u32 {
    let ratio = RETUNE_EXCESS / (mean - 1.0).max(0.01);
    (shift as f64 + ratio.log2()).round().clamp(0.0, 40.0) as u32
}

/// The smallest shift whose ring spans a message `ahead` ns after the current time, whatever the
/// time (see `Ring::push`): the message's bucket is up to `ahead` divided by the width, rounded
/// up, buckets after the current time's.
fn span_shift(ahead: i64) -> u32 {
    let mut shift = 0;
    while (ahead - 1).max(0) >> shift >= RING_BUCKETS as i64 - 1 {
        shift += 1;
    }
    shift
}

/// The message lane: a heap, the ordered form for constant latencies (see [`Sorted`]), or the
/// bucketed form for bursts (see [`Ring`]).
enum Messages {
    Heap(Heap4),
    Sorted(Sorted),
    Ring(Box<Ring>),
}

impl Messages {
    /// The next message (`EMPTY` if none) and where it is.
    #[inline(always)]
    fn top(&self) -> (u128, u32, Pos) {
        match self {
            Messages::Heap(h) => {
                let (k, s) = h.top();
                (k, s, Pos::Heap)
            }
            Messages::Sorted(q) => {
                let (k, s) = q.top();
                (k, s, Pos::Sorted)
            }
            Messages::Ring(r) => r.top(),
        }
    }

    #[inline(always)]
    fn push(&mut self, key: u128, slot: u32, now: i64) {
        match self {
            Messages::Heap(h) => h.push(key, slot),
            Messages::Sorted(s) => s.push(key, slot),
            Messages::Ring(r) => r.push(key, slot, now),
        }
    }

    #[inline(always)]
    fn pop(&mut self) {
        match self {
            Messages::Heap(h) => h.pop(),
            Messages::Sorted(s) => s.pop(),
            Messages::Ring(r) => r.pop(),
        }
    }

    #[inline(always)]
    fn replace_top(&mut self, pos: Pos, key: u128, now: i64) {
        match self {
            Messages::Heap(h) => h.replace_top(key),
            Messages::Sorted(s) => s.replace_top(key),
            Messages::Ring(r) => r.replace_top(pos, key, now),
        }
    }

    fn entries(&self) -> Vec<(u128, u32)> {
        match self {
            Messages::Heap(h) => h.entries().collect(),
            Messages::Sorted(s) => s.entries().collect(),
            Messages::Ring(r) => r.entries(),
        }
    }

    fn prefault(&mut self, n: usize) {
        match self {
            Messages::Heap(h) => h.reserve(n),
            Messages::Sorted(s) => {
                crate::mem::prefault(&mut s.keys, n);
                crate::mem::prefault(&mut s.slots, n);
            }
            Messages::Ring(_) => {}
        }
    }
}

enum Lanes {
    Packed { wakeups: Wakeups, messages: Messages },
    Tuple { wakeups: BinaryHeap<Reverse<Key>>, messages: BinaryHeap<Reverse<Key>> },
}

pub(super) struct EventQueue {
    lanes: Lanes,
    waiting: Waiting,
    /// Time of the event last returned by `peek`: the current time while it is handled.
    now: i64,
    /// Message pops left to observe before choosing between the heap and the ring, and the sum
    /// of the heap's sizes seen at them.
    probe: u32,
    probe_sizes: u64,
}

/// Message pops observed before choosing the message lane's form, and the mean number of pending
/// messages from which the [`Ring`] pays off.
const PROBE_POPS: u32 = 4096;
const RING_MIN_PENDING: u64 = 16;

impl EventQueue {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_ordered_messages(false)
    }

    /// `ordered` keeps messages in the [`Sorted`] form (for constant latencies).
    pub fn with_ordered_messages(ordered: bool) -> Self {
        let messages = if ordered {
            Messages::Sorted(Sorted::with_capacity(1024))
        } else {
            Messages::Heap(Heap4::with_capacity(1024))
        };
        Self {
            lanes: Lanes::Packed { wakeups: Wakeups::new(1024), messages },
            waiting: Waiting::default(),
            now: i64::MIN,
            probe: if ordered { 0 } else { PROBE_POPS },
            probe_sizes: 0,
        }
    }

    /// A queue whose message lane is a [`Ring`] of `1 << shift` ns buckets from the start.
    #[cfg(test)]
    fn with_ring(shift: u32) -> Self {
        let mut queue = Self::with_ordered_messages(false);
        queue.probe = 0;
        if let Lanes::Packed { messages, .. } = &mut queue.lanes {
            *messages = Messages::Ring(Box::new(Ring::new(shift)));
        }
        queue
    }

    /// A queue whose message lane is a [`Ring`] of `1 << shift` ns buckets from the start, which
    /// re-chooses its width after `window` pops and `more` times after that, each window four
    /// times as long as the one before (see `Ring::retune`).
    #[cfg(test)]
    fn with_retuning_ring(shift: u32, window: u32, more: u8) -> Self {
        let mut queue = Self::with_ring(shift);
        queue.ring().measure(window, more);
        queue
    }

    #[cold]
    #[inline(never)]
    fn to_tuple(&mut self) {
        if let Lanes::Packed { wakeups, messages } = &mut self.lanes {
            let w: BinaryHeap<Reverse<Key>> =
                wakeups.entries().into_iter().map(|(k, s)| Reverse(Key::unpack(k, s))).collect();
            let m: BinaryHeap<Reverse<Key>> =
                messages.entries().into_iter().map(|(k, s)| Reverse(Key::unpack(k, s))).collect();
            self.lanes = Lanes::Tuple { wakeups: w, messages: m };
        }
    }

    /// Group the agents' wakeups into classes: `class_of[agent]` for each agent that re-arms its
    /// wakeup a fixed interval after each one, the same class for the same interval, `u8::MAX`
    /// for none (see [`Wakeups`]). At most [`MAX_WAKEUP_CLASSES`] classes; before any wakeup.
    pub fn set_wakeup_classes(&mut self, class_of: Vec<u8>) {
        if let Lanes::Packed { wakeups, .. } = &mut self.lanes {
            wakeups.set_classes(class_of);
        }
    }

    /// Populate room for `n` events in each lane (see [`crate::mem`]).
    pub fn prefault(&mut self, n: usize) {
        if let Lanes::Packed { wakeups, messages } = &mut self.lanes {
            wakeups.reserve(n);
            messages.prefault(n);
        }
    }

    #[inline]
    pub fn push(&mut self, lane: Lane, key: Key) {
        self.push_parts(lane, key.time, key.sender(), key.recipient(), key.msg_id, key.slot);
    }

    /// [`EventQueue::push`] of `Key::new(time, sender, recipient, msg_id, slot)`, packed straight
    /// from the parts.
    #[inline]
    pub fn push_parts(&mut self, lane: Lane, time: i64, sender: u32, recipient: u32, msg_id: u64, slot: u32) {
        crate::marks::count(if lane == Lane::Wakeup { crate::marks::ctr::PUSH_WAKEUP } else { crate::marks::ctr::PUSH_MESSAGE }, 1);
        if let Lanes::Packed { wakeups, messages } = &mut self.lanes {
            if let Some(packed) = pack(time, sender, recipient, msg_id) {
                match lane {
                    Lane::Wakeup => wakeups.push(packed, slot),
                    Lane::Message => messages.push(packed, slot, self.now),
                }
                return;
            }
            self.to_tuple();
        }
        if let Lanes::Tuple { wakeups, messages } = &mut self.lanes {
            let heap = if lane == Lane::Wakeup { wakeups } else { messages };
            heap.push(Reverse(Key::new(time, sender, recipient, msg_id, slot)));
        }
    }

    /// The next event and where it is.
    #[inline]
    pub fn peek(&mut self) -> Option<(Pos, Key)> {
        let raw = self.peek_raw();
        let next = match (raw, self.waiting.peek()) {
            (None, None) => None,
            (Some(raw), None) => Some(raw),
            (None, Some(k)) => Some((Pos::Waiting, k)),
            (Some(raw), Some(k)) => Some(if k < raw.1 { (Pos::Waiting, k) } else { raw }),
        };
        if let Some((_, key)) = next {
            // Never decreasing, even when a message arrives earlier than the current event (a
            // negative latency or delay): the ring anchors its span at this time, and an entry
            // could otherwise fall out of the span (see `Ring::push`).
            self.now = self.now.max(key.time);
        }
        next
    }

    #[inline]
    fn peek_raw(&self) -> Option<(Pos, Key)> {
        match &self.lanes {
            Lanes::Packed { wakeups, messages } => {
                let (w, (mk, ms, mpos)) = (wakeups.top(), messages.top());
                let wakeup = w.0 < mk;
                let (k, s) = select_unpredictable(wakeup, w, (mk, ms));
                if k == EMPTY {
                    return None;
                }
                Some((select_unpredictable(wakeup, Pos::Wakeup, mpos), Key::unpack(k, s)))
            }
            Lanes::Tuple { wakeups, messages } => match (wakeups.peek(), messages.peek()) {
                (None, None) => None,
                (Some(w), None) => Some((Pos::TupleWakeup, w.0)),
                (None, Some(m)) => Some((Pos::TupleMessage, m.0)),
                (Some(w), Some(m)) => {
                    Some(if w.0 < m.0 { (Pos::TupleWakeup, w.0) } else { (Pos::TupleMessage, m.0) })
                }
            },
        }
    }

    /// The packed lanes, which every position but the tuple ones and `Waiting` lies in.
    #[inline(always)]
    fn packed_lanes(&mut self) -> (&mut Wakeups, &mut Messages) {
        match &mut self.lanes {
            Lanes::Packed { wakeups, messages } => (wakeups, messages),
            Lanes::Tuple { .. } => unreachable!("a packed position in tuple lanes"),
        }
    }

    /// The message ring, for a `Ring` or `Overflow` position `peek` gave: the lanes are packed
    /// and the messages a ring until something is pushed or popped (see `pop` of a `Heap`
    /// position and `to_tuple`), and every caller passes a position `peek` gave since.
    #[inline(always)]
    fn ring(&mut self) -> &mut Ring {
        match &mut self.lanes {
            Lanes::Packed { messages: Messages::Ring(ring), .. } => ring,
            _ => {
                debug_assert!(false, "a ring position without a ring");
                // SAFETY: see above.
                unsafe { std::hint::unreachable_unchecked() }
            }
        }
    }

    /// Remove the event `peek` returned (at `pos`).
    #[inline]
    pub fn pop(&mut self, pos: Pos) {
        crate::marks::count(
            match pos {
                Pos::Wakeup | Pos::TupleWakeup => crate::marks::ctr::POP_WAKEUP,
                Pos::Waiting => crate::marks::ctr::POP_WAITING,
                _ => crate::marks::ctr::POP_MESSAGE,
            },
            1,
        );
        match pos {
            Pos::Ring => {
                let ring = self.ring();
                ring.pop_min();
                if ring.retune_due {
                    let now = self.now;
                    self.ring().retune(now);
                }
            }
            Pos::Wakeup => self.packed_lanes().0.pop(),
            Pos::Overflow => self.ring().overflow.pop(),
            Pos::Waiting => self.waiting.pop(),
            Pos::Heap => {
                if self.probe > 0 {
                    self.pop_heap_probing();
                } else if let Messages::Heap(heap) = self.packed_lanes().1 {
                    heap.pop();
                }
            }
            Pos::Sorted => self.packed_lanes().1.pop(),
            Pos::TupleWakeup | Pos::TupleMessage => {
                if let Lanes::Tuple { wakeups, messages } = &mut self.lanes {
                    if pos == Pos::TupleWakeup { wakeups } else { messages }.pop();
                }
            }
        }
    }

    /// Pop the message lane's heap during its first `PROBE_POPS` pops, watching how many
    /// messages it holds: many at a time means bursts, which a ring handles in constant time per
    /// message, and the lane becomes one. (A lane that stays a heap pops in `pop` itself.)
    #[cold]
    #[inline(never)]
    fn pop_heap_probing(&mut self) {
        let now = self.now;
        let (_, messages) = self.packed_lanes();
        let len = if let Messages::Heap(heap) = messages { heap.len() as u64 } else { 0 };
        self.probe_sizes += len;
        self.probe -= 1;
        let probe_done = self.probe == 0 && self.probe_sizes >= RING_MIN_PENDING * PROBE_POPS as u64;
        let (_, messages) = self.packed_lanes();
        if probe_done {
            if let Messages::Heap(heap) = messages {
                *messages = Messages::Ring(Box::new(Ring::from_heap(heap, now)));
            }
        }
        messages.pop();
    }

    /// Requeue the event `peek` returned (`key` at `pos`) at the later `time`, keeping its identity.
    #[inline]
    pub fn delay_first(&mut self, pos: Pos, key: Key, time: i64) {
        debug_assert!(time >= key.time);
        let mut later = key;
        later.time = time;
        if pos == Pos::Waiting {
            self.waiting.pop();
            self.push(Lane::Message, later);
            return;
        }
        let now = self.now;
        match &mut self.lanes {
            Lanes::Packed { wakeups, messages } => {
                let packed = later.packed().expect("same ids as before");
                if pos == Pos::Wakeup {
                    wakeups.replace_top(packed);
                } else {
                    messages.replace_top(pos, packed, now);
                }
            }
            Lanes::Tuple { wakeups, messages } => {
                let heap = if pos == Pos::TupleWakeup { wakeups } else { messages };
                let mut first = heap.peek_mut().expect("nonempty event queue");
                first.0.time = time;
            }
        }
    }

    /// The recipient cannot execute ANY event before `until`. Events already waiting at this
    /// same timestamp can therefore share the update. Do not eagerly move raw events, or groups
    /// at another deadline: their old timestamps still decide whether the kernel reaches stop.
    #[inline]
    pub fn defer_busy(&mut self, pos: Pos, key: Key, until: i64) {
        debug_assert!(until > key.time);
        crate::marks::count(crate::marks::ctr::DEFER, 1);
        if pos == Pos::Waiting {
            crate::marks::count(crate::marks::ctr::DEFER_WAITING, 1);
            self.waiting.defer(key, until);
        } else if self.waiting.can_park(key.recipient(), until) {
            crate::marks::count(crate::marks::ctr::DEFER_PARK, 1);
            self.pop(pos);
            self.waiting.park(key, until);
        } else {
            crate::marks::count(crate::marks::ctr::DEFER_REQUEUE, 1);
            self.delay_first(pos, key, until);
        }
    }

    #[cfg(test)]
    fn is_packed(&self) -> bool {
        matches!(self.lanes, Lanes::Packed { .. })
    }

    #[cfg(test)]
    fn has_ring(&self) -> bool {
        matches!(self.lanes, Lanes::Packed { messages: Messages::Ring(_), .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Mt19937;

    // Independent original tuple key and single heap: the ABIDES order.
    type Original = Reverse<(i64, u32, u32, u64, u32)>;

    /// The message lane's starting form.
    #[derive(Clone, Copy, Debug)]
    enum Form {
        /// A heap, which may turn into a ring on its own.
        Heap,
        Sorted,
        /// A ring of `1 << shift` ns buckets from the start.
        Ring(u32),
        /// The same, re-choosing its width after 3 pops and then 8 more times, each window four
        /// times as long as the one before.
        Retune(u32),
    }

    const FORMS: [Form; 8] = [
        Form::Heap,
        Form::Sorted,
        Form::Ring(0),
        Form::Ring(3),
        Form::Ring(10),
        Form::Ring(24),
        Form::Retune(0),
        Form::Retune(24),
    ];

    fn queue(form: Form) -> EventQueue {
        match form {
            Form::Heap => EventQueue::with_ordered_messages(false),
            Form::Sorted => EventQueue::with_ordered_messages(true),
            Form::Ring(shift) => EventQueue::with_ring(shift),
            Form::Retune(shift) => EventQueue::with_retuning_ring(shift, 3, 8),
        }
    }
    fn original(k: Key) -> Original {
        Reverse((k.time, k.sender(), k.recipient(), k.msg_id, k.slot))
    }

    /// Random pushes, busy requeues and pops against one `BinaryHeap` of original tuples. Ids are
    /// issued as the kernel issues them: a counter (from `first_id`), where a broadcast reuses the
    /// latest id from the same sender for another recipient — so `(sender, recipient, id)` is unique.
    /// Times are mostly just after `now`, sometimes far ahead or even in the past.
    fn differential(seed: u32, max_agent: u32, first_id: u64, spread: i64, form: Form) -> EventQueue {
        differential_with(seed, max_agent, first_id, spread, form, false)
    }

    /// [`differential`], optionally with every agent below 2^16 in one of the most wakeup classes
    /// (or none), so that wakeups go to class queues whenever they follow their class's last one.
    fn differential_with(seed: u32, max_agent: u32, first_id: u64, spread: i64, form: Form, classes: bool) -> EventQueue {
        let mut rng = Mt19937::new(seed);
        let mut queue = queue(form);
        if classes {
            let n = max_agent.min(1 << 16);
            let k = MAX_WAKEUP_CLASSES as u32 + 1;
            queue.set_wakeup_classes((0..n).map(|a| if a % k == 0 { NO_CLASS } else { (a % k - 1) as u8 }).collect());
        }
        let mut reference = BinaryHeap::new();
        let mut now = 1_612_483_200_000_000_000i64;
        let mut slot = 0u32;
        let mut next_id = first_id;
        let mut last: Option<(u32, u64, Vec<u32>)> = None;
        for step in 0..100_000u32 {
            match rng.next_u32() % 7 {
                0..=3 => {
                    let lane = if rng.next_u32() % 4 == 0 { Lane::Wakeup } else { Lane::Message };
                    let mut recipient = rng.next_u32() % max_agent;
                    let broadcast = match &mut last {
                        Some((sender, id, used)) if rng.next_u32() % 4 == 0 && !used.contains(&recipient) => {
                            used.push(recipient);
                            Some((*sender, *id))
                        }
                        _ => None,
                    };
                    let (sender, id) = broadcast.unwrap_or_else(|| {
                        let sender = rng.next_u32() % max_agent;
                        recipient = rng.next_u32() % max_agent;
                        next_id += 1;
                        last = Some((sender, next_id, vec![recipient]));
                        (sender, next_id)
                    });
                    let time = match rng.next_u32() % 16 {
                        0 => now + (rng.next_u64() % (spread as u64 * 1000)) as i64, // far ahead
                        1 => now - (rng.next_u64() % spread as u64) as i64,          // in the past
                        _ => now + (rng.next_u64() % spread as u64) as i64,
                    };
                    let key = Key::new(time, sender, recipient, id, slot);
                    slot += 1;
                    queue.push(lane, key);
                    reference.push(original(key));
                }
                4 if !reference.is_empty() => {
                    let Reverse((time, sender, recipient, id, s)) = reference.pop().unwrap();
                    let later = time + (rng.next_u32() % 13) as i64;
                    let (pos, key) = queue.peek().unwrap();
                    assert_eq!(original(key), Reverse((time, sender, recipient, id, s)));
                    queue.delay_first(pos, key, later);
                    reference.push(Reverse((later, sender, recipient, id, s)));
                }
                _ => {
                    let got = queue.peek().map(|(pos, key)| {
                        queue.pop(pos);
                        original(key)
                    });
                    let want = reference.pop();
                    assert_eq!(got, want, "seed {seed}, step {step}");
                    if let Some(Reverse((t, ..))) = want {
                        now = t;
                    }
                }
            }
            assert_eq!(queue.peek().map(|(_, k)| original(k)), reference.peek().copied());
        }
        while let Some(want) = reference.pop() {
            let (pos, key) = queue.peek().unwrap();
            queue.pop(pos);
            assert_eq!(original(key), want);
        }
        assert!(queue.peek().is_none());
        queue
    }

    /// [`differential`] with the engine's traffic: a wakeup is an agent's own, and every message is
    /// between the exchange (agent 0) and one agent, the exchange's sometimes a broadcast (one id,
    /// several recipients).
    fn differential_exchange(seed: u32, max_agent: u32, first_id: u64, spread: i64, form: Form) -> EventQueue {
        let mut rng = Mt19937::new(seed);
        let mut queue = queue(form);
        let mut reference = BinaryHeap::new();
        let mut now = 1_612_483_200_000_000_000i64;
        let mut slot = 0u32;
        let mut next_id = first_id;
        let mut broadcast: Option<(u64, Vec<u32>)> = None;
        for step in 0..100_000u32 {
            match rng.next_u32() % 7 {
                0..=3 => {
                    let agent = 1 + rng.next_u32() % (max_agent - 1);
                    let (lane, sender, recipient) = match rng.next_u32() % 8 {
                        0 | 1 => (Lane::Wakeup, agent, agent),
                        2..=4 => (Lane::Message, 0, agent),
                        _ => (Lane::Message, agent, 0),
                    };
                    let id = match &mut broadcast {
                        Some((id, used)) if sender == 0 && rng.next_u32() % 4 == 0 && !used.contains(&agent) => {
                            used.push(agent);
                            *id
                        }
                        _ => {
                            next_id += 1;
                            if sender == 0 {
                                broadcast = Some((next_id, vec![agent]));
                            }
                            next_id
                        }
                    };
                    let time = match rng.next_u32() % 16 {
                        0 => now + (rng.next_u64() % (spread as u64 * 1000)) as i64, // far ahead
                        1 => now - (rng.next_u64() % spread as u64) as i64,          // in the past
                        _ => now + (rng.next_u64() % spread as u64) as i64,
                    };
                    let key = Key::new(time, sender, recipient, id, slot);
                    slot += 1;
                    queue.push(lane, key);
                    reference.push(original(key));
                }
                4 if !reference.is_empty() => {
                    let Reverse((time, sender, recipient, id, s)) = reference.pop().unwrap();
                    let later = time + (rng.next_u32() % 13) as i64;
                    let (pos, key) = queue.peek().unwrap();
                    assert_eq!(original(key), Reverse((time, sender, recipient, id, s)));
                    queue.delay_first(pos, key, later);
                    reference.push(Reverse((later, sender, recipient, id, s)));
                }
                _ => {
                    let got = queue.peek().map(|(pos, key)| {
                        queue.pop(pos);
                        original(key)
                    });
                    let want = reference.pop();
                    assert_eq!(got, want, "seed {seed}, step {step}");
                    if let Some(Reverse((t, ..))) = want {
                        now = t;
                    }
                }
            }
            assert_eq!(queue.peek().map(|(_, k)| original(k)), reference.peek().copied());
        }
        while let Some(want) = reference.pop() {
            let (pos, key) = queue.peek().unwrap();
            queue.pop(pos);
            assert_eq!(original(key), want);
        }
        assert!(queue.peek().is_none());
        queue
    }

    #[test]
    fn exchange_traffic_keeps_the_single_queue_order() {
        for seed in [0, 17, u32::MAX] {
            for form in FORMS {
                assert!(differential_exchange(seed, 4, 0, 16, form).is_packed());
                assert!(differential_exchange(seed, 300, 0, 4000, form).is_packed());
                // Agents up to 2^16 - 1 (the largest route) and ids ending just below 2^32.
                assert!(differential_exchange(seed, 1 << 16, 0xFFFF_FFFF - 100_000, 1 << 30, form).is_packed());
            }
            assert!(differential_exchange(seed, 300, 0, 4000, Form::Heap).has_ring());
        }
    }

    /// The ring's width aims at popped buckets of `1 + RETUNE_EXCESS` messages, by powers of two.
    #[test]
    fn retuned_width_aims_at_the_target_bucket_length() {
        let target = 1.0 + RETUNE_EXCESS;
        assert_eq!(retuned_shift(5, target), 5);
        assert_eq!(retuned_shift(5, 1.0 + 2.0 * RETUNE_EXCESS), 4);
        assert_eq!(retuned_shift(5, 1.0 + 4.0 * RETUNE_EXCESS), 3);
        assert_eq!(retuned_shift(5, 1.0 + RETUNE_EXCESS / 2.0), 6);
        // Rounded in log space: 1.4 times the excess keeps the width, 1.42 times halves it.
        assert_eq!(retuned_shift(5, 1.0 + 1.4 * RETUNE_EXCESS), 5);
        assert_eq!(retuned_shift(5, 1.0 + 1.42 * RETUNE_EXCESS), 4);
        // Every popped bucket alone: 45 times as wide (an excess of 0.01), 2^5 after rounding.
        assert_eq!(retuned_shift(5, 1.0), 10);
        // Full buckets: an excess of 7, 16 times narrower.
        assert_eq!(retuned_shift(5, RING_CAP as f64), 1);
        assert_eq!(retuned_shift(1, RING_CAP as f64), 0);
        assert_eq!(retuned_shift(38, 1.0), 40);
    }

    /// A ring of `1 << span_shift(a)` ns buckets spans a message `a` ns ahead at any current
    /// time, and one half as wide does not.
    #[test]
    fn span_shift_is_the_narrowest_width_spanning_a_message() {
        assert_eq!(span_shift(0), 0);
        assert_eq!(span_shift(511), 0);
        assert_eq!(span_shift(512), 1);
        assert_eq!(span_shift(1022), 1);
        assert_eq!(span_shift(1023), 2);
        assert_eq!(span_shift(i64::MAX), 55);
        // Buckets after the current time's bucket, the current time `r` into its own bucket.
        let after = |ahead: i64, shift: u32, r: i64| {
            let now = (1i64 << 60) + r;
            ((now + ahead) >> shift) - (now >> shift)
        };
        for ahead in [0, 1, 2, 510, 511, 512, 1000, 1022, 1023, 4095, 100_000, 1 << 40, 511 << 40, (1 << 50) + 12345] {
            let shift = span_shift(ahead);
            for r in [0, (1 << shift) / 2, (1 << shift) - 1] {
                assert!(after(ahead, shift, r) < RING_BUCKETS as i64, "{ahead} {shift} {r}");
            }
            if shift > 0 {
                assert!(after(ahead, shift - 1, (1 << (shift - 1)) - 1) >= RING_BUCKETS as i64, "{ahead}");
            }
        }
    }

    /// The differential runs re-choose the widths of `Form::Retune` rings on the way: narrower
    /// for messages within 4 us of each other and 2^24 ns buckets, wider for messages within a
    /// second and 1 ns buckets.
    #[test]
    fn retuning_rings_change_width_and_keep_the_single_queue_order() {
        for seed in [0, 17] {
            for (spread, shift) in [(4000, 24), (1 << 30, 0)] {
                let mut queue = differential(seed, 300, 0, spread, Form::Retune(shift));
                let first = queue.ring().shift;
                let mut queue = differential_exchange(seed, 300, 0, spread, Form::Retune(shift));
                let second = queue.ring().shift;
                assert!(
                    if shift == 0 { first > 8 && second > 8 } else { first < 16 && second < 16 },
                    "seed {seed}, spread {spread}: from {shift} to {first} and {second}"
                );
            }
        }
    }

    /// A thousand messages at a time, each due within 200 us: the popped buckets ask for far
    /// narrower buckets than the 512 ns that span nine in ten of the pending messages (their
    /// ring spans 261 us), where the width stops.
    #[test]
    fn retuned_ring_keeps_spanning_most_pending_messages() {
        let mut rng = Mt19937::new(4);
        let mut queue = EventQueue::new();
        let mut reference = BinaryHeap::new();
        let mut id = 0u64;
        let mut push = |now: i64, rng: &mut Mt19937, queue: &mut EventQueue, reference: &mut BinaryHeap<Original>| {
            id += 1;
            let key = Key::new(now + (rng.next_u32() % 200_000) as i64, 0, 1 + rng.next_u32() % 100, id, id as u32);
            queue.push(Lane::Message, key);
            reference.push(original(key));
        };
        for _ in 0..1000 {
            push(1_612_483_200_000_000_000, &mut rng, &mut queue, &mut reference);
        }
        let mut shifts = vec![];
        for pop in 0..300_000 {
            let (pos, key) = queue.peek().unwrap();
            queue.pop(pos);
            assert_eq!(Some(original(key)), reference.pop(), "pop {pop}");
            push(key.time, &mut rng, &mut queue, &mut reference);
            if queue.has_ring() && shifts.last() != Some(&queue.ring().shift) {
                shifts.push(queue.ring().shift);
            }
        }
        let ring = queue.ring();
        assert_eq!((ring.window, ring.windows_left), (0, 0));
        eprintln!("widths {shifts:?}, overflow {}", ring.overflow.len());
        assert_eq!(ring.shift, 9);
    }

    /// Moving the messages to a ring of any width at any moment (1 ns to 2^40 ns buckets, with
    /// messages in buckets, in the overflow heap and in the past) keeps the key order.
    #[test]
    fn ring_rebuilt_at_any_width_keeps_key_order() {
        for seed in [2, 9] {
            let mut rng = Mt19937::new(seed);
            let mut queue = EventQueue::with_ring(3);
            let mut reference = BinaryHeap::new();
            let mut now = 1_612_483_200_000_000_000i64;
            let (mut id, mut rebuilds) = (0u64, 0);
            for step in 0..50_000u32 {
                match rng.next_u32() % 9 {
                    0..=3 => {
                        id += 1;
                        let time = match rng.next_u32() % 8 {
                            0 => now + (rng.next_u64() % (1 << 50)) as i64,
                            1 => now - (rng.next_u32() % 1000) as i64,
                            _ => now + (rng.next_u32() % 5000) as i64,
                        };
                        let key = Key::new(time, rng.next_u32() % 64, rng.next_u32() % 64, id, id as u32);
                        queue.push(Lane::Message, key);
                        reference.push(original(key));
                    }
                    4 => {
                        let shift = [0, 1, 3, 7, 13, 24, 33, 40][rng.next_u32() as usize % 8];
                        let at = queue.now;
                        queue.ring().rebuild(shift, at);
                        rebuilds += 1;
                    }
                    _ => {
                        let got = queue.peek().map(|(pos, key)| {
                            queue.pop(pos);
                            original(key)
                        });
                        let want = reference.pop();
                        assert_eq!(got, want, "seed {seed}, step {step}");
                        if let Some(Reverse((t, ..))) = want {
                            now = t;
                        }
                    }
                }
            }
            while let Some(want) = reference.pop() {
                let (pos, key) = queue.peek().unwrap();
                queue.pop(pos);
                assert_eq!(original(key), want);
            }
            assert!(queue.peek().is_none() && queue.is_packed() && rebuilds > 5000);
        }
    }

    /// A heap that turns into a ring (a steady stream of 64 pending messages, each due within
    /// 200 us) re-chooses the width twice, after 16,384 and 65,536 more pops, towards popped
    /// buckets of `1 + RETUNE_EXCESS` messages.
    #[test]
    fn ring_from_a_heap_retunes_its_width_twice() {
        let mut rng = Mt19937::new(4);
        let mut queue = EventQueue::new();
        let mut reference = BinaryHeap::new();
        let mut now = 1_612_483_200_000_000_000i64;
        let mut id = 0u64;
        let mut push = |now: i64, rng: &mut Mt19937, queue: &mut EventQueue, reference: &mut BinaryHeap<Original>| {
            id += 1;
            let key = Key::new(now + (rng.next_u32() % 200_000) as i64, 0, 1 + rng.next_u32() % 100, id, id as u32);
            queue.push(Lane::Message, key);
            reference.push(original(key));
        };
        for _ in 0..64 {
            push(now, &mut rng, &mut queue, &mut reference);
        }
        let mut shifts = vec![];
        for pop in 0..(PROBE_POPS + RETUNE_FIRST * 5 + 1000) {
            let (pos, key) = queue.peek().unwrap();
            queue.pop(pos);
            assert_eq!(Some(original(key)), reference.pop(), "pop {pop}");
            now = key.time;
            push(now, &mut rng, &mut queue, &mut reference);
            if queue.has_ring() && shifts.last() != Some(&queue.ring().shift) {
                shifts.push(queue.ring().shift);
            }
        }
        let ring = queue.ring();
        assert_eq!((ring.window, ring.windows_left), (0, 0));
        // A 32nd of the median distance ahead (about 60 us) first, then half as wide.
        assert_eq!(shifts, [11, 10]);
    }

    /// Messages in the past or far beyond a ring's span (2^47 ns ahead, with 1 ns and 2^40 ns
    /// buckets) go to its overflow heap and still come out in key order.
    #[test]
    fn ring_messages_far_apart_or_in_the_past_keep_key_order() {
        for shift in [3, 40] {
            let mut queue = EventQueue::with_ring(shift);
            let t0 = 1_612_483_200_000_000_000i64;
            // Anchor the queue's current time at t0.
            queue.push(Lane::Message, Key::new(t0, 0, 1, 1, 0));
            let (pos, _) = queue.peek().unwrap();
            queue.pop(pos);
            let mut reference = BinaryHeap::new();
            let times = [t0 + 5, t0 + (1 << 47) - 1, t0 + (1 << 47), t0 + (1 << 47) + 3, t0 - 1, t0 + 2, t0 + 5, t0];
            for (i, &time) in times.iter().enumerate() {
                let (sender, recipient) = if i % 2 == 0 { (0, 7) } else { (9, 0) };
                let key = Key::new(time, sender, recipient, i as u64 + 2, i as u32 + 1);
                queue.push(Lane::Message, key);
                reference.push(original(key));
            }
            while let Some(want) = reference.pop() {
                let (pos, key) = queue.peek().unwrap();
                queue.pop(pos);
                assert_eq!(original(key), want, "shift {shift}");
            }
            assert!(queue.peek().is_none());
        }
    }

    #[test]
    fn two_heaps_pop_in_single_queue_order() {
        for seed in [0, 17, u32::MAX] {
            // Dense ties; realistic spreads around the wheel size; ids ending just below 2^32.
            for form in FORMS {
                assert!(differential(seed, 4, 0, 16, form).is_packed());
                assert!(differential(seed, 300, 0, 4000, form).is_packed());
                assert!(differential(seed, 300, 0xFFFF_FFFF - 100_000, 1 << 30, form).is_packed());
            }
            // The queue grows through these runs: a heap turns into a ring on the way.
            assert!(differential(seed, 300, 0, 4000, Form::Heap).has_ring());
        }
    }

    /// Agents waking every `interval` of their kind and re-arming at `now + interval`, with
    /// aligned starts (bursts of equal times), busy requeues and message traffic in between: the
    /// wakeups keep the single queue's order.
    #[test]
    fn periodic_wakeups_fill_runs_in_heap_order() {
        periodic_wakeups(false);
    }

    /// The same with the agents' wakeups in classes by interval, as the engine sets them.
    #[test]
    fn periodic_wakeups_in_classes_keep_the_single_queue_order() {
        periodic_wakeups(true);
    }

    #[test]
    fn wakeup_classes_keep_the_single_queue_order() {
        for seed in [0, 17, u32::MAX] {
            for form in [Form::Heap, Form::Ring(3)] {
                assert!(differential_with(seed, 4, 0, 16, form, true).is_packed());
                assert!(differential_with(seed, 300, 0, 4000, form, true).is_packed());
                assert!(differential_with(seed, 300, 0xFFFF_FFFF - 100_000, 1 << 30, form, true).is_packed());
                assert!(!differential_with(seed, 4, 0xFFFF_FFFF - 10_000, 1 << 20, form, true).is_packed());
            }
        }
    }

    fn periodic_wakeups(classes: bool) {
        let mut rng = Mt19937::new(7);
        let intervals = [50_000_000i64, 200_000_000, 500_000_000];
        let kinds = [0, 0, 0, 1, 1, 2];
        let agents: Vec<i64> = (0..150).map(|a| intervals[kinds[a % 6]]).collect();
        let mut queue = EventQueue::new();
        if classes {
            // Agent 0 (the exchange) in none, agent `a + 1` in its interval's class.
            queue.set_wakeup_classes(std::iter::once(NO_CLASS).chain((0..150).map(|a| kinds[a % 6] as u8)).collect());
        }
        let mut reference = BinaryHeap::new();
        let start = 1_612_483_200_000_000_000i64;
        let (mut next_id, mut slot) = (0u64, 0u32);
        for (a, _) in agents.iter().enumerate() {
            next_id += 1;
            let key = Key::new(start, a as u32 + 1, a as u32 + 1, next_id, slot);
            slot += 1;
            queue.push(Lane::Wakeup, key);
            reference.push(original(key));
        }
        let mut pops = 0;
        while pops < 200_000 {
            let (pos, key) = queue.peek().unwrap();
            let want = reference.pop().unwrap();
            assert_eq!(original(key), want, "pop {pops}");
            let now = key.time;
            if rng.next_u32() % 50 == 0 {
                // Busy: the event waits a little.
                let later = now + 1 + (rng.next_u32() % 5000) as i64;
                queue.delay_first(pos, key, later);
                let Reverse((_, s, r, id, sl)) = want;
                reference.push(Reverse((later, s, r, id, sl)));
                continue;
            }
            queue.pop(pos);
            pops += 1;
            if pos == Pos::Wakeup {
                let agent = key.recipient();
                next_id += 1;
                let wake = Key::new(now + agents[agent as usize - 1], agent, agent, next_id, slot);
                slot += 1;
                queue.push(Lane::Wakeup, wake);
                reference.push(original(wake));
                // A query to the exchange (agent 0), due within a few microseconds.
                next_id += 1;
                let msg = Key::new(now + 100 + (rng.next_u32() % 5000) as i64, agent, 0, next_id, slot);
                slot += 1;
                queue.push(Lane::Message, msg);
                reference.push(original(msg));
            }
        }
        assert!(queue.is_packed());
    }

    #[test]
    fn oversized_ids_fall_back_to_tuple_order() {
        for seed in [0, 17] {
            for form in FORMS {
                assert!(!differential(seed, 1 << 17, 0, 16, form).is_packed());
                assert!(!differential(seed, 4, 0xFFFF_FFFF - 10_000, 1 << 20, form).is_packed());
            }
        }
    }

    #[test]
    fn full_width_keys_and_broadcast_ties_keep_tuple_order() {
        // The largest packed id is 2^32 - 2: the next would make the largest key `EMPTY`.
        for (max_id, packed) in [(0xFFFF_FFFE, true), (0xFFFF_FFFF, false)] {
            let mut queue = EventQueue::new();
            let mut reference = BinaryHeap::new();
            let mut i = 0u32;
            for time in [i64::MAX, 0, -1, i64::MIN] {
                for sender in [0xFFFF, 0, 1] {
                    for recipient in [0xFFFF, 0, 1] {
                        for msg_id in [max_id, 0, 1] {
                            let key = Key::new(time, sender, recipient, msg_id, i);
                            queue.push(if i % 2 == 0 { Lane::Wakeup } else { Lane::Message }, key);
                            reference.push(original(key));
                            i += 1;
                        }
                    }
                }
            }
            assert_eq!(queue.is_packed(), packed);
            while let Some(expected) = reference.pop() {
                let (pos, key) = queue.peek().unwrap();
                queue.pop(pos);
                assert_eq!(original(key), expected);
            }
            assert!(queue.peek().is_none());
        }
    }

    fn next_delivered(
        queue: &mut EventQueue, busy: &[i64], now: &mut i64, stop: i64, steps: &mut usize,
    ) -> Option<Original> {
        while *now <= stop {
            let (pos, key) = queue.peek()?;
            *now = key.time;
            *steps += 1;
            if busy[key.recipient() as usize] > *now {
                queue.defer_busy(pos, key, busy[key.recipient() as usize]);
            } else {
                queue.pop(pos);
                return Some(original(key));
            }
        }
        None
    }

    fn next_reference(
        queue: &mut BinaryHeap<Original>, busy: &[i64], now: &mut i64, stop: i64, steps: &mut usize,
    ) -> Option<Original> {
        while *now <= stop {
            let Reverse((t, s, r, id, slot)) = queue.pop()?;
            *now = t;
            *steps += 1;
            if busy[r as usize] > t {
                queue.push(Reverse((busy[r as usize], s, r, id, slot)));
            } else {
                return Some(Reverse((t, s, r, id, slot)));
            }
        }
        None
    }

    #[test]
    fn large_waiting_groups_in_any_arrival_order_keep_the_reference_order() {
        for seed in [3, 8] {
            for form in FORMS {
                let mut rng = Mt19937::new(seed);
                let mut ours = queue(form);
                let mut reference = BinaryHeap::new();
                // Hundreds of messages for one recipient at nearly one time, from many senders and
                // in shuffled id order: its group outgrows the sorted form.
                let mut ids: Vec<u64> = (0..400).collect();
                for i in (1..ids.len()).rev() {
                    ids.swap(i, rng.next_u32() as usize % (i + 1));
                }
                for (slot, &id) in ids.iter().enumerate() {
                    let key = Key::new((rng.next_u32() % 3) as i64, rng.next_u32() % 50, 1, id, slot as u32);
                    ours.push(Lane::Message, key);
                    reference.push(original(key));
                }
                let mut busy = [-1000i64, -1000];
                let (mut now, mut ref_now, mut steps, mut ref_steps) = (-1, -1, 0, 0);
                let mut largest = 0;
                for delivery in 0..1000 {
                    let got = next_delivered(&mut ours, &busy, &mut now, i64::MAX, &mut steps);
                    let want = next_reference(&mut reference, &busy, &mut ref_now, i64::MAX, &mut ref_steps);
                    assert_eq!(got, want, "seed {seed}, {form:?}, delivery {delivery}");
                    largest = largest.max(ours.waiting.agents.get(1).map_or(0, |a| a.messages.len()));
                    let Some(Reverse((time, ..))) = got else { break };
                    busy[1] = time + 1;
                }
                assert!(ours.peek().is_none() && reference.is_empty());
                assert!(largest > SORTED_GROUP, "largest group {largest}");
            }
        }
    }

    #[test]
    fn several_busy_groups_at_once_keep_the_reference_order() {
        let mut most_groups = 0;
        for seed in [1, 5, 99] {
            for form in FORMS {
                let mut rng = Mt19937::new(seed);
                let senders = 8;
                // Three recipients take every message: their groups overlap, park and drain.
                let mut busy = vec![-1000i64; senders as usize];
                let mut ours = queue(form);
                let mut reference = BinaryHeap::new();
                let (mut id, mut slot) = (0u64, 0u32);
                let mut send = |time: i64, rng: &mut Mt19937, ours: &mut EventQueue, reference: &mut BinaryHeap<Original>| {
                    id += 1;
                    let key = Key::new(time, rng.next_u32() % senders, rng.next_u32() % 3, id, slot);
                    slot += 1;
                    ours.push(Lane::Message, key);
                    reference.push(original(key));
                };
                for _ in 0..600 {
                    let time = (rng.next_u32() % 20) as i64;
                    send(time, &mut rng, &mut ours, &mut reference);
                }
                let (mut now, mut ref_now, mut steps, mut ref_steps) = (-1, -1, 0, 0);
                for delivery in 0..5000 {
                    let got = next_delivered(&mut ours, &busy, &mut now, i64::MAX, &mut steps);
                    let want = next_reference(&mut reference, &busy, &mut ref_now, i64::MAX, &mut ref_steps);
                    assert_eq!(got, want, "seed {seed}, {form:?}, delivery {delivery}");
                    most_groups = most_groups.max(ours.waiting.groups.len());
                    let Some(Reverse((time, _, recipient, ..))) = got else { break };
                    busy[recipient as usize] = time + 1 + (rng.next_u32() % 3) as i64;
                    if delivery < 1500 && rng.next_u32() % 2 == 0 {
                        let later = time + (rng.next_u32() % 5) as i64;
                        send(later, &mut rng, &mut ours, &mut reference);
                    }
                }
                assert!(ours.peek().is_none() && reference.is_empty());
            }
        }
        assert!(most_groups >= 3, "the bursts should keep several groups waiting at once");
    }

    #[test]
    fn busy_groups_preserve_deliveries_with_backwards_sends_and_stop_boundaries() {
        for seed in [0, 17, u32::MAX] {
            for stop in [-1, 0, 30, 1200] {
                for (wide, form) in [false, true].into_iter().flat_map(|w| FORMS.map(|f| (w, f))) {
                    let agents = if wide { [0, 1, 3, 65536, 131071] } else { [0, 1, 2, 3, 4] };
                    let mut rng = Mt19937::new(seed);
                    let mut busy = vec![-1000; agents[4] as usize + 1];
                    let mut ours = queue(form);
                    let mut reference = BinaryHeap::new();
                    let mut id = if wide { u32::MAX as u64 - 100 } else { 0 };
                    let mut slot = 0;
                    for _ in 0..200 {
                        id += 1;
                        let key = Key::new((rng.next_u32() % 50) as i64, agents[(id % 5) as usize],
                            agents[(rng.next_u32() % 5) as usize], id, slot);
                        slot += 1;
                        ours.push(Lane::Message, key);
                        reference.push(original(key));
                    }
                    let (mut now, mut ref_now) = (-1, -1);
                    let (mut steps, mut ref_steps) = (0, 0);
                    for delivery in 0..5000 {
                        let got = next_delivered(&mut ours, &busy, &mut now, stop, &mut steps);
                        let want = next_reference(&mut reference, &busy, &mut ref_now, stop, &mut ref_steps);
                        assert_eq!(got, want, "seed {seed}, stop {stop}, wide {wide}, {form:?}, delivery {delivery}");
                        assert_eq!(now, ref_now, "the first event past stop must remain observable");
                        let Some(Reverse((time, _, recipient, ..))) = got else { break };
                        busy[recipient as usize] = time + (rng.next_u32() % 40) as i64 - 3;
                        if delivery < 2000 {
                            // A broadcast can share its id; both negative delays and dense ties occur.
                            id += 1;
                            for &r in agents.iter().take((rng.next_u32() % 4) as usize) {
                                let key = Key::new(time + (rng.next_u32() % 50) as i64 - 10,
                                    recipient, r, id, slot);
                                slot += 1;
                                ours.push(if slot % 3 == 0 { Lane::Wakeup } else { Lane::Message }, key);
                                reference.push(original(key));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn busy_burst_avoids_quadratic_rescheduling() {
        let mut ours = EventQueue::new();
        let mut reference = BinaryHeap::new();
        for id in 0..256 {
            let key = Key::new(0, 0, 1, id, id as u32);
            ours.push(Lane::Message, key);
            reference.push(original(key));
        }
        let (mut now, mut ref_now) = (0, 0);
        let (mut steps, mut ref_steps) = (0, 0);
        let mut busy = [0, 0];
        for _ in 0..256 {
            let got = next_delivered(&mut ours, &busy, &mut now, 1000, &mut steps);
            assert_eq!(got, next_reference(&mut reference, &busy, &mut ref_now, 1000, &mut ref_steps));
            busy[1] = now + 1;
        }
        assert!(steps < 4 * 256, "{steps} scheduler steps");
        assert!(ref_steps > 30_000);
    }
}
