//! Phase marks for measuring setup and teardown (feature `marks`; without it every call is
//! empty and compiles away). `FASTSIM_MARKS=1` records the wall clock at each mark,
//! `FASTSIM_MARKS=2` also the process's minor page faults and voluntary context switches; the
//! marks go to stderr as `MARK <name> <realtime ns> <faults> <switches>` when the process exits.
//! `FASTSIM_LAB_EXIT=1` ends the process at its first mark (the cost of starting the binary).
//! `FASTSIM_PROF=<us>` samples the main thread's program counter every `<us>` microseconds of
//! wall time (Linux): `PROF <pc - on_sample> <count>` lines go to stderr at exit (offsets from the
//! handler `fastsim::marks::prof::on_sample`, since the executable is position-independent).
//! `FASTSIM_LAB_PERF=<set>` counts hardware events of the event loop's own thread, from the start
//! to the end of `Sim::run` (Linux, Skylake-family encodings): `HW <name> <count>` at exit. Each
//! set fits the core's four general counters, so nothing is time-shared: 1 = TopDown level 1,
//! 2 = branch misses and stalls, 3 = memory.

#[cfg(feature = "marks")]
mod imp {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU8, Ordering};

    static MODE: AtomicU8 = AtomicU8::new(0);
    static MARKS: Mutex<Vec<(&'static str, i64, i64, i64)>> = Mutex::new(Vec::new());

    pub fn init() {
        let mode = match std::env::var("FASTSIM_MARKS").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            _ => 0,
        };
        MODE.store(mode, Ordering::Relaxed);
        if let Some(us) = std::env::var("FASTSIM_PROF").ok().and_then(|v| v.parse::<i64>().ok()) {
            super::prof::start(us);
        }
        mark("main");
        if std::env::var_os("FASTSIM_LAB_EXIT").is_some() {
            dump();
            std::process::exit(0);
        }
    }

    pub fn mark(name: &'static str) {
        let mode = MODE.load(Ordering::Relaxed);
        if mode == 0 {
            return;
        }
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        let t = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
        let (faults, switches) = if mode == 2 {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            (u.ru_minflt as i64, u.ru_nvcsw as i64)
        } else {
            (0, 0)
        };
        MARKS.lock().expect("marks").push((name, t, faults, switches));
    }

    /// Waits counted by [`blocked`]: `CNT <name> <count> <total us>` at exit.
    pub const WAITS: [&str; 5] = ["raw_send_full", "latency_recv_empty", "ledger_submit", "trace_buffer", "ledger_buffer"];
    static WAIT_N: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];
    static WAIT_NS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

    pub fn blocked<T>(id: usize, f: impl FnOnce() -> T) -> T {
        let t = std::time::Instant::now();
        let r = f();
        WAIT_N[id].fetch_add(1, Ordering::Relaxed);
        WAIT_NS[id].fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    /// Event counters of [`count`]: `CTR <name> <count>` at exit.
    pub const COUNTERS: [&str; 15] = [
        "loop", "defer", "defer_waiting", "defer_park", "defer_requeue", "pop_wakeup", "pop_message",
        "pop_waiting", "push_wakeup", "push_message", "ring_overflow", "ring_scan", "ring_min_from",
        "waiting_stale", "lane_kind",
    ];
    static COUNTS: [std::sync::atomic::AtomicU64; 15] = [const { std::sync::atomic::AtomicU64::new(0) }; 15];

    /// Counts only with the `counters` feature too: an atomic add per event would distort the
    /// timings the other marks take.
    #[inline(always)]
    pub fn count(id: usize, n: u64) {
        #[cfg(feature = "counters")]
        COUNTS[id].fetch_add(n, Ordering::Relaxed);
        #[cfg(not(feature = "counters"))]
        let _ = (id, n);
    }

    pub fn dump() {
        super::prof::dump();
        super::hw::dump();
        // `FASTSIM_LAB_SMAPS=1`: every mapping holding resident pages, as `SMAP <kB mapped> <kB
        // resident> <name>`, to see where a run's memory is.
        if std::env::var_os("FASTSIM_LAB_SMAPS").is_some() {
            if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
                for t in tasks.flatten() {
                    let stat = std::fs::read_to_string(t.path().join("stat")).unwrap_or_default();
                    let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
                    let state = stat.rsplit(')').next().and_then(|r| r.split_whitespace().next()).unwrap_or("?").to_string();
                    let wchan = std::fs::read_to_string(t.path().join("wchan")).unwrap_or_default();
                    eprintln!("TASK {} {} {} {}", t.file_name().to_string_lossy(), comm.trim(), state, wchan.trim());
                }
            }
            if let Ok(text) = std::fs::read_to_string("/proc/self/smaps") {
                let (mut size, mut name) = (0u64, String::new());
                for line in text.lines() {
                    let mut f = line.split_whitespace();
                    match f.next() {
                        Some(k) if k.contains('-') && !k.ends_with(':') => {
                            let (a, b) = k.split_once('-').unwrap_or(("0", "0"));
                            size = (u64::from_str_radix(b, 16).unwrap_or(0) - u64::from_str_radix(a, 16).unwrap_or(0)) / 1024;
                            name = f.nth(4).unwrap_or("[anon]").to_string();
                        }
                        Some("Rss:") => {
                            let rss: u64 = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                            if rss > 0 {
                                eprintln!("SMAP {size} {rss} {name}");
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        for (i, name) in COUNTERS.iter().enumerate() {
            let n = COUNTS[i].load(Ordering::Relaxed);
            if n > 0 {
                eprintln!("CTR {name} {n}");
            }
        }
        for (i, name) in WAITS.iter().enumerate() {
            let n = WAIT_N[i].load(Ordering::Relaxed);
            if n > 0 {
                eprintln!("CNT {name} {n} {}", WAIT_NS[i].load(Ordering::Relaxed) / 1000);
            }
        }
        if MODE.load(Ordering::Relaxed) == 0 {
            return;
        }
        for (name, t, faults, switches) in MARKS.lock().expect("marks").iter() {
            eprintln!("MARK {name} {t} {faults} {switches}");
        }
    }
}

/// The sampling profiler of `FASTSIM_PROF` (main thread only).
#[cfg(feature = "marks")]
mod prof {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const CAP: usize = 1 << 20;
    static SAMPLES: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];
    /// The word at the top of the stack at each sample: the caller's return address while a leaf
    /// function such as `memcpy` runs (`FASTSIM_PROF_RET=1` prints these pairs).
    static TOPS: [AtomicU64; CAP] = [const { AtomicU64::new(0) }; CAP];
    static COUNT: AtomicUsize = AtomicUsize::new(0);

    #[cfg(target_os = "linux")]
    extern "C" fn on_sample(_sig: libc::c_int, _info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        let uc = ctx as *const libc::ucontext_t;
        #[cfg(target_arch = "x86_64")]
        let (pc, top) = unsafe {
            let sp = (*uc).uc_mcontext.gregs[libc::REG_RSP as usize] as *const u64;
            ((*uc).uc_mcontext.gregs[libc::REG_RIP as usize] as u64, sp.read())
        };
        #[cfg(target_arch = "aarch64")]
        let (pc, top) = unsafe { ((*uc).uc_mcontext.pc, (*uc).uc_mcontext.regs[30]) };
        let i = COUNT.fetch_add(1, Ordering::Relaxed);
        if i < CAP {
            SAMPLES[i].store(pc, Ordering::Relaxed);
            TOPS[i].store(top, Ordering::Relaxed);
        }
    }

    #[cfg(target_os = "linux")]
    pub fn start(us: i64) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sample as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigaction(libc::SIGPROF, &sa, std::ptr::null_mut());
            let mut sev: libc::sigevent = std::mem::zeroed();
            sev.sigev_notify = libc::SIGEV_THREAD_ID;
            sev.sigev_signo = libc::SIGPROF;
            sev.sigev_notify_thread_id = libc::gettid();
            let mut timer: libc::timer_t = std::mem::zeroed();
            if libc::timer_create(libc::CLOCK_MONOTONIC, &mut sev, &mut timer) != 0 {
                eprintln!("FASTSIM_PROF: timer_create failed");
                return;
            }
            let period = libc::timespec { tv_sec: us / 1_000_000, tv_nsec: (us % 1_000_000) * 1000 };
            let spec = libc::itimerspec { it_interval: period, it_value: period };
            libc::timer_settime(timer, 0, &spec, std::ptr::null_mut());
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn start(_us: i64) {}

    pub fn dump() {
        let n = COUNT.load(Ordering::Relaxed).min(CAP);
        if n == 0 {
            return;
        }
        #[cfg(target_os = "linux")]
        let anchor = on_sample as usize as i64;
        #[cfg(not(target_os = "linux"))]
        let anchor = 0i64;
        if std::env::var_os("FASTSIM_PROF_RET").is_some() {
            let mut pairs: Vec<(u64, u64)> =
                (0..n).map(|i| (SAMPLES[i].load(Ordering::Relaxed), TOPS[i].load(Ordering::Relaxed))).collect();
            pairs.sort_unstable();
            let mut i = 0;
            while i < pairs.len() {
                let mut j = i;
                while j < pairs.len() && pairs[j] == pairs[i] {
                    j += 1;
                }
                let (pc, top) = pairs[i];
                eprintln!("PROFR {} {} {}", pc as i64 - anchor, top as i64 - anchor, j - i);
                i = j;
            }
        }
        let mut pcs: Vec<u64> = (0..n).map(|i| SAMPLES[i].load(Ordering::Relaxed)).collect();
        pcs.sort_unstable();
        let mut i = 0;
        while i < pcs.len() {
            let mut j = i;
            while j < pcs.len() && pcs[j] == pcs[i] {
                j += 1;
            }
            eprintln!("PROF {} {}", pcs[i] as i64 - anchor, j - i);
            i = j;
        }
    }
}

/// Hardware counters of the calling thread around the event loop (see `FASTSIM_LAB_PERF`).
#[cfg(all(feature = "marks", target_os = "linux"))]
mod hw {
    use std::sync::Mutex;

    /// (name, perf type: 0 hardware / 4 raw, config).
    const SETS: [&[(&str, u32, u64)]; 3] = [
        &[("cycles", 0, 0), ("instructions", 0, 1), ("uops_issued", 4, 0x010e), ("uops_retired_slots", 4, 0x02c2),
          ("fe_undelivered", 4, 0x019c), ("recovery_cycles", 4, 0x010d)],
        &[("cycles", 0, 0), ("instructions", 0, 1), ("branch_misses", 0, 5), ("stalls_total", 4, 0x0400_04a3),
          ("stalls_mem_any", 4, 0x1400_14a3), ("store_buffer_full", 4, 0x08a2)],
        &[("cycles", 0, 0), ("instructions", 0, 1), ("l1_miss_loads", 4, 0x08d1), ("l2_miss_loads", 4, 0x10d1),
          ("store_forward_blocks", 4, 0x0203), ("stalls_l1d_miss", 4, 0x0c00_0ca3)],
    ];
    static OPEN: Mutex<Vec<(&'static str, i32)>> = Mutex::new(Vec::new());
    static READ: Mutex<Vec<(&'static str, f64)>> = Mutex::new(Vec::new());

    pub fn start() {
        let Some(set) = std::env::var("FASTSIM_LAB_PERF").ok().and_then(|v| v.parse::<usize>().ok()) else { return };
        let Some(events) = SETS.get(set.wrapping_sub(1)) else { return };
        let mut open = OPEN.lock().expect("hw");
        for &(name, ty, config) in events.iter() {
            // struct perf_event_attr (128 bytes): type, size, config; read_format = total time
            // enabled | running; flags = disabled | exclude_kernel | exclude_hv; this thread only.
            let mut attr = [0u64; 16];
            attr[0] = ty as u64 | (128u64 << 32);
            attr[1] = config;
            attr[4] = 1 | 2;
            attr[5] = 1 | (1 << 5) | (1 << 6);
            let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) } as i32;
            if fd < 0 {
                eprintln!("HW {name}: perf_event_open failed");
                continue;
            }
            open.push((name, fd));
        }
        for &(_, fd) in open.iter() {
            unsafe {
                libc::ioctl(fd, 0x2403, 0); // PERF_EVENT_IOC_RESET
                libc::ioctl(fd, 0x2400, 0); // PERF_EVENT_IOC_ENABLE
            }
        }
    }

    pub fn stop() {
        let open = OPEN.lock().expect("hw");
        for &(_, fd) in open.iter() {
            unsafe { libc::ioctl(fd, 0x2401, 0) }; // PERF_EVENT_IOC_DISABLE
        }
        let mut read = READ.lock().expect("hw");
        for &(name, fd) in open.iter() {
            let mut v = [0u64; 3];
            let n = unsafe { libc::read(fd, v.as_mut_ptr().cast(), 24) };
            if n == 24 && v[2] > 0 {
                read.push((name, v[0] as f64 * v[1] as f64 / v[2] as f64));
            }
        }
    }

    pub fn dump() {
        for (name, value) in READ.lock().expect("hw").iter() {
            eprintln!("HW {name} {value:.0}");
        }
    }
}

#[cfg(not(all(feature = "marks", target_os = "linux")))]
mod hw {
    #[inline(always)]
    pub fn start() {}
    #[inline(always)]
    pub fn stop() {}
    #[inline(always)]
    #[allow(dead_code)]
    pub fn dump() {}
}

pub use hw::{start as hw_start, stop as hw_stop};

#[cfg(not(feature = "marks"))]
mod imp {
    #[inline(always)]
    pub fn init() {}
    #[inline(always)]
    pub fn mark(_name: &'static str) {}
    #[inline(always)]
    pub fn dump() {}
    #[inline(always)]
    pub fn blocked<T>(_id: usize, f: impl FnOnce() -> T) -> T {
        f()
    }
    #[inline(always)]
    pub fn count(_id: usize, _n: u64) {}
}

pub use imp::{blocked, count, dump, init, mark};

/// Ids for [`count`] (see `COUNTERS`).
pub mod ctr {
    pub const LOOP: usize = 0;
    pub const DEFER: usize = 1;
    pub const DEFER_WAITING: usize = 2;
    pub const DEFER_PARK: usize = 3;
    pub const DEFER_REQUEUE: usize = 4;
    pub const POP_WAKEUP: usize = 5;
    pub const POP_MESSAGE: usize = 6;
    pub const POP_WAITING: usize = 7;
    pub const PUSH_WAKEUP: usize = 8;
    pub const PUSH_MESSAGE: usize = 9;
    pub const RING_OVERFLOW: usize = 10;
    pub const RING_SCAN: usize = 11;
    pub const RING_MIN_FROM: usize = 12;
    pub const WAITING_STALE: usize = 13;
    pub const LANE_KIND: usize = 14;
}

/// Ids for [`blocked`] (see `WAITS`).
pub const RAW_SEND_FULL: usize = 0;
pub const LATENCY_RECV_EMPTY: usize = 1;
pub const LEDGER_SUBMIT: usize = 2;
pub const TRACE_BUFFER: usize = 3;
pub const LEDGER_BUFFER: usize = 4;
