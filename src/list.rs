//! Intrusive circular doubly-linked list (Linux kernel `list_head` style).
//!
//! This is an inherently unsafe data structure — elements are linked via
//! embedded pointers.  The Rust API is `unsafe` throughout; callers must ensure
//! pointer validity and list integrity.  The C API matches `qblist.h` exactly.

use std::ptr;

// ── Core type ─────────────────────────────────────────────────────────────────

/// Embedded list node.  Embed this in your struct and use `container_of!` /
/// `qb_list_entry!` to recover the outer type.
#[repr(C)]
pub struct QbListHead {
    pub next: *mut QbListHead,
    pub prev: *mut QbListHead,
}

// SAFETY: The user is responsible for synchronisation; QbListHead is Send/Sync
// in the same way raw pointers are — deliberately opt-in.
unsafe impl Send for QbListHead {}
unsafe impl Sync for QbListHead {}

impl QbListHead {
    /// Returns a zeroed (uninitialised) node; call `init` before use.
    pub const fn uninit() -> Self {
        QbListHead {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
        }
    }
}

// ── Rust helpers ──────────────────────────────────────────────────────────────

/// Initialise a list head to point at itself (empty list).
///
/// # Safety
/// `head` must be a valid, aligned, non-null pointer.
#[inline]
pub unsafe fn list_init(head: *mut QbListHead) {
    (*head).next = head;
    (*head).prev = head;
}

/// Prepend `element` after `head` (stack / LIFO order).
///
/// # Safety
/// Both pointers must be valid.
#[inline]
pub unsafe fn list_add(element: *mut QbListHead, head: *mut QbListHead) {
    let next = (*head).next;
    (*next).prev = element;
    (*element).next = next;
    (*element).prev = head;
    (*head).next = element;
}

/// Append `element` before `head` (queue / FIFO order).
///
/// # Safety
/// Both pointers must be valid.
#[inline]
pub unsafe fn list_add_tail(element: *mut QbListHead, head: *mut QbListHead) {
    let prev = (*head).prev;
    (*prev).next = element;
    (*element).prev = prev;
    (*element).next = head;
    (*head).prev = element;
}

/// Remove `node` from its list.  The node's pointers are set to null.
///
/// # Safety
/// `node` must be a valid, linked node.
#[inline]
pub unsafe fn list_del(node: *mut QbListHead) {
    let prev = (*node).prev;
    let next = (*node).next;
    (*prev).next = next;
    (*next).prev = prev;
    (*node).next = ptr::null_mut();
    (*node).prev = ptr::null_mut();
}

/// Replace `old_node` with `new_node`.
///
/// # Safety
/// Both pointers must be valid; `old_node` must be in a list.
#[inline]
pub unsafe fn list_replace(old_node: *mut QbListHead, new_node: *mut QbListHead) {
    (*new_node).next = (*old_node).next;
    (*(*new_node).next).prev = new_node;
    (*new_node).prev = (*old_node).prev;
    (*(*new_node).prev).next = new_node;
}

/// Returns `true` if the list is empty (head points to itself).
///
/// # Safety
/// `head` must be a valid, initialised list head.
#[inline]
pub unsafe fn list_empty(head: *const QbListHead) -> bool {
    std::ptr::eq((*head).next, head)
}

/// Count the number of elements in the list.
///
/// # Safety
/// `head` must be a valid, initialised list head.
pub unsafe fn list_length(head: *const QbListHead) -> i32 {
    let mut count = 0i32;
    let mut pos = (*head).next;
    while !std::ptr::eq(pos, head) {
        count += 1;
        pos = (*pos).next;
    }
    count
}

/// Splice `list` onto the front of `head`.
///
/// # Safety
/// Both must be valid, initialised lists.
pub unsafe fn list_splice(list: *mut QbListHead, head: *mut QbListHead) {
    if list_empty(list) {
        return;
    }
    let first = (*list).next;
    let last = (*list).prev;
    let at = (*head).next;
    (*first).prev = head;
    (*head).next = first;
    (*last).next = at;
    (*at).prev = last;
}

/// Splice `list` onto the tail of `head`.
///
/// # Safety
/// Both must be valid, initialised lists.
pub unsafe fn list_splice_tail(list: *mut QbListHead, head: *mut QbListHead) {
    if list_empty(list) {
        return;
    }
    let first = (*list).next;
    let last = (*list).prev;
    let at = (*head).prev;
    (*last).next = head;
    (*head).prev = last;
    (*first).prev = at;
    (*at).next = first;
}

/// Recover the enclosing struct from an embedded `QbListHead` pointer.
///
/// # Safety
/// `ptr` must point to the `member` field of a live `$Type` value.
#[macro_export]
macro_rules! list_entry {
    ($ptr:expr, $Type:ty, $member:ident) => {{
        let offset = std::mem::offset_of!($Type, $member);
        &mut *(($ptr as *mut u8).sub(offset) as *mut $Type)
    }};
}

/// Iterate over list entries.  Do not remove nodes inside the loop body
/// (use `list_for_each_safe` instead).
#[macro_export]
macro_rules! list_for_each {
    ($pos:ident, $head:expr, $body:block) => {{
        let head_ptr = $head as *const $crate::list::QbListHead;
        let mut $pos = (*head_ptr).next;
        while $pos != head_ptr as *mut _ {
            let next = (*$pos).next;
            $body
            $pos = next;
        }
    }};
}

// ── C API ─────────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn qb_list_init(head: *mut QbListHead) {
    list_init(head);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_add(
    element: *mut QbListHead,
    head: *mut QbListHead,
) {
    list_add(element, head);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_add_tail(
    element: *mut QbListHead,
    head: *mut QbListHead,
) {
    list_add_tail(element, head);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_del(node: *mut QbListHead) {
    list_del(node);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_replace(
    old_node: *mut QbListHead,
    new_node: *mut QbListHead,
) {
    list_replace(old_node, new_node);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_empty(head: *const QbListHead) -> i32 {
    list_empty(head) as i32
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_length(head: *const QbListHead) -> i32 {
    list_length(head)
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_is_last(
    node: *const QbListHead,
    head: *const QbListHead,
) -> i32 {
    std::ptr::eq((*node).next, head) as i32
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_splice(
    list: *mut QbListHead,
    head: *mut QbListHead,
) {
    list_splice(list, head);
}

#[no_mangle]
pub unsafe extern "C" fn qb_list_splice_tail(
    list: *mut QbListHead,
    head: *mut QbListHead,
) {
    list_splice_tail(list, head);
}
