//! Utilities: thread locks, high-resolution time, and stopwatches.

use crate::error::{QbError, QbResult};
use parking_lot::Mutex;
use std::ptr;

// ── Thread lock ───────────────────────────────────────────────────────────────

#[repr(u32)]
#[derive(Copy, Clone)]
pub enum QbThreadLockType {
    Short = 0, // spinlock for short critical sections
    Long = 1,  // sleeping mutex for longer sections
}

/// Unified lock type.  Both variants use `parking_lot::Mutex` internally;
/// `parking_lot` already adaptively spins before sleeping, so the distinction
/// is mainly for clarity and compatibility with the C API.
pub struct QbThreadLock {
    inner: Mutex<()>,
}

impl QbThreadLock {
    pub fn new(_lock_type: QbThreadLockType) -> Box<Self> {
        Box::new(QbThreadLock {
            inner: Mutex::new(()),
        })
    }

    pub fn lock(&self) -> QbResult<parking_lot::MutexGuard<'_, ()>> {
        Ok(self.inner.lock())
    }

    pub fn try_lock(&self) -> QbResult<parking_lot::MutexGuard<'_, ()>> {
        self.inner
            .try_lock()
            .ok_or(QbError::new(libc::EBUSY))
    }
}

// ── C API: locks ──────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn qb_thread_lock_create(lock_type: QbThreadLockType) -> *mut QbThreadLock {
    Box::into_raw(QbThreadLock::new(lock_type))
}

/// # Safety
/// `tl` must be a valid pointer from `qb_thread_lock_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_thread_lock(tl: *mut QbThreadLock) -> i32 {
    if tl.is_null() {
        return -libc::EINVAL;
    }
    // Intentionally leak the guard — the caller is expected to call unlock.
    // We use the raw lock API so we can forget the guard.
    use parking_lot::lock_api::RawMutex as _;
    (*tl).inner.raw().lock();
    0
}

/// # Safety
/// `tl` must be a valid pointer from `qb_thread_lock_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_thread_trylock(tl: *mut QbThreadLock) -> i32 {
    if tl.is_null() {
        return -libc::EINVAL;
    }
    use parking_lot::lock_api::RawMutex as _;
    if (*tl).inner.raw().try_lock() {
        0
    } else {
        -libc::EBUSY
    }
}

/// # Safety
/// `tl` must be a valid, locked pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_thread_unlock(tl: *mut QbThreadLock) -> i32 {
    if tl.is_null() {
        return -libc::EINVAL;
    }
    use parking_lot::lock_api::RawMutex as _;
    (*tl).inner.raw().unlock();
    0
}

/// # Safety
/// `tl` must be a valid pointer.  Must not be locked when destroyed.
#[no_mangle]
pub unsafe extern "C" fn qb_thread_lock_destroy(tl: *mut QbThreadLock) -> i32 {
    if tl.is_null() {
        return -libc::EINVAL;
    }
    drop(Box::from_raw(tl));
    0
}

// ── Time functions ────────────────────────────────────────────────────────────

/// Returns a monotonic timestamp in nanoseconds.
pub fn nano_current() -> u64 {
    // We use CLOCK_MONOTONIC via a raw syscall to avoid Instant's opacity.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000u64 + ts.tv_nsec as u64
}

/// Returns the timer resolution in Hz (ticks per second).
pub fn nano_monotonic_hz() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_getres(libc::CLOCK_MONOTONIC, &mut ts);
    }
    if ts.tv_nsec == 0 {
        return 1_000_000_000;
    }
    1_000_000_000u64 / ts.tv_nsec as u64
}

/// Returns a wall-clock timestamp in nanoseconds since the Unix epoch.
pub fn nano_from_epoch() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000u64 + ts.tv_nsec as u64
}

// ── C API: time ───────────────────────────────────────────────────────────────

/// # Safety
/// `ts` must be a valid pointer to a `timespec`.
#[no_mangle]
pub unsafe extern "C" fn qb_timespec_add_ms(ts: *mut libc::timespec, ms: i32) {
    if ts.is_null() {
        return;
    }
    let total_ns = (*ts).tv_sec * 1_000_000_000 + (*ts).tv_nsec
        + ms as i64 * 1_000_000;
    (*ts).tv_sec = (total_ns / 1_000_000_000) as libc::time_t;
    (*ts).tv_nsec = (total_ns % 1_000_000_000) as libc::c_long;
}

#[no_mangle]
pub extern "C" fn qb_util_nano_current_get() -> u64 {
    nano_current()
}

#[no_mangle]
pub extern "C" fn qb_util_nano_monotonic_hz() -> u64 {
    nano_monotonic_hz()
}

#[no_mangle]
pub extern "C" fn qb_util_nano_from_epoch_get() -> u64 {
    nano_from_epoch()
}

/// # Safety
/// `ts` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_timespec_from_epoch_get(ts: *mut libc::timespec) {
    if ts.is_null() {
        return;
    }
    libc::clock_gettime(libc::CLOCK_REALTIME, ts);
}

// ── Stopwatch ─────────────────────────────────────────────────────────────────

pub const QB_UTIL_SW_OVERWRITE: u32 = 0x01;

struct Split {
    abs_ns: u64, // absolute monotonic time of the split
}

/// Multi-split stopwatch.
pub struct QbStopwatch {
    started_at: Option<u64>, // monotonic ns
    stopped_at: Option<u64>,
    splits: std::collections::VecDeque<Split>,
    base_idx: u32,  // logical index of splits[0] (increments on OVERWRITE eviction)
    max_splits: u32,
    options: u32,
}

impl QbStopwatch {
    fn new() -> Box<Self> {
        Box::new(QbStopwatch {
            started_at: None,
            stopped_at: None,
            splits: std::collections::VecDeque::new(),
            base_idx: 0,
            max_splits: 0,
            options: 0,
        })
    }

    fn start(&mut self) {
        self.started_at = Some(nano_current());
        self.stopped_at = None;
        self.splits.clear();
        self.base_idx = 0;
    }

    fn stop(&mut self) {
        self.stopped_at = Some(nano_current());
    }

    fn us_elapsed(&self) -> u64 {
        let start = match self.started_at {
            Some(t) => t,
            None => return 0,
        };
        let end = self.stopped_at.unwrap_or_else(nano_current);
        (end.saturating_sub(start)) / 1000
    }

    fn sec_elapsed(&self) -> f32 {
        self.us_elapsed() as f32 / 1_000_000.0
    }

    fn split_ctl(&mut self, max_splits: u32, options: u32) -> i32 {
        self.max_splits = max_splits;
        self.options = options;
        self.splits = std::collections::VecDeque::with_capacity(max_splits as usize);
        self.base_idx = 0;
        0
    }

    /// Record a split at the current time.  Returns the elapsed µs since the
    /// last split (or since start if this is the first split).
    /// Auto-starts on the first call if `start()` was never called.
    fn split(&mut self) -> u64 {
        let now = nano_current();
        // Auto-start if not yet started.
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        let prev = self
            .splits
            .back()
            .map(|s| s.abs_ns)
            .or(self.started_at)
            .unwrap_or(now);

        if self.max_splits > 0 && self.splits.len() >= self.max_splits as usize {
            if self.options & QB_UTIL_SW_OVERWRITE != 0 {
                // Evict the oldest split; bump base_idx so logical indices stay valid.
                self.splits.pop_front();
                self.base_idx = self.base_idx.wrapping_add(1);
            } else {
                // Buffer full and overwrite disabled: report no split recorded.
                return 0;
            }
        }
        self.splits.push_back(Split { abs_ns: now });
        (now - prev) / 1000
    }

    fn split_last(&self) -> u32 {
        // Return the logical index of the last recorded split.
        self.base_idx + self.splits.len().saturating_sub(1) as u32
    }

    /// Return µs between the split at logical index `recent` and the split at
    /// logical index `older`.  Returns 0 if either index is out of the live window
    /// (i.e. evicted or never recorded).
    fn time_split_get(&self, recent: u32, older: u32) -> u64 {
        let to_phys = |logical: u32| -> Option<usize> {
            // A logical index below base_idx has been evicted.
            logical.checked_sub(self.base_idx).and_then(|p| {
                if (p as usize) < self.splits.len() { Some(p as usize) } else { None }
            })
        };
        let t_older = match to_phys(older).and_then(|p| self.splits.get(p)) {
            Some(s) => s.abs_ns,
            None => return 0,
        };
        let t_recent = match to_phys(recent).and_then(|p| self.splits.get(p)) {
            Some(s) => s.abs_ns,
            None => return 0,
        };
        t_recent.saturating_sub(t_older) / 1000
    }
}

// ── C API: stopwatch ──────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn qb_util_stopwatch_create() -> *mut QbStopwatch {
    Box::into_raw(QbStopwatch::new())
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_free(sw: *mut QbStopwatch) {
    if !sw.is_null() {
        drop(Box::from_raw(sw));
    }
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_start(sw: *mut QbStopwatch) {
    if !sw.is_null() {
        (*sw).start();
    }
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_stop(sw: *mut QbStopwatch) {
    if !sw.is_null() {
        (*sw).stop();
    }
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_us_elapsed_get(sw: *const QbStopwatch) -> u64 {
    if sw.is_null() {
        return 0;
    }
    (*sw).us_elapsed()
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_sec_elapsed_get(sw: *const QbStopwatch) -> f32 {
    if sw.is_null() {
        return 0.0;
    }
    (*sw).sec_elapsed()
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_split_ctl(
    sw: *mut QbStopwatch,
    max_splits: u32,
    options: u32,
) -> i32 {
    if sw.is_null() {
        return -libc::EINVAL;
    }
    (*sw).split_ctl(max_splits, options)
}

/// Record a split and return µs since the previous split.
///
/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_split(sw: *mut QbStopwatch) -> u64 {
    if sw.is_null() {
        return 0;
    }
    (*sw).split()
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_split_last(sw: *const QbStopwatch) -> u32 {
    if sw.is_null() {
        return 0;
    }
    (*sw).split_last()
}

/// # Safety
/// `sw` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_util_stopwatch_time_split_get(
    sw: *const QbStopwatch,
    recent: u32,
    older: u32,
) -> u64 {
    if sw.is_null() {
        return 0;
    }
    (*sw).time_split_get(recent, older)
}

// ── Version ───────────────────────────────────────────────────────────────────

/// Version information.
#[repr(C)]
pub struct QbVersion {
    pub major: u8,
    pub minor: u8,
    pub micro: u8,
}

// SAFETY: QbVersion contains only plain integer fields; it is safe to share
// across threads.
unsafe impl Sync for QbVersion {}

#[no_mangle]
pub static qb_ver: QbVersion = QbVersion { major: 0, minor: 1, micro: 0 };

// A *const c_char static must be wrapped in a newtype to be Sync.
struct SyncPtr(*const libc::c_char);
unsafe impl Sync for SyncPtr {}

static QB_VER_STR_BYTES: &[u8] = b"libqb 0.1.0 (Rust)\0";
static QB_VER_STR_PTR: SyncPtr = SyncPtr(QB_VER_STR_BYTES.as_ptr() as *const libc::c_char);

#[no_mangle]
pub unsafe extern "C" fn qb_ver_str_get() -> *const libc::c_char {
    QB_VER_STR_PTR.0
}

// ── strerror_r ────────────────────────────────────────────────────────────────

/// Thread-safe strerror.
///
/// # Safety
/// `buf` must point to at least `buflen` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn qb_strerror_r(
    errnum: i32,
    buf: *mut libc::c_char,
    buflen: libc::size_t,
) -> *mut libc::c_char {
    if buf.is_null() || buflen == 0 {
        return ptr::null_mut();
    }
    libc::strerror_r(errnum, buf, buflen);
    buf
}
