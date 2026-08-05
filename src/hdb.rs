//! Handle database: reference-counted opaque object registry.
//!
//! Maps 64-bit handles to heap-allocated instances.  The upper 32 bits are a
//! generation counter (epoch) that catches use-after-free.  Destruction is
//! deferred: the destructor fires when the refcount reaches zero.

use crate::error::{QbError, QbResult};
use std::ptr;

// ── Handle format ─────────────────────────────────────────────────────────────
//
// handle: u64 = (check: u32) << 32 | (index: u32)

pub type QbHandle = u64;

#[inline]
fn make_handle(check: u32, index: u32) -> QbHandle {
    ((check as u64) << 32) | (index as u64)
}

#[inline]
fn handle_index(h: QbHandle) -> u32 {
    h as u32
}

#[inline]
fn handle_check(h: QbHandle) -> u32 {
    (h >> 32) as u32
}

// ── Slot ──────────────────────────────────────────────────────────────────────

enum SlotState {
    Free,
    Active,
    PendingDestroy,
}

struct Slot {
    state: SlotState,
    check: u32,
    ref_count: i32,
    instance: *mut u8,
    instance_size: usize,
}

unsafe impl Send for Slot {}

impl Slot {
    fn empty() -> Self {
        Slot {
            state: SlotState::Free,
            check: 0,
            ref_count: 0,
            instance: ptr::null_mut(),
            instance_size: 0,
        }
    }
}

// ── Inner database (heap-allocated, not repr(C)) ───────────────────────────────

struct QbHdbInner {
    slots: Vec<Slot>,
    iterator: usize,
    destructor: Option<unsafe extern "C" fn(*mut libc::c_void)>,
}

impl QbHdbInner {
    fn new(destructor: Option<unsafe extern "C" fn(*mut libc::c_void)>) -> Self {
        QbHdbInner {
            slots: Vec::new(),
            iterator: 0,
            destructor,
        }
    }

    fn handle_create(&mut self, instance_size: usize) -> QbResult<QbHandle> {
        let idx = self
            .slots
            .iter()
            .position(|s| matches!(s.state, SlotState::Free));

        let idx = match idx {
            Some(i) => i,
            None => {
                let i = self.slots.len();
                if i >= u32::MAX as usize {
                    return Err(QbError::new(libc::ENOMEM));
                }
                self.slots.push(Slot::empty());
                i
            }
        };

        let instance = if instance_size > 0 {
            let layout =
                std::alloc::Layout::from_size_align(instance_size, std::mem::align_of::<u64>())
                    .map_err(|_| QbError::new(libc::EINVAL))?;
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            if ptr.is_null() {
                return Err(QbError::new(libc::ENOMEM));
            }
            ptr
        } else {
            ptr::null_mut()
        };

        let slot = &mut self.slots[idx];
        slot.state = SlotState::Active;
        slot.ref_count = 1;
        slot.instance = instance;
        slot.instance_size = instance_size;
        slot.check = slot.check.wrapping_add(1);

        Ok(make_handle(slot.check, idx as u32))
    }

    fn resolve_active(&self, handle: QbHandle) -> QbResult<usize> {
        let idx = handle_index(handle) as usize;
        let check = handle_check(handle);
        match self.slots.get(idx) {
            Some(s) if s.check == check && matches!(s.state, SlotState::Active) => Ok(idx),
            _ => Err(QbError::new(libc::EBADF)),
        }
    }

    fn resolve_any_live(&self, handle: QbHandle) -> QbResult<usize> {
        let idx = handle_index(handle) as usize;
        let check = handle_check(handle);
        match self.slots.get(idx) {
            Some(s)
                if s.check == check
                    && (matches!(s.state, SlotState::Active)
                        || matches!(s.state, SlotState::PendingDestroy)) =>
            {
                Ok(idx)
            }
            _ => Err(QbError::new(libc::EBADF)),
        }
    }

    fn handle_get(&mut self, handle: QbHandle) -> QbResult<*mut libc::c_void> {
        let idx = self.resolve_active(handle)?;
        self.slots[idx].ref_count += 1;
        Ok(self.slots[idx].instance as *mut libc::c_void)
    }

    fn handle_get_always(&mut self, handle: QbHandle) -> QbResult<*mut libc::c_void> {
        let idx = self.resolve_any_live(handle)?;
        self.slots[idx].ref_count += 1;
        Ok(self.slots[idx].instance as *mut libc::c_void)
    }

    fn handle_put(&mut self, handle: QbHandle) -> QbResult<()> {
        let idx = handle_index(handle) as usize;
        let check = handle_check(handle);
        let slot = self.slots.get_mut(idx).ok_or(QbError::new(libc::EBADF))?;
        if slot.check != check {
            return Err(QbError::new(libc::EBADF));
        }
        slot.ref_count -= 1;
        if slot.ref_count <= 0 && matches!(slot.state, SlotState::PendingDestroy) {
            self.free_slot(idx);
        }
        Ok(())
    }

    fn handle_destroy(&mut self, handle: QbHandle) -> QbResult<()> {
        let idx = self.resolve_active(handle)?;
        self.slots[idx].state = SlotState::PendingDestroy;
        self.slots[idx].ref_count -= 1;
        if self.slots[idx].ref_count <= 0 {
            self.free_slot(idx);
        }
        Ok(())
    }

    fn free_slot(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        if let Some(dtor) = self.destructor {
            if !slot.instance.is_null() {
                unsafe { dtor(slot.instance as *mut libc::c_void) };
            }
        }
        if !slot.instance.is_null() && slot.instance_size > 0 {
            let layout = std::alloc::Layout::from_size_align(
                slot.instance_size,
                std::mem::align_of::<u64>(),
            )
            .unwrap();
            unsafe { std::alloc::dealloc(slot.instance, layout) };
        }
        slot.instance = ptr::null_mut();
        slot.instance_size = 0;
        slot.ref_count = 0;
        slot.state = SlotState::Free;
    }

    fn handle_refcount_get(&self, handle: QbHandle) -> QbResult<i32> {
        let idx = self.resolve_active(handle)?;
        Ok(self.slots[idx].ref_count)
    }

    fn iterator_reset(&mut self) {
        self.iterator = 0;
    }

    fn iterator_next(&mut self) -> Option<(QbHandle, *mut libc::c_void)> {
        while self.iterator < self.slots.len() {
            let idx = self.iterator;
            self.iterator += 1;
            let slot = &self.slots[idx];
            if matches!(slot.state, SlotState::Active) {
                let handle = make_handle(slot.check, idx as u32);
                return Some((handle, slot.instance as *mut libc::c_void));
            }
        }
        None
    }
}

impl Drop for QbHdbInner {
    fn drop(&mut self) {
        for idx in 0..self.slots.len() {
            if !matches!(self.slots[idx].state, SlotState::Free) {
                self.free_slot(idx);
            }
        }
    }
}

// ── Public handle type (repr(C), caller-allocatable) ──────────────────────────
//
// Must match C libqb's `struct qb_hdb` exactly (x86_64 layout):
//   uint32_t handle_count;      offset  0
//   <padding>                   offset  4
//   qb_array_t *handles;        offset  8   ← we use this slot for our inner ptr
//   uint32_t iterator;          offset 16
//   <padding>                   offset 20
//   void (*destructor)(void *); offset 24
//   uint32_t first_run;         offset 32   ← QB_TRUE (1) when using QB_HDB_DECLARE
//   <padding>                   offset 36
//   sizeof = 40
//
// QB_HDB_DECLARE sets first_run=QB_TRUE=1.  C libqb's qb_hdb_handle_create then
// calls qb_hdb_create on first use.  We replicate that lazy-init here so that
// databases declared with QB_HDB_DECLARE work without an explicit qb_hdb_create.
//
// ALIGNMENT NOTE: C code may pass pointers to `struct qb_hdb` that are only
// 4-byte aligned (e.g. embedded fields in packed-ish structs like cmap_conn_info).
// All field reads/writes use byte-offset + read_unaligned/write_unaligned to
// avoid Rust's alignment checks while still accessing the correct memory.

#[repr(C)]
pub struct QbHdb {
    _handle_count: u32,
    _pad0: u32,
    _inner: *mut libc::c_void,      // offset 8  — accessed only via helpers below
    _iterator_c: u32,
    _pad1: u32,
    _destructor: usize,             // offset 24 — fn ptr stored as usize for repr
    _first_run: u32,                // offset 32
    _pad2: u32,
}

unsafe impl Send for QbHdb {}

// Field byte offsets (verified against C with gcc on x86_64).
const OFF_INNER: usize = 8;
const OFF_DESTRUCTOR: usize = 24;
const OFF_FIRST_RUN: usize = 32;

type DestructorFn = unsafe extern "C" fn(*mut libc::c_void);

#[inline]
unsafe fn hdb_read_inner(hdb: *const QbHdb) -> *mut libc::c_void {
    (hdb as *const u8).add(OFF_INNER).cast::<*mut libc::c_void>().read_unaligned()
}
#[inline]
unsafe fn hdb_write_inner(hdb: *mut QbHdb, val: *mut libc::c_void) {
    (hdb as *mut u8).add(OFF_INNER).cast::<*mut libc::c_void>().write_unaligned(val);
}
#[inline]
unsafe fn hdb_read_destructor(hdb: *const QbHdb) -> Option<DestructorFn> {
    (hdb as *const u8).add(OFF_DESTRUCTOR).cast::<Option<DestructorFn>>().read_unaligned()
}
#[inline]
unsafe fn hdb_read_first_run(hdb: *const QbHdb) -> u32 {
    (hdb as *const u8).add(OFF_FIRST_RUN).cast::<u32>().read_unaligned()
}
#[inline]
unsafe fn hdb_write_first_run(hdb: *mut QbHdb, val: u32) {
    (hdb as *mut u8).add(OFF_FIRST_RUN).cast::<u32>().write_unaligned(val);
}

/// Ensure the database is initialised (handles both explicit `qb_hdb_create`
/// and the lazy path triggered by `QB_HDB_DECLARE`).
/// Returns a mutable reference to the inner state, or None if `hdb` is null.
unsafe fn hdb_get_inner(hdb: *mut QbHdb) -> Option<&'static mut QbHdbInner> {
    if hdb.is_null() {
        return None;
    }
    if hdb_read_first_run(hdb) != 0 || hdb_read_inner(hdb).is_null() {
        // Lazy initialisation: triggered by QB_HDB_DECLARE (first_run=1)
        // or a zero-initialised struct that was never passed to qb_hdb_create.
        let destructor = hdb_read_destructor(hdb);
        let inner = Box::new(QbHdbInner::new(destructor));
        hdb_write_inner(hdb, Box::into_raw(inner) as *mut libc::c_void);
        hdb_write_first_run(hdb, 0);
    }
    Some(&mut *(hdb_read_inner(hdb) as *mut QbHdbInner))
}

/// Read-only variant — does NOT trigger lazy init (returns None when not ready).
unsafe fn hdb_get_inner_ref(hdb: *const QbHdb) -> Option<&'static QbHdbInner> {
    if hdb.is_null() || hdb_read_inner(hdb).is_null() || hdb_read_first_run(hdb) != 0 {
        return None;
    }
    Some(&*(hdb_read_inner(hdb) as *const QbHdbInner))
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// Initialize a handle database.  `hdb` must point to a zero-initialized or
/// `QB_HDB_DECLARE`-declared `struct qb_hdb`.
///
/// # Safety
/// `hdb` must be a valid, writable pointer (alignment as low as 4 is accepted).
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_create(hdb: *mut QbHdb) {
    if hdb.is_null() {
        return;
    }
    // Free any previously initialised state.
    let prev = hdb_read_inner(hdb);
    if !prev.is_null() {
        drop(Box::from_raw(prev as *mut QbHdbInner));
    }
    let destructor = hdb_read_destructor(hdb);
    let inner = Box::new(QbHdbInner::new(destructor));
    hdb_write_inner(hdb, Box::into_raw(inner) as *mut libc::c_void);
    hdb_write_first_run(hdb, 0);
}

/// Destroy the database and free all live handles.
///
/// # Safety
/// `hdb` must be a valid pointer previously initialized with `qb_hdb_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_destroy(hdb: *mut QbHdb) {
    if hdb.is_null() {
        return;
    }
    let inner = hdb_read_inner(hdb);
    if inner.is_null() {
        return;
    }
    drop(Box::from_raw(inner as *mut QbHdbInner));
    hdb_write_inner(hdb, ptr::null_mut());
    hdb_write_first_run(hdb, 1); // mark as needing re-init if reused
}

/// Allocate a new handle with `instance_size` bytes of zeroed backing storage.
///
/// # Safety
/// `hdb` and `handle_out` must be valid non-null pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_create(
    hdb: *mut QbHdb,
    instance_size: i32,
    handle_out: *mut QbHandle,
) -> i32 {
    if hdb.is_null() || handle_out.is_null() || instance_size < 0 {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_create(instance_size as usize) {
        Ok(h) => {
            *handle_out = h;
            0
        }
        Err(e) => -(e.errno),
    }
}

/// Increment refcount and return the instance pointer (active handles only).
///
/// # Safety
/// `hdb` and `instance_out` must be valid non-null pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_get(
    hdb: *mut QbHdb,
    handle: QbHandle,
    instance_out: *mut *mut libc::c_void,
) -> i32 {
    if hdb.is_null() || instance_out.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_get(handle) {
        Ok(p) => {
            *instance_out = p;
            0
        }
        Err(e) => -(e.errno),
    }
}

/// Like `qb_hdb_handle_get` but also succeeds on pending-destroy handles.
///
/// # Safety
/// `hdb` and `instance_out` must be valid non-null pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_get_always(
    hdb: *mut QbHdb,
    handle: QbHandle,
    instance_out: *mut *mut libc::c_void,
) -> i32 {
    if hdb.is_null() || instance_out.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_get_always(handle) {
        Ok(p) => {
            *instance_out = p;
            0
        }
        Err(e) => -(e.errno),
    }
}

/// Decrement refcount.  May trigger destruction if pending.
///
/// # Safety
/// `hdb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_put(hdb: *mut QbHdb, handle: QbHandle) -> i32 {
    if hdb.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_put(handle) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// Mark the handle for deferred destruction.
///
/// # Safety
/// `hdb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_destroy(hdb: *mut QbHdb, handle: QbHandle) -> i32 {
    if hdb.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_destroy(handle) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// Returns the current refcount for a handle.
///
/// # Safety
/// `hdb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_handle_refcount_get(
    hdb: *const QbHdb,
    handle: QbHandle,
) -> i32 {
    if hdb.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner_ref(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.handle_refcount_get(handle) {
        Ok(v) => v,
        Err(e) => -(e.errno),
    }
}

/// Reset the iterator to the beginning.
///
/// # Safety
/// `hdb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_iterator_reset(hdb: *mut QbHdb) {
    if hdb.is_null() {
        return;
    }
    if let Some(inner) = hdb_get_inner(hdb) {
        inner.iterator_reset();
    }
}

/// Advance the iterator.  Returns 0 and writes to `instance_out`/`handle_out`
/// when an entry is found; returns -ENOENT when exhausted.
///
/// # Safety
/// All pointers must be valid.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_iterator_next(
    hdb: *mut QbHdb,
    instance_out: *mut *mut libc::c_void,
    handle_out: *mut QbHandle,
) -> i32 {
    if hdb.is_null() {
        return -libc::EINVAL;
    }
    let inner = match hdb_get_inner(hdb) {
        Some(i) => i,
        None => return -libc::EINVAL,
    };
    match inner.iterator_next() {
        Some((h, p)) => {
            if !instance_out.is_null() {
                *instance_out = p;
            }
            if !handle_out.is_null() {
                *handle_out = h;
            }
            0
        }
        None => -libc::ENOENT,
    }
}

#[no_mangle]
pub extern "C" fn qb_hdb_base_convert(handle: QbHandle) -> u32 {
    handle_index(handle)
}

#[no_mangle]
pub extern "C" fn qb_hdb_nocheck_convert(index: u32) -> QbHandle {
    index as u64
}
