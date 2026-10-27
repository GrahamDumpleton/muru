//! Rate limit buckets in shared memory (design specification section
//! 4.15), so the limits hold across every edge process on the node: an
//! edge being replaced and its successor draw from the same buckets.
//!
//! The table is a file the processes map: a header, then a fixed number
//! of slots, each a bucket under a short lock. A bucket is found by the
//! hash of its limit and key, among a few slots from where the hash
//! points; when all of them hold other buckets, the one least recently
//! drawn from gives way, and starts full again when next wanted.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Marks a table of this layout.
const MAGIC: u64 = u64::from_le_bytes(*b"MURUBKT1");

/// Bytes before the first slot.
const HEADER: usize = 64;

/// Bytes a slot takes: its lock, its key, its tokens and when it was last
/// drawn from.
const SLOT: usize = 32;

/// Slots in a table, 2 MiB of them.
pub const SLOTS: usize = 65536;

/// Slots looked at for a bucket.
const PROBES: usize = 8;

/// A table of buckets mapped from a file.
pub struct Table {
    base: *mut u8,
    length: usize,
}

// SAFETY: the mapping lives as long as the table, and every access to it
// goes through atomics, so threads may share it as processes do.

unsafe impl Send for Table {}

// SAFETY: as for Send.

unsafe impl Sync for Table {}

impl std::fmt::Debug for Table {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Table").finish_non_exhaustive()
    }
}

impl Table {
    /// Map the table at `path`, making it when it is not there.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let length = HEADER + SLOTS * SLOT;

        if file.metadata()?.len() < length as u64 {
            file.set_len(length as u64)?;
        }

        // SAFETY: a shared mapping of a file at least `length` long, with
        // no address asked for; the result is checked before use, and the
        // file may be closed once mapped.

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };

        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let table = Self {
            base: base.cast(),
            length,
        };

        // A new file is all zeros: the first process to see it writes the
        // header, the slot count before the mark.

        let slots = table.word(8);
        let _ = slots.compare_exchange(0, SLOTS as u64, Ordering::AcqRel, Ordering::Acquire);
        let magic = table.word(0);
        let _ = magic.compare_exchange(0, MAGIC, Ordering::AcqRel, Ordering::Acquire);

        if magic.load(Ordering::Acquire) != MAGIC || slots.load(Ordering::Acquire) != SLOTS as u64 {
            return Err(io::Error::other(format!(
                "{} holds buckets of another layout",
                path.display()
            )));
        }

        Ok(table)
    }

    /// The 64-bit word at `offset`.
    fn word(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset.is_multiple_of(8) && offset + 8 <= self.length);

        // SAFETY: the offset is within the mapping and 8-byte aligned
        // (the mapping is page aligned), and the word is only ever used
        // as an atomic, by every process.

        unsafe { &*self.base.add(offset).cast::<AtomicU64>() }
    }

    /// The lock of slot `index`.
    fn lock_word(&self, index: usize) -> &AtomicU32 {
        // SAFETY: as for `word`, at the slot's first 4 bytes.

        unsafe { &*self.base.add(HEADER + index * SLOT).cast::<AtomicU32>() }
    }

    fn slot(&self, index: usize) -> Slot<'_> {
        let offset = HEADER + index * SLOT;
        Slot {
            lock: self.lock_word(index),
            key: self.word(offset + 8),
            tokens: self.word(offset + 16),
            at: self.word(offset + 24),
        }
    }

    /// Draw from the bucket of `key`, `now` in microseconds since the
    /// epoch: refill it at `per_second` up to `burst`, then take a token
    /// when `take` says so, given the seconds until one is there (0 when
    /// one is). Returns those seconds.
    pub fn draw(
        &self,
        key: u64,
        per_second: f64,
        burst: f64,
        now: u64,
        take: impl Fn(f64) -> bool,
    ) -> f64 {
        let key = key.max(1);
        let start = (key % SLOTS as u64) as usize;
        let mut victim = start;
        let mut oldest = u64::MAX;

        for probe in 0..PROBES {
            let index = (start + probe) % SLOTS;
            let slot = self.slot(index);
            let _held = slot.hold();
            let found = slot.key.load(Ordering::Relaxed);

            if found == key || found == 0 {
                if found == 0 {
                    slot.fill(key, burst, now);
                }

                return slot.draw(per_second, burst, now, &take);
            }

            let at = slot.at.load(Ordering::Relaxed);

            if at < oldest {
                oldest = at;
                victim = index;
            }
        }

        // Every slot looked at holds another bucket: the one least
        // recently drawn from gives way.

        let slot = self.slot(victim);
        let _held = slot.hold();

        if slot.key.load(Ordering::Relaxed) != key {
            slot.fill(key, burst, now);
        }

        slot.draw(per_second, burst, now, &take)
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `open`, unmapped once.

        unsafe {
            libc::munmap(self.base.cast(), self.length);
        }
    }
}

struct Slot<'a> {
    lock: &'a AtomicU32,
    key: &'a AtomicU64,
    tokens: &'a AtomicU64,
    at: &'a AtomicU64,
}

/// A slot's lock, held until dropped.
struct Held<'a>(&'a AtomicU32);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

impl Slot<'_> {
    /// Take the slot's lock, which holds the holder's process id. A
    /// holder that has gone, killed while it held the lock, loses it.
    fn hold(&self) -> Held<'_> {
        let me = std::process::id();
        let mut spins: u32 = 0;

        loop {
            match self
                .lock
                .compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => return Held(self.lock),
                Err(holder) => {
                    spins = spins.wrapping_add(1);

                    if spins.is_multiple_of(4096) && holder != 0 && holder != me && !alive(holder) {
                        let _ = self.lock.compare_exchange(
                            holder,
                            0,
                            Ordering::AcqRel,
                            Ordering::Relaxed,
                        );
                    }

                    if spins < 64 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    fn fill(&self, key: u64, burst: f64, now: u64) {
        self.key.store(key, Ordering::Relaxed);
        self.tokens.store(burst.to_bits(), Ordering::Relaxed);
        self.at.store(now, Ordering::Relaxed);
    }

    fn draw(&self, per_second: f64, burst: f64, now: u64, take: &impl Fn(f64) -> bool) -> f64 {
        let at = self.at.load(Ordering::Relaxed);
        let elapsed = now.saturating_sub(at) as f64 / 1_000_000.0;
        let mut tokens =
            (f64::from_bits(self.tokens.load(Ordering::Relaxed)) + elapsed * per_second).min(burst);
        let wait = (1.0 - tokens).max(0.0) / per_second;

        if take(wait) {
            tokens -= 1.0;
        }

        self.tokens.store(tokens.to_bits(), Ordering::Relaxed);
        self.at.store(at.max(now), Ordering::Relaxed);
        wait
    }
}

/// Whether a process is still there.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };

    // SAFETY: signal 0 sends nothing; it only asks whether the process
    // exists and may be signalled.

    let answered = unsafe { libc::kill(pid, 0) };
    answered == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The key of a limit's bucket, the same in every process: FNV-1a over
/// the limit's name and the request's key.
pub fn key(limit: &str, key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

    for byte in limit.bytes().chain([0x1e]).chain(key.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }

    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_mappings_draw_from_the_same_buckets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("buckets");
        let first = Table::open(&path).unwrap();
        let second = Table::open(&path).unwrap();
        let bucket = key("crawlers", "203.0.113.0/32");
        let always = |_: f64| true;
        let when_there = |wait: f64| wait == 0.0;

        // A burst of two at one a second: two tokens, then none, across
        // both mappings.

        assert_eq!(first.draw(bucket, 1.0, 2.0, 1_000_000, when_there), 0.0);
        assert_eq!(second.draw(bucket, 1.0, 2.0, 1_000_000, when_there), 0.0);
        let wait = first.draw(bucket, 1.0, 2.0, 1_000_000, when_there);
        assert!((wait - 1.0).abs() < 1e-9, "{wait}");

        // Half a second later half a token has come back.

        let wait = second.draw(bucket, 1.0, 2.0, 1_500_000, when_there);
        assert!((wait - 0.5).abs() < 1e-9, "{wait}");

        // Another key has a bucket of its own.

        assert_eq!(
            second.draw(key("crawlers", "other"), 1.0, 2.0, 1_500_000, always),
            0.0
        );
    }

    #[test]
    fn a_full_neighbourhood_gives_up_its_least_recent_bucket() {
        let directory = tempfile::tempdir().unwrap();
        let table = Table::open(&directory.path().join("buckets")).unwrap();
        let take = |_: f64| true;

        // Keys that all point at the same slot, drawn from in turn.

        let keys: Vec<u64> = (1..=PROBES as u64 + 1)
            .map(|n| n * SLOTS as u64 + 5)
            .collect();

        for (n, bucket) in keys.iter().enumerate() {
            table.draw(*bucket, 1.0, 1.0, 1_000 + n as u64, take);
        }

        // The first key's bucket gave way to the last, so it starts full
        // again; the second's is still empty.

        assert_eq!(
            table.draw(keys[0], 1.0, 1.0, 2_000, |wait| wait == 0.0),
            0.0
        );
        assert!(table.draw(keys[2], 1.0, 1.0, 2_000, |wait| wait == 0.0) > 0.0);
    }

    #[test]
    fn a_table_of_another_layout_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("buckets");
        std::fs::write(&path, b"something else entirely").unwrap();
        assert!(Table::open(&path).is_err());
    }
}
