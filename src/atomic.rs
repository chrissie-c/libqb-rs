//! Atomic integer and pointer operations.
//!
//! Wraps `std::sync::atomic` with the same API as libqb's `qbatomic.h`.
//! All operations use `SeqCst` ordering (full memory barrier), matching the C library.

use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

const SEQ: Ordering = Ordering::SeqCst;

// ── Rust API ──────────────────────────────────────────────────────────────────

/// Atomically exchange `*atomic` with `*atomic + val`, returning the old value.
#[inline]
pub fn atomic_int_exchange_and_add(atomic: &AtomicI32, val: i32) -> i32 {
    atomic.fetch_add(val, SEQ)
}

#[inline]
pub fn atomic_int_add(atomic: &AtomicI32, val: i32) {
    atomic.fetch_add(val, SEQ);
}

/// Compare-and-swap. Returns the old value. On success old == `old_val`.
#[inline]
pub fn atomic_int_compare_and_exchange(atomic: &AtomicI32, old_val: i32, new_val: i32) -> i32 {
    match atomic.compare_exchange(old_val, new_val, SEQ, SEQ) {
        Ok(v) | Err(v) => v,
    }
}

#[inline]
pub fn atomic_int_get(atomic: &AtomicI32) -> i32 {
    atomic.load(SEQ)
}

#[inline]
pub fn atomic_int_set(atomic: &AtomicI32, val: i32) {
    atomic.store(val, SEQ);
}

#[inline]
pub fn atomic_int_inc(atomic: &AtomicI32) {
    atomic.fetch_add(1, SEQ);
}

/// Returns `true` if the result after decrement is zero.
#[inline]
pub fn atomic_int_dec_and_test(atomic: &AtomicI32) -> bool {
    atomic.fetch_sub(1, SEQ) == 1
}

#[inline]
pub fn atomic_pointer_compare_and_exchange<T>(
    atomic: &AtomicPtr<T>,
    old_val: *mut T,
    new_val: *mut T,
) -> bool {
    atomic
        .compare_exchange(old_val, new_val, SEQ, SEQ)
        .is_ok()
}

#[inline]
pub fn atomic_pointer_get<T>(atomic: &AtomicPtr<T>) -> *mut T {
    atomic.load(SEQ)
}

#[inline]
pub fn atomic_pointer_set<T>(atomic: &AtomicPtr<T>, val: *mut T) {
    atomic.store(val, SEQ);
}

// ── C API ─────────────────────────────────────────────────────────────────────
//
// The C API operates on raw `volatile int32_t *` and `volatile void **`.
// We cast them to the appropriate atomic type; this is valid because:
//   - `AtomicI32` has the same representation as `i32`
//   - `AtomicPtr<T>` has the same representation as `*mut T`
//   - The caller is responsible for proper alignment and validity

/// No-op in Rust; atomics do not require explicit initialisation.
#[no_mangle]
pub extern "C" fn qb_atomic_init() {}

/// Atomically add `val` to `*atomic`, returning the previous value.
///
/// # Safety
/// `atomic` must be a valid, aligned pointer to an `i32`/`AtomicI32`.
#[no_mangle]
pub unsafe extern "C" fn qb_atomic_int_exchange_and_add(
    atomic: *mut i32,
    val: i32,
) -> i32 {
    (*(atomic as *const AtomicI32)).fetch_add(val, SEQ)
}

#[no_mangle]
pub unsafe extern "C" fn qb_atomic_int_add(atomic: *mut i32, val: i32) {
    (*(atomic as *const AtomicI32)).fetch_add(val, SEQ);
}

/// Returns the old value. Writes `new_val` only when current == `old_val`.
#[no_mangle]
pub unsafe extern "C" fn qb_atomic_int_compare_and_exchange(
    atomic: *mut i32,
    old_val: i32,
    new_val: i32,
) -> i32 {
    match (*(atomic as *const AtomicI32)).compare_exchange(old_val, new_val, SEQ, SEQ) {
        Ok(v) | Err(v) => v,
    }
}

#[no_mangle]
pub unsafe extern "C" fn qb_atomic_int_get(atomic: *const i32) -> i32 {
    (*(atomic as *const AtomicI32)).load(SEQ)
}

#[no_mangle]
pub unsafe extern "C" fn qb_atomic_int_set(atomic: *mut i32, new_val: i32) {
    (*(atomic as *const AtomicI32)).store(new_val, SEQ);
}

#[no_mangle]
pub unsafe extern "C" fn qb_atomic_pointer_get(atomic: *mut *mut libc::c_void) -> *mut libc::c_void {
    (*(atomic as *const AtomicPtr<libc::c_void>)).load(SEQ)
}

#[no_mangle]
pub unsafe extern "C" fn qb_atomic_pointer_set(
    atomic: *mut *mut libc::c_void,
    new_val: *mut libc::c_void,
) {
    (*(atomic as *const AtomicPtr<libc::c_void>)).store(new_val, SEQ);
}

/// Returns non-zero (true) if the exchange succeeded.
#[no_mangle]
pub unsafe extern "C" fn qb_atomic_pointer_compare_and_exchange(
    atomic: *mut *mut libc::c_void,
    old_val: *mut libc::c_void,
    new_val: *mut libc::c_void,
) -> i32 {
    let a = &*(atomic as *const AtomicPtr<libc::c_void>);
    match a.compare_exchange(old_val, new_val, SEQ, SEQ) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}
