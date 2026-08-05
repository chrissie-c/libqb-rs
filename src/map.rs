//! Unified string-keyed map with three backends: hashtable, skiplist, trie.
//!
//! All backends expose the same `QbMap` interface.  Keys are `*const c_char`
//! (null-terminated C strings).  Values are `*mut c_void` (user data).

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::ptr;

// ── Notification flags ────────────────────────────────────────────────────────

#[repr(u32)]
#[derive(Copy, Clone)]
pub enum QbMapNotifyFlags {
    Deleted = 1,
    Replaced = 2,
    Inserted = 4,
    Recursive = 8,
    Free = 16,
}

// ── Callback types ────────────────────────────────────────────────────────────

pub type QbMapNotifyFn = unsafe extern "C" fn(
    event: u32,
    key: *mut libc::c_char,
    old_value: *mut libc::c_void,
    value: *mut libc::c_void,
    user_data: *mut libc::c_void,
);

pub type QbMapTransverseFn = unsafe extern "C" fn(
    key: *const libc::c_char,
    value: *mut libc::c_void,
    user_data: *mut libc::c_void,
) -> i32;

// ── Notify subscription ───────────────────────────────────────────────────────

struct NotifyEntry {
    key: Option<CString>, // None = wildcard
    cb: QbMapNotifyFn,
    events: u32,
    user_data: *mut libc::c_void,
}

unsafe impl Send for NotifyEntry {}

// ── Internal trait ────────────────────────────────────────────────────────────

trait MapBackend {
    fn put(&mut self, key: &CStr, value: *mut libc::c_void) -> Option<*mut libc::c_void>;
    fn get(&self, key: &CStr) -> Option<*mut libc::c_void>;
    fn remove(&mut self, key: &CStr) -> Option<*mut libc::c_void>;
    fn count(&self) -> usize;
    fn foreach(&self, cb: QbMapTransverseFn, user_data: *mut libc::c_void);
    fn iter_create(&self) -> Box<dyn MapIterBackend + Send>;
    fn iter_create_prefix(&self, prefix: &CStr) -> Box<dyn MapIterBackend + Send>;
}

trait MapIterBackend {
    fn next(&mut self) -> Option<(CString, *mut libc::c_void)>;
}

// ── HashMap backend ───────────────────────────────────────────────────────────

struct HashBackend {
    inner: HashMap<CString, *mut libc::c_void>,
}

unsafe impl Send for HashBackend {}

impl HashBackend {
    fn new(capacity: usize) -> Self {
        HashBackend {
            inner: HashMap::with_capacity(capacity),
        }
    }
}

impl MapBackend for HashBackend {
    fn put(&mut self, key: &CStr, value: *mut libc::c_void) -> Option<*mut libc::c_void> {
        self.inner.insert(key.to_owned(), value)
    }

    fn get(&self, key: &CStr) -> Option<*mut libc::c_void> {
        self.inner.get(key).copied()
    }

    fn remove(&mut self, key: &CStr) -> Option<*mut libc::c_void> {
        self.inner.remove(key)
    }

    fn count(&self) -> usize {
        self.inner.len()
    }

    fn foreach(&self, cb: QbMapTransverseFn, user_data: *mut libc::c_void) {
        for (k, &v) in &self.inner {
            let ret = unsafe { cb(k.as_ptr(), v, user_data) };
            if ret != 0 {
                break;
            }
        }
    }

    fn iter_create(&self) -> Box<dyn MapIterBackend + Send> {
        let items: Vec<(CString, *mut libc::c_void)> = self
            .inner
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        Box::new(VecIter { items, pos: 0 })
    }

    fn iter_create_prefix(&self, _prefix: &CStr) -> Box<dyn MapIterBackend + Send> {
        // hashtable does not support prefix iteration
        Box::new(VecIter { items: Vec::new(), pos: 0 })
    }
}

// ── BTreeMap backend (skiplist semantics: sorted, ordered iteration) ───────────

struct SkiplistBackend {
    inner: BTreeMap<CString, *mut libc::c_void>,
}

unsafe impl Send for SkiplistBackend {}

impl SkiplistBackend {
    fn new() -> Self {
        SkiplistBackend {
            inner: BTreeMap::new(),
        }
    }
}

impl MapBackend for SkiplistBackend {
    fn put(&mut self, key: &CStr, value: *mut libc::c_void) -> Option<*mut libc::c_void> {
        self.inner.insert(key.to_owned(), value)
    }

    fn get(&self, key: &CStr) -> Option<*mut libc::c_void> {
        self.inner.get(key).copied()
    }

    fn remove(&mut self, key: &CStr) -> Option<*mut libc::c_void> {
        self.inner.remove(key)
    }

    fn count(&self) -> usize {
        self.inner.len()
    }

    fn foreach(&self, cb: QbMapTransverseFn, user_data: *mut libc::c_void) {
        for (k, &v) in &self.inner {
            let ret = unsafe { cb(k.as_ptr(), v, user_data) };
            if ret != 0 {
                break;
            }
        }
    }

    fn iter_create(&self) -> Box<dyn MapIterBackend + Send> {
        Box::new(SkiplistLiveIter {
            map: &self.inner,
            current_key: None,
        })
    }

    fn iter_create_prefix(&self, prefix: &CStr) -> Box<dyn MapIterBackend + Send> {
        let prefix_str = prefix.to_string_lossy();
        let items: Vec<(CString, *mut libc::c_void)> = self
            .inner
            .range(prefix.to_owned()..)
            .take_while(|(k, _)| k.to_string_lossy().starts_with(prefix_str.as_ref()))
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        Box::new(VecIter { items, pos: 0 })
    }
}

// ── Radix trie backend ─────────────────────────────────────────────────────────

/// Simple compressed trie node.
struct TrieNode {
    // Partial key stored at this node (compressed edge label).
    edge: Vec<u8>,
    value: Option<*mut libc::c_void>,
    children: Vec<TrieNode>,
}

unsafe impl Send for TrieNode {}

impl TrieNode {
    fn new(edge: Vec<u8>) -> Self {
        TrieNode {
            edge,
            value: None,
            children: Vec::new(),
        }
    }

    fn insert(&mut self, key: &[u8], value: *mut libc::c_void) -> Option<*mut libc::c_void> {
        if key.is_empty() {
            let old = self.value;
            self.value = Some(value);
            return old;
        }
        // Find a child whose edge shares a common prefix with key.
        for child in &mut self.children {
            let common = common_prefix(&child.edge, key);
            if common == 0 {
                continue;
            }
            if common == child.edge.len() {
                // Full edge matched; recurse with the remainder.
                return child.insert(&key[common..], value);
            }
            // Split the edge at the common prefix.
            let old_edge = child.edge[common..].to_vec();
            let new_edge = key[common..].to_vec();
            let old_value = child.value.take();
            let old_children = std::mem::take(&mut child.children);

            child.edge = child.edge[..common].to_vec();

            let mut old_child = TrieNode::new(old_edge);
            old_child.value = old_value;
            old_child.children = old_children;

            if new_edge.is_empty() {
                // The new key ends exactly at the split point; store value here.
                child.value = Some(value);
                child.children = vec![old_child];
            } else {
                let mut new_child = TrieNode::new(new_edge);
                new_child.value = Some(value);
                child.children = vec![old_child, new_child];
            }
            return None;
        }
        // No matching child; add a new one.
        let mut new_child = TrieNode::new(key.to_vec());
        new_child.value = Some(value);
        self.children.push(new_child);
        None
    }

    fn get(&self, key: &[u8]) -> Option<*mut libc::c_void> {
        if key.is_empty() {
            return self.value;
        }
        for child in &self.children {
            let common = common_prefix(&child.edge, key);
            if common == child.edge.len() {
                return child.get(&key[common..]);
            }
        }
        None
    }

    fn remove(&mut self, key: &[u8]) -> Option<*mut libc::c_void> {
        if key.is_empty() {
            return self.value.take();
        }
        for child in &mut self.children {
            let common = common_prefix(&child.edge, key);
            if common == child.edge.len() {
                return child.remove(&key[common..]);
            }
        }
        None
    }

    fn collect_all(&self, prefix: &mut Vec<u8>, out: &mut Vec<(Vec<u8>, *mut libc::c_void)>) {
        let prev_len = prefix.len();
        prefix.extend_from_slice(&self.edge);
        if let Some(v) = self.value {
            out.push((prefix.clone(), v));
        }
        for child in &self.children {
            child.collect_all(prefix, out);
        }
        prefix.truncate(prev_len);
    }

    fn collect_prefix(
        &self,
        key: &[u8],
        prefix: &mut Vec<u8>,
        out: &mut Vec<(Vec<u8>, *mut libc::c_void)>,
    ) {
        let prev_len = prefix.len();
        prefix.extend_from_slice(&self.edge);

        if key.is_empty() {
            // Search prefix fully consumed; collect this node and all descendants.
            if let Some(v) = self.value {
                out.push((prefix.clone(), v));
            }
            for child in &self.children {
                child.collect_all(prefix, out);
            }
        } else {
            let common = common_prefix(&self.edge, key);
            if common == self.edge.len() {
                let remaining = &key[common..];
                if remaining.is_empty() {
                    // Search prefix exactly ends at this node; collect here and below.
                    if let Some(v) = self.value {
                        out.push((prefix.clone(), v));
                    }
                    for child in &self.children {
                        child.collect_all(prefix, out);
                    }
                } else {
                    // Edge fully consumed; continue searching in children.
                    for child in &self.children {
                        child.collect_prefix(remaining, prefix, out);
                    }
                }
            } else if common == key.len() {
                // Search prefix ends inside this edge; entire subtree matches.
                if let Some(v) = self.value {
                    out.push((prefix.clone(), v));
                }
                for child in &self.children {
                    child.collect_all(prefix, out);
                }
            }
            // else: partial mismatch — no match in this subtree.
        }
        prefix.truncate(prev_len);
    }
}

#[inline]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

struct TrieBackend {
    root: TrieNode,
    count: usize,
}

impl TrieBackend {
    fn new() -> Self {
        TrieBackend {
            root: TrieNode::new(vec![]),
            count: 0,
        }
    }
}

impl MapBackend for TrieBackend {
    fn put(&mut self, key: &CStr, value: *mut libc::c_void) -> Option<*mut libc::c_void> {
        let k = key.to_bytes();
        let old = self.root.insert(k, value);
        if old.is_none() {
            self.count += 1;
        }
        old
    }

    fn get(&self, key: &CStr) -> Option<*mut libc::c_void> {
        self.root.get(key.to_bytes())
    }

    fn remove(&mut self, key: &CStr) -> Option<*mut libc::c_void> {
        let old = self.root.remove(key.to_bytes());
        if old.is_some() {
            self.count -= 1;
        }
        old
    }

    fn count(&self) -> usize {
        self.count
    }

    fn foreach(&self, cb: QbMapTransverseFn, user_data: *mut libc::c_void) {
        let mut items = Vec::new();
        self.root.collect_all(&mut Vec::new(), &mut items);
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in items {
            if let Ok(cs) = CString::new(k) {
                let ret = unsafe { cb(cs.as_ptr(), v, user_data) };
                if ret != 0 {
                    break;
                }
            }
        }
    }

    fn iter_create(&self) -> Box<dyn MapIterBackend + Send> {
        let mut raw = Vec::new();
        self.root.collect_all(&mut Vec::new(), &mut raw);
        raw.sort_by(|a, b| a.0.cmp(&b.0));
        let items = raw
            .into_iter()
            .filter_map(|(k, v)| CString::new(k).ok().map(|cs| (cs, v)))
            .collect();
        Box::new(VecIter { items, pos: 0 })
    }

    fn iter_create_prefix(&self, prefix: &CStr) -> Box<dyn MapIterBackend + Send> {
        let p = prefix.to_bytes();
        let mut raw = Vec::new();
        self.root.collect_prefix(p, &mut Vec::new(), &mut raw);
        raw.sort_by(|a, b| a.0.cmp(&b.0));
        let items = raw
            .into_iter()
            .filter_map(|(k, v)| CString::new(k).ok().map(|cs| (cs, v)))
            .collect();
        Box::new(VecIter { items, pos: 0 })
    }
}

// ── Skiplist live iterator ────────────────────────────────────────────────────

/// Live iterator over the BTreeMap — sees inserts/removes that happen after creation.
/// The caller must keep the owning QbMap alive for the iterator's lifetime (same as C).
struct SkiplistLiveIter {
    map: *const BTreeMap<CString, *mut libc::c_void>,
    current_key: Option<CString>,
}

unsafe impl Send for SkiplistLiveIter {}

impl MapIterBackend for SkiplistLiveIter {
    fn next(&mut self) -> Option<(CString, *mut libc::c_void)> {
        use std::collections::Bound;
        let inner = unsafe { &*self.map };
        let result = match &self.current_key {
            None => inner.iter().next().map(|(k, &v)| (k.clone(), v)),
            Some(cur) => inner
                .range::<CString, _>((Bound::Excluded(cur), Bound::Unbounded))
                .next()
                .map(|(k, &v)| (k.clone(), v)),
        };
        if let Some((ref k, _)) = result {
            self.current_key = Some(k.clone());
        }
        result
    }
}

// ── Shared iterator implementation ────────────────────────────────────────────

struct VecIter {
    items: Vec<(CString, *mut libc::c_void)>,
    pos: usize,
}

unsafe impl Send for VecIter {}

impl MapIterBackend for VecIter {
    fn next(&mut self) -> Option<(CString, *mut libc::c_void)> {
        if self.pos < self.items.len() {
            let item = self.items[self.pos].clone();
            self.pos += 1;
            Some(item)
        } else {
            None
        }
    }
}

// ── Public handle types ───────────────────────────────────────────────────────

/// Opaque map handle.  Use `qb_hashtable_create`, `qb_skiplist_create`, or
/// `qb_trie_create` to obtain one.
pub struct QbMap {
    backend: Box<dyn MapBackend + Send>,
    notify: Vec<NotifyEntry>,
}

/// Iterator handle returned by `qb_map_iter_create`.
pub struct QbMapIter {
    inner: Box<dyn MapIterBackend + Send + 'static>,
    current_key: Option<CString>,
}

impl QbMap {
    fn new(backend: Box<dyn MapBackend + Send>) -> Box<Self> {
        Box::new(QbMap {
            backend,
            notify: Vec::new(),
        })
    }

    fn fire_notify(
        &self,
        event: u32,
        key: &CStr,
        old_value: *mut libc::c_void,
        new_value: *mut libc::c_void,
    ) {
        let recursive_flag = QbMapNotifyFlags::Recursive as u32;
        let free_flag = QbMapNotifyFlags::Free as u32;

        for n in &self.notify {
            if n.events & event == 0 {
                continue;
            }
            let matches = match &n.key {
                None => true,
                Some(k) => {
                    if n.events & recursive_flag != 0 {
                        // Prefix match: notify key must be a prefix of the map key.
                        key.to_bytes().starts_with(k.to_bytes())
                    } else {
                        k.as_c_str() == key
                    }
                }
            };
            if matches {
                unsafe {
                    (n.cb)(event, key.as_ptr() as *mut _, old_value, new_value, n.user_data)
                };
            }
        }

        // Fire FREE notifiers after REPLACED or DELETED (old value is being freed).
        let fires_free = (event == QbMapNotifyFlags::Replaced as u32
            || event == QbMapNotifyFlags::Deleted as u32)
            && !old_value.is_null();
        if fires_free {
            for n in &self.notify {
                if n.events & free_flag != 0 {
                    unsafe {
                        (n.cb)(free_flag, key.as_ptr() as *mut _, old_value, new_value, n.user_data)
                    };
                }
            }
        }
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn qb_hashtable_create(max_size: libc::size_t) -> *mut QbMap {
    Box::into_raw(QbMap::new(Box::new(HashBackend::new(max_size))))
}

#[no_mangle]
pub extern "C" fn qb_skiplist_create() -> *mut QbMap {
    Box::into_raw(QbMap::new(Box::new(SkiplistBackend::new())))
}

#[no_mangle]
pub extern "C" fn qb_trie_create() -> *mut QbMap {
    Box::into_raw(QbMap::new(Box::new(TrieBackend::new())))
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.  `key` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn qb_map_put(
    m: *mut QbMap,
    key: *const libc::c_char,
    value: *const libc::c_void,
) {
    if m.is_null() || key.is_null() {
        return;
    }
    let k = CStr::from_ptr(key);
    let v = value as *mut libc::c_void;
    let old = (*m).backend.put(k, v);
    let event = if old.is_some() {
        QbMapNotifyFlags::Replaced as u32
    } else {
        QbMapNotifyFlags::Inserted as u32
    };
    (*m).fire_notify(event, k, old.unwrap_or(ptr::null_mut()), v);
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.  `key` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn qb_map_get(
    m: *mut QbMap,
    key: *const libc::c_char,
) -> *mut libc::c_void {
    if m.is_null() || key.is_null() {
        return ptr::null_mut();
    }
    let k = CStr::from_ptr(key);
    (*m).backend.get(k).unwrap_or(ptr::null_mut())
}

/// Returns 1 if the key was removed, 0 if not found.
///
/// # Safety
/// `m` must be a valid `QbMap` pointer.  `key` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn qb_map_rm(
    m: *mut QbMap,
    key: *const libc::c_char,
) -> i32 {
    if m.is_null() || key.is_null() {
        return 0;
    }
    let k = CStr::from_ptr(key);
    match (*m).backend.remove(k) {
        Some(old) => {
            (*m).fire_notify(QbMapNotifyFlags::Deleted as u32, k, old, ptr::null_mut());
            1
        }
        None => 0,
    }
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_count_get(m: *const QbMap) -> libc::size_t {
    if m.is_null() {
        return 0;
    }
    (*m).backend.count()
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_foreach(
    m: *mut QbMap,
    cb: Option<unsafe extern "C" fn(key: *const libc::c_char, value: *mut libc::c_void, user_data: *mut libc::c_void) -> i32>,
    user_data: *mut libc::c_void,
) {
    if m.is_null() {
        return;
    }
    if let Some(f) = cb {
        (*m).backend.foreach(f, user_data);
    }
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_destroy(m: *mut QbMap) {
    if !m.is_null() {
        drop(Box::from_raw(m));
    }
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.  `key` may be null (wildcard).
#[no_mangle]
pub unsafe extern "C" fn qb_map_notify_add(
    m: *mut QbMap,
    key: *const libc::c_char,
    cb: Option<unsafe extern "C" fn(event: u32, key: *mut libc::c_char, old_value: *mut libc::c_void, value: *mut libc::c_void, user_data: *mut libc::c_void)>,
    events: i32,
    user_data: *mut libc::c_void,
) -> i32 {
    if m.is_null() {
        return -libc::EINVAL;
    }
    let cb = match cb {
        Some(f) => f,
        None => return -libc::EINVAL,
    };
    let events_u = events as u32;
    let free_flag = QbMapNotifyFlags::Free as u32;

    // QB_MAP_NOTIFY_FREE: only allowed with a NULL key (global), and at most one at a time.
    if events_u & free_flag != 0 {
        if !key.is_null() {
            return -libc::EINVAL;
        }
        if (*m).notify.iter().any(|n| n.events & free_flag != 0) {
            return -libc::EEXIST;
        }
    } else {
        // Exact duplicate check: same key, same callback, same event mask.
        let k_cmp: Option<CString> = if key.is_null() {
            None
        } else {
            Some(CStr::from_ptr(key).to_owned())
        };
        let cb_ptr = cb as usize;
        if (*m).notify.iter().any(|n| {
            n.key == k_cmp && (n.cb as usize) == cb_ptr && n.events == events_u
        }) {
            return -libc::EEXIST;
        }
    }

    let k = if key.is_null() {
        None
    } else {
        Some(CStr::from_ptr(key).to_owned())
    };
    (*m).notify.push(NotifyEntry {
        key: k,
        cb,
        events: events_u,
        user_data,
    });
    0
}

/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_notify_del(
    m: *mut QbMap,
    key: *const libc::c_char,
    cb: Option<unsafe extern "C" fn(event: u32, key: *mut libc::c_char, old_value: *mut libc::c_void, value: *mut libc::c_void, user_data: *mut libc::c_void)>,
    events: i32,
) -> i32 {
    if m.is_null() {
        return -libc::EINVAL;
    }
    let cb_ptr = match cb {
        Some(f) => f as usize,
        None => return -libc::EINVAL,
    };
    let k: Option<CString> = if key.is_null() {
        None
    } else {
        Some(CStr::from_ptr(key).to_owned())
    };
    let before = (*m).notify.len();
    (*m).notify.retain(|n| {
        !(n.key == k && (n.cb as usize) == cb_ptr && n.events == events as u32)
    });
    if (*m).notify.len() < before {
        0
    } else {
        -libc::ENOENT
    }
}

/// Like `qb_map_notify_del` but also matches on `user_data`.
///
/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_notify_del_2(
    m: *mut QbMap,
    key: *const libc::c_char,
    cb: Option<unsafe extern "C" fn(event: u32, key: *mut libc::c_char, old_value: *mut libc::c_void, value: *mut libc::c_void, user_data: *mut libc::c_void)>,
    events: i32,
    user_data: *mut libc::c_void,
) -> i32 {
    if m.is_null() {
        return -libc::EINVAL;
    }
    let cb_ptr = match cb {
        Some(f) => f as usize,
        None => return -libc::EINVAL,
    };
    let k: Option<CString> = if key.is_null() {
        None
    } else {
        Some(CStr::from_ptr(key).to_owned())
    };
    let before = (*m).notify.len();
    (*m).notify.retain(|n| {
        !(n.key == k
            && (n.cb as usize) == cb_ptr
            && n.events == events as u32
            && std::ptr::eq(n.user_data, user_data))
    });
    if (*m).notify.len() < before {
        0
    } else {
        -libc::ENOENT
    }
}

/// Create an iterator over all map entries.
///
/// # Safety
/// `m` must be a valid `QbMap` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_iter_create(m: *mut QbMap) -> *mut QbMapIter {
    if m.is_null() {
        return ptr::null_mut();
    }
    let iter = (*m).backend.iter_create();
    Box::into_raw(Box::new(QbMapIter {
        inner: iter,
        current_key: None,
    }))
}

/// Create a prefix iterator (trie only; falls back to empty for other backends).
///
/// # Safety
/// `m` must be a valid `QbMap` pointer.  `prefix` must be a valid C string.
#[no_mangle]
pub unsafe extern "C" fn qb_map_pref_iter_create(
    m: *mut QbMap,
    prefix: *const libc::c_char,
) -> *mut QbMapIter {
    if m.is_null() {
        return ptr::null_mut();
    }
    let iter = if prefix.is_null() {
        (*m).backend.iter_create()
    } else {
        let p = CStr::from_ptr(prefix);
        (*m).backend.iter_create_prefix(p)
    };
    Box::into_raw(Box::new(QbMapIter {
        inner: iter,
        current_key: None,
    }))
}

/// Advance to the next entry.  Returns the key (valid until next call or free).
/// Writes the value to `*value`.  Returns null when exhausted.
///
/// # Safety
/// `iter` must be a valid `QbMapIter` pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_map_iter_next(
    iter: *mut QbMapIter,
    value: *mut *mut libc::c_void,
) -> *const libc::c_char {
    if iter.is_null() {
        return ptr::null();
    }
    match (*iter).inner.next() {
        Some((key, val)) => {
            if !value.is_null() {
                *value = val;
            }
            let ptr = key.as_ptr();
            (*iter).current_key = Some(key);
            ptr
        }
        None => {
            (*iter).current_key = None;
            ptr::null()
        }
    }
}

/// # Safety
/// `iter` must be a valid `QbMapIter` pointer created by `qb_map_iter_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_map_iter_free(iter: *mut QbMapIter) {
    if !iter.is_null() {
        drop(Box::from_raw(iter));
    }
}
