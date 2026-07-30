//! Segmented stable-pointer dynamic array.
//!
//! Unlike `Vec`, growing the array does **not** move existing elements.
//! Pointers into the array remain valid after a grow.  Maximum 65 536 elements.

use crate::error::{QbError, QbResult};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr;

pub const QB_ARRAY_MAX_ELEMENTS: usize = 65536;
pub const QB_ARRAY_MAX_INDEX_BITS: u32 = 16;

// ── Internal types ────────────────────────────────────────────────────────────

// Bins are raw heap slabs; deallocation is handled in QbArray::drop.

// ── Public type ───────────────────────────────────────────────────────────────

/// Stable-pointer segmented array.  Opaque to C callers.
pub struct QbArray {
    bins: Vec<*mut u8>,       // each bin is a heap allocation
    elem_size: usize,
    elems_per_bin: usize,
    total_capacity: usize,
    autogrow: usize,          // how many elems to add on auto-grow; 0 = no autogrow
    on_new_bin: Option<unsafe extern "C" fn(*mut QbArray, u32)>,
}

// SAFETY: QbArray owns its allocations and synchronisation is the caller's job.
unsafe impl Send for QbArray {}

impl QbArray {
    pub fn new(max_elements: usize, elem_size: usize) -> QbResult<Box<Self>> {
        Self::new_with_autogrow(max_elements, elem_size, 0)
    }

    pub fn new_with_autogrow(
        max_elements: usize,
        elem_size: usize,
        autogrow_elements: usize,
    ) -> QbResult<Box<Self>> {
        if max_elements == 0 || max_elements > QB_ARRAY_MAX_ELEMENTS || elem_size == 0 {
            return Err(QbError::new(libc::EINVAL));
        }
        // Choose a bin size: prefer power-of-two elems_per_bin.
        // Simple heuristic: aim for bins of ~4 KiB.
        let target_bin_bytes = 4096usize;
        let elems_per_bin = (target_bin_bytes / elem_size).next_power_of_two().max(1);
        let elems_per_bin = elems_per_bin.min(max_elements).max(1);

        let mut a = Box::new(QbArray {
            bins: Vec::new(),
            elem_size,
            elems_per_bin,
            total_capacity: 0,
            autogrow: autogrow_elements,
            on_new_bin: None,
        });
        a.ensure_capacity(max_elements)?;
        Ok(a)
    }

    fn ensure_capacity(&mut self, needed: usize) -> QbResult<()> {
        while self.total_capacity < needed {
            let bin_size = self.elems_per_bin * self.elem_size;
            let layout =
                Layout::from_size_align(bin_size, std::mem::align_of::<u64>()).unwrap();
            let ptr = unsafe { alloc_zeroed(layout) };
            if ptr.is_null() {
                return Err(QbError::new(libc::ENOMEM));
            }
            let bin_idx = self.bins.len() as u32;
            self.bins.push(ptr);
            self.total_capacity += self.elems_per_bin;

            if let Some(cb) = self.on_new_bin {
                unsafe { cb(self as *mut Self, bin_idx) };
            }
        }
        Ok(())
    }

    /// Returns a mutable pointer to the element at `idx`.
    pub fn index(&mut self, idx: usize) -> QbResult<*mut u8> {
        if idx >= QB_ARRAY_MAX_ELEMENTS {
            return Err(QbError::new(libc::EINVAL));
        }
        if idx >= self.total_capacity {
            if self.autogrow > 0 {
                let new_cap = (idx + self.autogrow).min(QB_ARRAY_MAX_ELEMENTS);
                self.ensure_capacity(new_cap)?;
            } else {
                return Err(QbError::new(libc::ERANGE));
            }
        }
        let bin = idx / self.elems_per_bin;
        let off = idx % self.elems_per_bin;
        let ptr = unsafe { self.bins[bin].add(off * self.elem_size) };
        Ok(ptr)
    }

    pub fn grow(&mut self, max_elements: usize) -> QbResult<()> {
        if max_elements > QB_ARRAY_MAX_ELEMENTS {
            return Err(QbError::new(libc::EINVAL));
        }
        self.ensure_capacity(max_elements)
    }

    pub fn num_bins(&self) -> usize {
        self.bins.len()
    }

    pub fn elems_per_bin(&self) -> usize {
        self.elems_per_bin
    }
}

impl Drop for QbArray {
    fn drop(&mut self) {
        let bin_size = self.elems_per_bin * self.elem_size;
        let layout =
            Layout::from_size_align(bin_size, std::mem::align_of::<u64>()).unwrap();
        for &bin in &self.bins {
            if !bin.is_null() {
                unsafe { dealloc(bin, layout) };
            }
        }
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// Callback invoked when a new bin is allocated.
pub type QbArrayNewBinCbFn = unsafe extern "C" fn(*mut QbArray, u32);

/// Create a new array.  Returns null on failure (errno set).
#[no_mangle]
pub extern "C" fn qb_array_create(
    max_elements: libc::size_t,
    element_size: libc::size_t,
) -> *mut QbArray {
    match QbArray::new(max_elements, element_size) {
        Ok(b) => Box::into_raw(b),
        Err(e) => {
            unsafe { crate::error::set_errno(e.errno) };
            ptr::null_mut()
        }
    }
}

/// Create a new array with auto-grow support.
#[no_mangle]
pub extern "C" fn qb_array_create_2(
    max_elements: libc::size_t,
    element_size: libc::size_t,
    autogrow_elements: libc::size_t,
) -> *mut QbArray {
    match QbArray::new_with_autogrow(max_elements, element_size, autogrow_elements) {
        Ok(b) => Box::into_raw(b),
        Err(e) => {
            unsafe { crate::error::set_errno(e.errno) };
            ptr::null_mut()
        }
    }
}

/// Get a pointer to element at `idx`.  Returns 0 on success, -errno on failure.
///
/// # Safety
/// `a` must be a valid `QbArray` pointer.  `element_out` must be non-null.
#[no_mangle]
pub unsafe extern "C" fn qb_array_index(
    a: *mut QbArray,
    idx: i32,
    element_out: *mut *mut libc::c_void,
) -> i32 {
    if a.is_null() || element_out.is_null() {
        return -libc::EINVAL;
    }
    if idx < 0 {
        return -libc::ERANGE;
    }
    match (*a).index(idx as usize) {
        Ok(p) => {
            *element_out = p as *mut libc::c_void;
            0
        }
        Err(e) => -(e.errno),
    }
}

/// Grow the array to hold at least `max_elements` elements.
///
/// # Safety
/// `a` must be a valid `QbArray` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_array_grow(
    a: *mut QbArray,
    max_elements: libc::size_t,
) -> i32 {
    if a.is_null() {
        return -libc::EINVAL;
    }
    match (*a).grow(max_elements) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// Returns the number of allocated bins.
///
/// # Safety
/// `a` must be a valid `QbArray` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_array_num_bins_get(a: *const QbArray) -> libc::size_t {
    if a.is_null() {
        return 0;
    }
    (*a).num_bins()
}

/// Returns the number of elements per bin.
///
/// # Safety
/// `a` must be a valid `QbArray` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_array_elems_per_bin_get(a: *const QbArray) -> libc::size_t {
    if a.is_null() {
        return 0;
    }
    (*a).elems_per_bin()
}

/// Register a callback invoked each time a new bin is allocated.
///
/// # Safety
/// `a` must be a valid `QbArray` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_array_new_bin_cb_set(
    a: *mut QbArray,
    cb: Option<unsafe extern "C" fn(a: *mut QbArray, bin_index: u32)>,
) -> i32 {
    if a.is_null() {
        return -libc::EINVAL;
    }
    (*a).on_new_bin = cb;
    0
}

/// Free the array and all its bins.
///
/// # Safety
/// `a` must have been created by `qb_array_create` / `qb_array_create_2` and
/// must not be used after this call.
#[no_mangle]
pub unsafe extern "C" fn qb_array_free(a: *mut QbArray) {
    if !a.is_null() {
        drop(Box::from_raw(a));
    }
}
