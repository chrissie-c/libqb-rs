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

/// Handle database.  Declare with `QB_HDB_DECLARE` (or zero-initialize) then
/// call `qb_hdb_create` before use.  The inner pointer is heap-allocated by
/// `qb_hdb_create` and freed by `qb_hdb_destroy`.
#[repr(C)]
pub struct QbHdb {
    /// Heap-allocated inner state; NULL until `qb_hdb_create` is called.
    inner: *mut libc::c_void,
    /// Optional destructor called for each handle instance when freed.
    pub destructor: Option<unsafe extern "C" fn(*mut libc::c_void)>,
}

unsafe impl Send for QbHdb {}

impl QbHdb {
    pub fn new() -> Self {
        let inner = Box::new(QbHdbInner::new(None));
        QbHdb {
            inner: Box::into_raw(inner) as *mut libc::c_void,
            destructor: None,
        }
    }

    pub fn with_destructor(destructor: unsafe extern "C" fn(*mut libc::c_void)) -> Self {
        let inner = Box::new(QbHdbInner::new(Some(destructor)));
        QbHdb {
            inner: Box::into_raw(inner) as *mut libc::c_void,
            destructor: Some(destructor),
        }
    }

    fn inner_mut(&mut self) -> Option<&mut QbHdbInner> {
        if self.inner.is_null() {
            None
        } else {
            Some(unsafe { &mut *(self.inner as *mut QbHdbInner) })
        }
    }

    fn inner_ref(&self) -> Option<&QbHdbInner> {
        if self.inner.is_null() {
            None
        } else {
            Some(unsafe { &*(self.inner as *const QbHdbInner) })
        }
    }
}

impl Default for QbHdb {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for QbHdb {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { drop(Box::from_raw(self.inner as *mut QbHdbInner)) };
            self.inner = ptr::null_mut();
        }
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// Initialize a handle database.  `hdb` must point to a zero-initialized or
/// `QB_HDB_DECLARE`-declared `struct qb_hdb`.
///
/// # Safety
/// `hdb` must be a valid, writable pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_create(hdb: *mut QbHdb) {
    if hdb.is_null() {
        return;
    }
    let destructor = (*hdb).destructor;
    let inner = Box::new(QbHdbInner::new(destructor));
    (*hdb).inner = Box::into_raw(inner) as *mut libc::c_void;
}

/// Destroy the database and free all live handles.
///
/// # Safety
/// `hdb` must be a valid pointer previously initialized with `qb_hdb_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_hdb_destroy(hdb: *mut QbHdb) {
    if hdb.is_null() || (*hdb).inner.is_null() {
        return;
    }
    drop(Box::from_raw((*hdb).inner as *mut QbHdbInner));
    (*hdb).inner = ptr::null_mut();
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
    let inner = match (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_ref() {
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
    if let Some(inner) = (*hdb).inner_mut() {
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
    let inner = match (*hdb).inner_mut() {
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
