//! Memory prepared before a run. A fresh process meets every page it writes for the first time
//! with a page fault, which costs about a microsecond inside a virtual machine. Buffers the event
//! loop is expected to fill are therefore populated beforehand, in one call where the kernel
//! supports it (`MADV_POPULATE_WRITE`, Linux 5.14+), otherwise by touching each page.

const PAGE: usize = 4096;

/// Reserve room for `extra` more elements in `v` and populate the pages behind them, so that
/// writing them later does not fault. The contents and length of `v` are unchanged.
pub fn prefault<T>(v: &mut Vec<T>, extra: usize) {
    let size = std::mem::size_of::<T>();
    if size == 0 || extra == 0 {
        return;
    }
    v.reserve(extra);
    // SAFETY: `reserve` guarantees `extra` elements of spare capacity after `len`.
    let start = unsafe { v.as_mut_ptr().add(v.len()) }.cast::<u8>();
    populate(start, extra * size);
}

/// A `Vec` with room for `rows` elements, all of it populated.
pub fn prefaulted<T>(rows: usize) -> Vec<T> {
    let mut v = Vec::new();
    prefault(&mut v, rows);
    v
}

/// [`prefaulted`] for a helper thread working beside the main one: page by page. One
/// `MADV_POPULATE_WRITE` call holds the address space's lock for the whole range, and a thread
/// being created meanwhile (which needs that lock exclusively) then makes every other user of
/// the lock wait behind it, the main thread included; single page faults hardly take it.
pub fn prefaulted_beside<T>(rows: usize) -> Vec<T> {
    prefaulted_beside_with_capacity(rows, rows)
}

/// [`prefaulted_beside`] with room for `capacity` elements, of which the first `rows` are
/// populated.
pub fn prefaulted_beside_with_capacity<T>(rows: usize, capacity: usize) -> Vec<T> {
    let mut v: Vec<T> = Vec::with_capacity(capacity.max(rows));
    let size = std::mem::size_of::<T>() * rows;
    if size > 0 {
        touch(v.as_mut_ptr().cast::<u8>(), size);
    }
    v
}

/// [`prefaulted`] with room for `capacity` elements, of which the first `rows` are populated.
pub fn prefaulted_with_capacity<T>(rows: usize, capacity: usize) -> Vec<T> {
    let mut v = Vec::with_capacity(capacity.max(rows));
    prefault(&mut v, rows);
    v
}

fn populate(start: *mut u8, len: usize) {
    #[cfg(target_os = "linux")]
    {
        const MADV_POPULATE_WRITE: libc::c_int = 23;
        let begin = start as usize & !(PAGE - 1);
        let end = (start as usize + len).next_multiple_of(PAGE);
        // Faults the pages in writable without accessing them; the range stays inside the
        // mappings that hold the allocation, whose pages are all ours.
        if unsafe { libc::madvise(begin as *mut libc::c_void, end - begin, MADV_POPULATE_WRITE) } == 0 {
            return;
        }
    }
    touch(start, len);
}

/// One write per page of `len` bytes of spare capacity at `start`, which nothing reads before it
/// is initialized.
fn touch(start: *mut u8, len: usize) {
    let mut offset = 0;
    while offset < len {
        unsafe { start.add(offset).write_volatile(0) };
        offset += PAGE;
    }
    unsafe { start.add(len - 1).write_volatile(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefault_keeps_contents_and_reserves_capacity() {
        let mut v: Vec<u64> = (0..10).collect();
        prefault(&mut v, 100_000);
        assert!(v.capacity() >= 100_010);
        assert_eq!(v, (0..10).collect::<Vec<_>>());
        v.extend(10..100_010);
        assert_eq!(v.len(), 100_010);
        assert_eq!(v[100_009], 100_009);
        let empty: Vec<u8> = prefaulted(0);
        assert_eq!(empty.capacity(), 0);
        let big: Vec<[u8; 40]> = prefaulted(8193);
        assert!(big.capacity() >= 8193 && big.is_empty());
    }
}
