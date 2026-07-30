//! Multi-target logging system.
//!
//! Supports up to 32 simultaneous targets (4 static + 28 dynamic).
//! Each target has its own filter, format string, and callback.

use crate::ringbuffer::{QbRingbuffer, QB_RB_FLAG_CREATE, QB_RB_FLAG_SHARED_PROCESS, QB_RB_FLAG_OVERWRITE};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::os::unix::io::IntoRawFd;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

extern "C" {
    fn sched_get_priority_min(policy: libc::c_int) -> libc::c_int;
    fn sched_get_priority_max(policy: libc::c_int) -> libc::c_int;
}

// Stored by qb_log_thread_priority_set; validated by qb_log_thread_start.
static THREAD_POLICY: AtomicI32 = AtomicI32::new(libc::SCHED_OTHER);
static THREAD_PRIORITY: AtomicI32 = AtomicI32::new(0);

// ── Constants ─────────────────────────────────────────────────────────────────

pub const QB_LOG_MAX_LEN: usize = 512;
pub const QB_LOG_ABSOLUTE_MAX_LEN: usize = 4096;
pub const QB_LOG_STRERROR_MAX_LEN: usize = 128;

pub const QB_LOG_SYSLOG: i32 = 0;
pub const QB_LOG_STDERR: i32 = 1;
pub const QB_LOG_BLACKBOX: i32 = 2;
pub const QB_LOG_STDOUT: i32 = 3;
pub const QB_LOG_TARGET_STATIC_MAX: i32 = 4;
pub const QB_LOG_TARGET_DYNAMIC_START: i32 = 4;
pub const QB_LOG_TARGET_MAX: i32 = 32;

const CONF_ENABLED: i32       = 0;
const CONF_FACILITY: i32      = 1;
const CONF_DEBUG: i32         = 2;
const CONF_SIZE: i32          = 3;
const CONF_THREADED: i32      = 4;
const CONF_PRIORITY_BUMP: i32 = 5;
const CONF_STATE_GET: i32     = 6;
const CONF_FILE_SYNC: i32     = 7;
const CONF_EXTENDED: i32      = 8;
const CONF_IDENT: i32         = 9;
const CONF_MAX_LINE_LEN: i32  = 10;
const CONF_ELLIPSIS: i32      = 11;
const CONF_USE_JOURNAL: i32   = 12;

// QB_LOG_STATE_* must match qblog.h macros (0=UNUSED,1=DISABLED,2=ENABLED)
const STATE_DISABLED: i32 = 1;
const STATE_ENABLED: i32  = 2;

const FILTER_FILE: i32           = 0;
const FILTER_FUNCTION: i32       = 1;
const FILTER_FORMAT: i32         = 2;
const FILTER_FILE_REGEX: i32     = 3;
const FILTER_FUNCTION_REGEX: i32 = 4;
const FILTER_FORMAT_REGEX: i32   = 5;

const FCONF_ADD: i32           = 0;
const FCONF_REMOVE: i32        = 1;
const FCONF_CLEAR_ALL: i32     = 2;
const FCONF_TAG_SET: i32       = 3;
const FCONF_TAG_CLEAR: i32     = 4;
const FCONF_TAG_CLEAR_ALL: i32 = 5;

const QB_XS_CHAR: char = '\x07';

// ── Callsite ──────────────────────────────────────────────────────────────────

#[repr(C)]
pub struct QbLogCallsite {
    pub function:   *const libc::c_char,
    pub filename:   *const libc::c_char,
    pub format:     *const libc::c_char,
    pub priority:   u8,
    pub _pad:       [u8; 3],
    pub lineno:     u32,
    pub targets:    u32,
    pub tags:       u32,
    pub message_id: *const libc::c_char,
}

unsafe impl Send for QbLogCallsite {}
unsafe impl Sync for QbLogCallsite {}

// ── Callback types ────────────────────────────────────────────────────────────

pub type QbLogTagsStringifyFn = unsafe extern "C" fn(tags: u32) -> *const libc::c_char;
pub type QbLogFilterFn = unsafe extern "C" fn(cs: *mut QbLogCallsite);
pub type LoggerFnRaw = unsafe extern "C" fn(i32, *mut QbLogCallsite, *mut libc::timespec, *const libc::c_char);
pub type CloseFnRaw  = unsafe extern "C" fn(i32);
pub type ReloadFnRaw = unsafe extern "C" fn(i32);

// ── Regex helper ──────────────────────────────────────────────────────────────

fn posix_regex_match(pattern: &str, text: &str) -> bool {
    let pat = match CString::new(pattern) { Ok(p) => p, Err(_) => return false };
    let txt = match CString::new(text)    { Ok(t) => t, Err(_) => return false };
    unsafe {
        let mut re: libc::regex_t = std::mem::zeroed();
        let flags = libc::REG_EXTENDED | libc::REG_NOSUB;
        if libc::regcomp(&mut re, pat.as_ptr(), flags) != 0 { return false; }
        let matched = libc::regexec(&re, txt.as_ptr(), 0, ptr::null_mut(), 0) == 0;
        libc::regfree(&mut re);
        matched
    }
}

// ── Filter ────────────────────────────────────────────────────────────────────

struct Filter {
    filter_type:   i32,
    text:          String,
    low_priority:  u8,
    high_priority: u8,
}

impl Filter {
    fn matches_callsite(&self, cs: &QbLogCallsite) -> bool {
        if cs.priority < self.low_priority || cs.priority > self.high_priority {
            return false;
        }
        let field = match self.filter_type {
            FILTER_FILE | FILTER_FILE_REGEX => {
                if cs.filename.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.filename).to_string_lossy().into_owned() }
            }
            FILTER_FUNCTION | FILTER_FUNCTION_REGEX => {
                if cs.function.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.function).to_string_lossy().into_owned() }
            }
            FILTER_FORMAT | FILTER_FORMAT_REGEX => {
                if cs.format.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.format).to_string_lossy().into_owned() }
            }
            _ => return false,
        };
        match self.filter_type {
            FILTER_FILE | FILTER_FUNCTION | FILTER_FORMAT => {
                if self.text == "*" { return true; }
                for part in self.text.split(',') {
                    let p = part.trim();
                    if !p.is_empty() && field.contains(p) { return true; }
                }
                false
            }
            _ => posix_regex_match(&self.text, &field),
        }
    }
}

// ── Target ────────────────────────────────────────────────────────────────────

enum TargetSink {
    Syslog { facility: i32, ident: String },
    Stderr,
    Stdout,
    File { file: std::fs::File, sync: bool, filename: String },
    Blackbox { rb: Box<QbRingbuffer> },
    Custom {
        logger:    LoggerFnRaw,
        close:     Option<CloseFnRaw>,
        #[allow(dead_code)]
        reload:    Option<ReloadFnRaw>,
        #[allow(dead_code)]
        user_data: *mut libc::c_void,
    },
}

unsafe impl Send for TargetSink {}

struct Target {
    state:         i32,
    sink:          TargetSink,
    format:        String,
    filters:       Vec<Filter>,
    index:         i32,
    priority_bump: u8,
    extended:      bool,
    max_line_len:  usize,
    ellipsis:      bool,
}

impl Target {
    fn is_enabled(&self) -> bool { self.state == STATE_ENABLED }

    fn passes_filters(&self, cs: &QbLogCallsite) -> bool {
        if self.filters.is_empty() {
            return cs.priority <= (libc::LOG_DEBUG as u8 + self.priority_bump);
        }
        self.filters.iter().any(|f| f.matches_callsite(cs))
    }

    fn visible_msg(&self, msg: &str) -> Option<String> {
        if let Some(xs_pos) = msg.find(QB_XS_CHAR) {
            let before = &msg[..xs_pos];
            let after  = &msg[xs_pos + QB_XS_CHAR.len_utf8()..];
            if self.extended {
                if after.is_empty() { Some(before.to_string()) }
                else { Some(format!("{}|{}", before, after)) }
            } else {
                if before.is_empty() { None } else { Some(before.to_string()) }
            }
        } else {
            Some(msg.to_string())
        }
    }

    fn write_msg(
        &mut self,
        cs: &QbLogCallsite,
        ts: &libc::timespec,
        msg: &str,
        ident: &str,
        tags_stringify: Option<QbLogTagsStringifyFn>,
    ) {
        let visible = match self.visible_msg(msg) { Some(v) => v, None => return };

        match &mut self.sink {
            TargetSink::Custom { logger, .. } => {
                let truncated = truncate_msg(&visible, self.max_line_len);
                if let Ok(s) = CString::new(truncated) {
                    let cs_ptr = cs as *const QbLogCallsite as *mut QbLogCallsite;
                    let ts_ptr = ts as *const libc::timespec as *mut libc::timespec;
                    unsafe { logger(self.index, cs_ptr, ts_ptr, s.as_ptr()) };
                }
            }
            _ => {
                let formatted = self.format_message(cs, ts, &visible, ident, tags_stringify);
                match &mut self.sink {
                    TargetSink::Stderr => {
                        let _ = io::stderr().write_all(formatted.as_bytes());
                        let _ = io::stderr().write_all(b"\n");
                    }
                    TargetSink::Stdout => {
                        let _ = io::stdout().write_all(formatted.as_bytes());
                        let _ = io::stdout().write_all(b"\n");
                    }
                    TargetSink::File { file, sync, .. } => {
                        let _ = file.write_all(formatted.as_bytes());
                        let _ = file.write_all(b"\n");
                        if *sync { let _ = file.sync_all(); }
                    }
                    TargetSink::Syslog { facility, .. } => {
                        let priority = *facility | cs.priority as i32;
                        if let Ok(s) = CString::new(formatted) {
                            #[allow(clippy::manual_c_str_literals)]
                            unsafe { libc::syslog(priority, b"%s\0".as_ptr() as *const _, s.as_ptr()) };
                        }
                    }
                    TargetSink::Blackbox { rb } => {
                        let _ = rb.chunk_write(formatted.as_bytes());
                    }
                    TargetSink::Custom { .. } => unreachable!(),
                }
            }
        }
    }

    fn format_message(
        &self,
        cs: &QbLogCallsite,
        ts: &libc::timespec,
        msg: &str,
        ident: &str,
        tags_stringify: Option<QbLogTagsStringifyFn>,
    ) -> String {
        let function = if cs.function.is_null() { "?".to_string() }
                       else { unsafe { CStr::from_ptr(cs.function).to_string_lossy().into_owned() } };
        let filename  = if cs.filename.is_null() { "?".to_string() }
                        else { unsafe { CStr::from_ptr(cs.filename).to_string_lossy().into_owned() } };
        let prio_str     = priority_to_str(cs.priority).to_string();
        let ts_t_str     = format_timestamp(ts, false);
        let ts_hires_str = format_timestamp(ts, true);
        let pid_str      = unsafe { libc::getpid() }.to_string();
        let lineno_str   = cs.lineno.to_string();
        let hostname     = get_hostname();
        let group_str: String = if let Some(f) = tags_stringify {
            let ptr = unsafe { f(cs.tags) };
            if ptr.is_null() { ident.to_string() }
            else { unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() } }
        } else { ident.to_string() };

        let fmt   = &self.format;
        let bytes = fmt.as_bytes();
        let mut out = String::with_capacity(fmt.len() + 128);
        let mut i = 0usize;

        while i < bytes.len() {
            if bytes[i] != b'%' { out.push(bytes[i] as char); i += 1; continue; }
            i += 1;
            if i >= bytes.len() { out.push('%'); break; }

            // '-' flag → right-justify (left-pad) in libqb convention
            let right_justify = if bytes[i] == b'-' { i += 1; true } else { false };

            let width_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            let width_str = &fmt[width_start..i];
            let width: Option<usize> = if !width_str.is_empty() {
                width_str.parse().ok().filter(|&w: &usize| w > 0)
            } else { None };

            if i >= bytes.len() {
                out.push('%');
                if right_justify { out.push('-'); }
                out.push_str(width_str);
                break;
            }
            let spec = bytes[i] as char; i += 1;

            let value: &str = match spec {
                't' => &ts_t_str,
                'T' => &ts_hires_str,
                'p' => &prio_str,
                'b' => msg,
                'f' => &filename,
                'l' => &lineno_str,
                'n' => &function,
                'g' => &group_str,
                'P' => &pid_str,
                'N' => ident,
                'H' => &hostname,
                'h' => &hostname,
                _ => {
                    out.push('%');
                    if right_justify { out.push('-'); }
                    out.push_str(width_str);
                    out.push(spec);
                    continue;
                }
            };
            apply_width(&mut out, value, width, right_justify);
        }

        // max_line_len is buffer-size: truncate to max_line_len-1 chars
        let limit = if self.max_line_len > 0 { self.max_line_len.saturating_sub(1) } else { 0 };
        if limit > 0 && out.len() > limit {
            if self.ellipsis {
                let trunc = limit.saturating_sub(3);
                out.truncate(trunc);
                out.push_str("...");
            } else {
                out.truncate(limit);
            }
        }
        out
    }
}

fn apply_width(out: &mut String, value: &str, width: Option<usize>, right_justify: bool) {
    if let Some(w) = width {
        if value.len() >= w {
            let mut end = w;
            while end > 0 && !value.is_char_boundary(end) { end -= 1; }
            out.push_str(&value[..end]);
        } else {
            let pad = w - value.len();
            if right_justify {
                for _ in 0..pad { out.push(' '); }
                out.push_str(value);
            } else {
                out.push_str(value);
                for _ in 0..pad { out.push(' '); }
            }
        }
    } else {
        out.push_str(value);
    }
}

fn truncate_msg(msg: &str, max_line_len: usize) -> String {
    if max_line_len == 0 { return msg.to_string(); }
    let limit = max_line_len.saturating_sub(1);
    if msg.len() <= limit { return msg.to_string(); }
    let mut end = limit;
    while end > 0 && !msg.is_char_boundary(end) { end -= 1; }
    msg[..end].to_string()
}

fn format_timestamp(ts: &libc::timespec, include_ms: bool) -> String {
    const MONTHS: [&str; 12] = ["Jan","Feb","Mar","Apr","May","Jun",
                                  "Jul","Aug","Sep","Oct","Nov","Dec"];
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) };
    let mon = MONTHS.get(tm.tm_mon as usize).copied().unwrap_or("???");
    if include_ms {
        format!("{} {:2} {:02}:{:02}:{:02}.{:03}",
            mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec,
            ts.tv_nsec / 1_000_000)
    } else {
        format!("{} {:2} {:02}:{:02}:{:02}",
            mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn get_hostname() -> String {
    let mut buf = vec![0u8; 256];
    unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
    buf.iter().position(|&b| b == 0)
        .map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
        .unwrap_or_default()
}

fn priority_to_str(p: u8) -> &'static str {
    match p as i32 {
        libc::LOG_EMERG   => "emerg",
        libc::LOG_ALERT   => "alert",
        libc::LOG_CRIT    => "crit",
        libc::LOG_ERR     => "error",
        libc::LOG_WARNING => "warning",
        libc::LOG_NOTICE  => "notice",
        libc::LOG_INFO    => "info",
        libc::LOG_DEBUG   => "debug",
        9                 => "trace",
        _ => "?",
    }
}

// ── Callsite cache key ────────────────────────────────────────────────────────

// Key uses raw pointer values since libqb callsites use static string literals.
type CallsiteKey = (usize, usize, usize, u8, u32);

fn callsite_key(
    function: *const libc::c_char,
    filename: *const libc::c_char,
    format:   *const libc::c_char,
    priority: u8,
    lineno:   u32,
) -> CallsiteKey {
    (function as usize, filename as usize, format as usize, priority, lineno)
}

// ── Global state ──────────────────────────────────────────────────────────────

struct TagMatcher { filter_type: i32, pattern: String, subsys: u32 }

impl TagMatcher {
    fn matches(&self, cs: &QbLogCallsite) -> bool {
        let field = match self.filter_type {
            FILTER_FILE | FILTER_FILE_REGEX => {
                if cs.filename.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.filename).to_string_lossy().into_owned() }
            }
            FILTER_FUNCTION | FILTER_FUNCTION_REGEX => {
                if cs.function.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.function).to_string_lossy().into_owned() }
            }
            FILTER_FORMAT | FILTER_FORMAT_REGEX => {
                if cs.format.is_null() { return false; }
                unsafe { CStr::from_ptr(cs.format).to_string_lossy().into_owned() }
            }
            _ => return false,
        };
        if self.pattern == "*" { return true; }
        for part in self.pattern.split(',') {
            let p = part.trim();
            if !p.is_empty() && field.contains(p) { return true; }
        }
        false
    }
}

struct LogState {
    initialized:    bool,
    targets:        Vec<Option<Target>>,
    callsites:      Vec<*mut QbLogCallsite>,
    callsite_cache: HashMap<CallsiteKey, *mut QbLogCallsite>,
    ident:          String,
    tags_stringify: Option<QbLogTagsStringifyFn>,
    filter_fn:      Option<QbLogFilterFn>,
    tag_matchers:   Vec<TagMatcher>,
}

unsafe impl Send for LogState {}

static LOG: OnceLock<Mutex<LogState>> = OnceLock::new();

fn log_state() -> &'static Mutex<LogState> {
    LOG.get_or_init(|| {
        let mut targets = Vec::with_capacity(QB_LOG_TARGET_MAX as usize);
        for _ in 0..QB_LOG_TARGET_MAX as usize { targets.push(None); }
        Mutex::new(LogState {
            initialized: false,
            targets,
            callsites: Vec::new(),
            callsite_cache: HashMap::new(),
            ident: String::new(),
            tags_stringify: None,
            filter_fn: None,
            tag_matchers: Vec::new(),
        })
    })
}

fn default_format() -> String { "%t [%p] %f:%l %n(): %b".to_string() }

fn init_static_targets(state: &mut LogState, name: &str, facility: i32, priority: u8) {
    state.targets[QB_LOG_SYSLOG as usize] = Some(Target {
        state: STATE_ENABLED,
        sink: TargetSink::Syslog { facility, ident: name.to_string() },
        format: "%b".to_string(),
        filters: vec![Filter {
            filter_type: FILTER_FILE, text: String::new(),
            low_priority: libc::LOG_EMERG as u8, high_priority: priority,
        }],
        index: QB_LOG_SYSLOG, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
    state.targets[QB_LOG_STDERR as usize] = Some(Target {
        state: STATE_DISABLED, sink: TargetSink::Stderr,
        format: default_format(), filters: Vec::new(),
        index: QB_LOG_STDERR, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
    let bb_name = format!("qb-bb-{}", name);
    let rb = QbRingbuffer::open(&bb_name, 1024*1024,
        QB_RB_FLAG_CREATE | QB_RB_FLAG_SHARED_PROCESS | QB_RB_FLAG_OVERWRITE, 0);
    state.targets[QB_LOG_BLACKBOX as usize] = Some(Target {
        state: STATE_DISABLED,
        sink: match rb { Ok(r) => TargetSink::Blackbox { rb: r }, Err(_) => TargetSink::Stderr },
        format: default_format(), filters: Vec::new(),
        index: QB_LOG_BLACKBOX, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
    state.targets[QB_LOG_STDOUT as usize] = Some(Target {
        state: STATE_DISABLED, sink: TargetSink::Stdout,
        format: default_format(), filters: Vec::new(),
        index: QB_LOG_STDOUT, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
}

fn update_callsite_targets(state: &mut LogState, cs: *mut QbLogCallsite) {
    let cs_ref = unsafe { &mut *cs };
    let mut mask = 0u32;
    for (idx, maybe_target) in state.targets.iter().enumerate() {
        if let Some(t) = maybe_target {
            if t.is_enabled() && t.passes_filters(cs_ref) { mask |= 1 << idx; }
        }
    }
    cs_ref.targets = mask;
    if let Some(f) = state.filter_fn { unsafe { f(cs) }; }
}

fn rebuild_all_targets(state: &mut LogState) {
    let callsites: Vec<*mut QbLogCallsite> = state.callsites.clone();
    for &cs in &callsites { update_callsite_targets(state, cs); }
}

// ── Exported: dispatch a pre-formatted message (called by C shims) ────────────

#[no_mangle]
pub unsafe extern "C" fn qb_log_dispatch_raw(
    cs:  *mut QbLogCallsite,
    ts:  *mut libc::timespec,
    msg: *const libc::c_char,
) {
    if cs.is_null() || ts.is_null() || msg.is_null() { return; }
    let msg_str = CStr::from_ptr(msg).to_string_lossy();
    let ts_ref  = &*ts;
    let cs_ref  = &*cs;
    let mask    = cs_ref.targets;

    // Pending custom-logger calls: (target_index, logger_fn, cs_ptr, ts_ptr, msg).
    // We collect these while holding the lock, then call them after releasing it
    // to avoid deadlock when the logger callback re-enters logging (e.g. calls
    // qb_log_target_format, which also acquires the state lock).
    let mut custom_calls: Vec<(i32, LoggerFnRaw, *mut QbLogCallsite, *mut libc::timespec, CString)> =
        Vec::new();

    {
        let mut state = log_state().lock();
        if !state.initialized { return; }
        let ident          = state.ident.clone();
        let tags_stringify = state.tags_stringify;

        for idx in 0..QB_LOG_TARGET_MAX as usize {
            if mask & (1 << idx) == 0 { continue; }
            if let Some(t) = state.targets.get_mut(idx).and_then(|x| x.as_mut()) {
                match &t.sink {
                    TargetSink::Custom { logger, .. } => {
                        // Defer: call the logger after releasing the state lock.
                        if let Some(visible) = t.visible_msg(&msg_str) {
                            let truncated = truncate_msg(&visible, t.max_line_len);
                            if let Ok(s) = CString::new(truncated) {
                                custom_calls.push((
                                    t.index,
                                    *logger,
                                    cs,
                                    ts as *mut libc::timespec,
                                    s,
                                ));
                            }
                        }
                    }
                    _ => {
                        t.write_msg(cs_ref, ts_ref, &msg_str, &ident, tags_stringify);
                    }
                }
            }
        }
    } // state lock released here

    // Call custom loggers without holding the state lock.
    for (target_idx, logger, cs_ptr, ts_ptr, msg_c) in custom_calls {
        logger(target_idx, cs_ptr, ts_ptr, msg_c.as_ptr());
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn qb_log_init(
    name:     *const libc::c_char,
    facility: i32,
    priority: u8,
) {
    let name_str = if name.is_null() { "default".to_string() }
                   else { CStr::from_ptr(name).to_string_lossy().into_owned() };

    let ident = CString::new(name_str.clone()).unwrap_or_default();
    libc::openlog(ident.as_ptr(), libc::LOG_PID | libc::LOG_NDELAY, facility);
    std::mem::forget(ident); // keep pointer alive for syslog

    let mut state = log_state().lock();
    state.ident = name_str.clone();
    for i in QB_LOG_TARGET_DYNAMIC_START as usize..QB_LOG_TARGET_MAX as usize {
        state.targets[i] = None;
    }
    init_static_targets(&mut state, &name_str, facility, priority);
    let callsites: Vec<*mut QbLogCallsite> = state.callsites.clone();
    for &cs in &callsites { update_callsite_targets(&mut state, cs); }
    state.initialized = true;
}

#[no_mangle]
pub extern "C" fn qb_log_fini() {
    unsafe { libc::closelog() };
    let mut state = log_state().lock();
    state.initialized = false;
    for t in state.targets.iter_mut() { *t = None; }
    for &cs in &state.callsites { unsafe { (*cs).targets = 0; } }
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_callsite_get2(
    message_id: *const libc::c_char,
    function:   *const libc::c_char,
    filename:   *const libc::c_char,
    format:     *const libc::c_char,
    priority:   u8,
    lineno:     u32,
    tags:       u32,
) -> *mut QbLogCallsite {
    let key = callsite_key(function, filename, format, priority, lineno);
    let mut state = log_state().lock();

    if let Some(&cached) = state.callsite_cache.get(&key) {
        return cached;
    }
    let cs = Box::into_raw(Box::new(QbLogCallsite {
        function, filename, format, priority, _pad: [0; 3],
        lineno, targets: 0, tags, message_id,
    }));
    if (*cs).tags == 0 {
        if let Some(m) = state.tag_matchers.iter().find(|m| m.matches(&*cs)) {
            (*cs).tags = m.subsys;
        }
    }
    update_callsite_targets(&mut state, cs);
    state.callsites.push(cs);
    state.callsite_cache.insert(key, cs);
    cs
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_callsite_get(
    function: *const libc::c_char,
    filename: *const libc::c_char,
    format:   *const libc::c_char,
    priority: u8,
    lineno:   u32,
    tags:     u32,
) -> *mut QbLogCallsite {
    qb_log_callsite_get2(ptr::null(), function, filename, format, priority, lineno, tags)
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_callsites_dump() {
    let state = log_state().lock();
    eprintln!("=== qb_log callsites ({}) ===", state.callsites.len());
    for &cs in &state.callsites {
        let cs_ref = &*cs;
        let file = if cs_ref.filename.is_null() { "?" }
                   else { CStr::from_ptr(cs_ref.filename).to_str().unwrap_or("?") };
        let func = if cs_ref.function.is_null() { "?" }
                   else { CStr::from_ptr(cs_ref.function).to_str().unwrap_or("?") };
        eprintln!("  {}:{} {}() pri={} targets={:#010x}",
            file, cs_ref.lineno, func, cs_ref.priority, cs_ref.targets);
    }
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_ctl(target: i32, conf: i32, arg: i32) -> i32 {
    {
        let state = log_state().lock();
        if !state.initialized { return -libc::EINVAL; }
    }
    if !(0..QB_LOG_TARGET_MAX).contains(&target) { return -libc::EBADF; }
    let mut state = log_state().lock();
    let t = match state.targets.get_mut(target as usize).and_then(|x| x.as_mut()) {
        Some(t) => t, None => return -libc::EBADF,
    };
    match conf {
        CONF_ENABLED => {
            t.state = if arg != 0 { STATE_ENABLED } else { STATE_DISABLED };
            let _ = t;
            rebuild_all_targets(&mut state);
        }
        CONF_FACILITY => {
            if let TargetSink::Syslog { facility, .. } = &mut t.sink { *facility = arg; }
        }
        CONF_DEBUG | CONF_PRIORITY_BUMP => { t.priority_bump = arg as u8; }
        CONF_FILE_SYNC => {
            if let TargetSink::File { sync, .. } = &mut t.sink { *sync = arg != 0; }
        }
        CONF_EXTENDED  => { t.extended = arg != 0; }
        CONF_MAX_LINE_LEN => {
            if arg < 0 { return -libc::EINVAL; }
            t.max_line_len = arg as usize;
        }
        CONF_ELLIPSIS  => { t.ellipsis = arg != 0; }
        CONF_STATE_GET => { return t.state; }
        CONF_THREADED  => { return 0; }
        CONF_SIZE => {
            match &t.sink {
                TargetSink::Syslog { .. } => return -libc::ENOSYS,
                TargetSink::Blackbox { .. } => {
                    if arg < 0 { return -libc::EINVAL; }
                    return 0;
                }
                _ => return -libc::ENOSYS,
            }
        }
        CONF_USE_JOURNAL => { return -libc::EOPNOTSUPP; }
        _ => return -libc::EINVAL,
    }
    0
}

#[repr(C)]
pub union QbLogCtl2Arg {
    pub i32: i32,
    pub s:   *const libc::c_char,
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_ctl2(target: i32, conf: i32, arg: QbLogCtl2Arg) -> i32 {
    {
        let state = log_state().lock();
        if !state.initialized { return -libc::EINVAL; }
    }
    if !(0..QB_LOG_TARGET_MAX).contains(&target) { return -libc::EBADF; }
    if conf == CONF_IDENT {
        let new_ident = if arg.s.is_null() { String::new() }
                        else { CStr::from_ptr(arg.s).to_string_lossy().into_owned() };
        let ident_cstr = match CString::new(new_ident.clone()) { Ok(s) => s, Err(_) => return -libc::EINVAL };
        {
            let mut state = log_state().lock();
            if let Some(Some(t)) = state.targets.get_mut(target as usize) {
                if let TargetSink::Syslog { ident, .. } = &mut t.sink { *ident = new_ident.clone(); }
            }
        }
        libc::openlog(ident_cstr.as_ptr(), libc::LOG_PID | libc::LOG_NDELAY, libc::LOG_USER);
        std::mem::forget(ident_cstr);
        return 0;
    }
    qb_log_ctl(target, conf, unsafe { arg.i32 })
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_filter_ctl(
    target:      i32,
    conf:        i32,
    filter_type: i32,
    text:        *const libc::c_char,
    priority:    u8,
) -> i32 {
    if conf == FCONF_TAG_SET || conf == FCONF_TAG_CLEAR || conf == FCONF_TAG_CLEAR_ALL {
        let text_str = if text.is_null() { String::new() }
                       else { CStr::from_ptr(text).to_string_lossy().into_owned() };
        let mut state = log_state().lock();
        match conf {
            FCONF_TAG_SET => {
                state.tag_matchers.push(TagMatcher { filter_type, pattern: text_str, subsys: target as u32 });
            }
            FCONF_TAG_CLEAR => {
                state.tag_matchers.retain(|m| !(m.subsys == target as u32 && m.pattern == text_str));
            }
            FCONF_TAG_CLEAR_ALL => { state.tag_matchers.clear(); }
            _ => {}
        }
        return 0;
    }
    {
        let state = log_state().lock();
        if !state.initialized { return -libc::EINVAL; }
    }
    if conf != FCONF_ADD && conf != FCONF_REMOVE && conf != FCONF_CLEAR_ALL { return -libc::EINVAL; }
    if (conf == FCONF_ADD || conf == FCONF_REMOVE) && text.is_null() { return -libc::EINVAL; }
    qb_log_filter_ctl2(target, conf, filter_type, text, libc::LOG_EMERG as u8, priority)
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_filter_ctl2(
    target:        i32,
    conf:          i32,
    filter_type:   i32,
    text:          *const libc::c_char,
    low_priority:  u8,
    high_priority: u8,
) -> i32 {
    if conf != FCONF_ADD && conf != FCONF_REMOVE && conf != FCONF_CLEAR_ALL { return -libc::EINVAL; }
    if !(0..QB_LOG_TARGET_MAX).contains(&target) { return -libc::EBADF; }

    let text_str = if text.is_null() { String::new() }
                   else { CStr::from_ptr(text).to_string_lossy().into_owned() };

    let mut state = log_state().lock();
    let t = match state.targets.get_mut(target as usize).and_then(|x| x.as_mut()) {
        Some(t) => t, None => return -libc::EBADF,
    };
    match conf {
        FCONF_ADD     => { t.filters.push(Filter { filter_type, text: text_str, low_priority, high_priority }); }
        FCONF_REMOVE  => { t.filters.retain(|f| !(f.filter_type == filter_type && f.text == text_str)); }
        FCONF_CLEAR_ALL => { t.filters.clear(); }
        _ => return -libc::EINVAL,
    }
    let _ = t;
    rebuild_all_targets(&mut state);
    0
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_filter_fn_set(
    cb: Option<unsafe extern "C" fn(cs: *mut QbLogCallsite)>,
) -> i32 {
    log_state().lock().filter_fn = cb;
    0
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_format_set(target: i32, format: *const libc::c_char) {
    if !(0..QB_LOG_TARGET_MAX).contains(&target) || format.is_null() { return; }
    let fmt = CStr::from_ptr(format).to_string_lossy().into_owned();
    let mut state = log_state().lock();
    if let Some(Some(t)) = state.targets.get_mut(target as usize) { t.format = fmt; }
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_target_format(
    target:        i32,
    cs:            *mut QbLogCallsite,
    timestamp:     *const libc::timespec,
    msg:           *const libc::c_char,
    output_buffer: *mut libc::c_char,
) {
    if output_buffer.is_null() || msg.is_null() || cs.is_null() { return; }
    let msg_str = CStr::from_ptr(msg).to_string_lossy().into_owned();
    let ts = if timestamp.is_null() {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut t); t
    } else { *timestamp };

    let state = log_state().lock();
    let ident          = state.ident.clone();
    let tags_stringify = state.tags_stringify;
    let formatted = if let Some(Some(t)) = state.targets.get(target as usize) {
        let visible = t.visible_msg(&msg_str).unwrap_or_default();
        t.format_message(&*cs, &ts, &visible, &ident, tags_stringify)
    } else { msg_str.clone() };
    drop(state);

    let bytes = formatted.as_bytes();
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const libc::c_char, output_buffer, bytes.len());
    *output_buffer.add(bytes.len()) = 0;
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_file_open(filename: *const libc::c_char) -> i32 {
    if filename.is_null() { return -libc::EINVAL; }
    let path = CStr::from_ptr(filename).to_string_lossy().into_owned();
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f, Err(e) => return -(e.raw_os_error().unwrap_or(libc::EIO)),
    };
    let mut state = log_state().lock();
    let idx = (QB_LOG_TARGET_DYNAMIC_START..QB_LOG_TARGET_MAX)
        .find(|&i| state.targets[i as usize].is_none());
    let idx = match idx { Some(i) => i, None => return -libc::ENOSPC };
    state.targets[idx as usize] = Some(Target {
        state: STATE_ENABLED,
        sink: TargetSink::File { file, sync: false, filename: path },
        format: default_format(), filters: Vec::new(),
        index: idx, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
    idx
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_file_close(target: i32) {
    if !(QB_LOG_TARGET_DYNAMIC_START..QB_LOG_TARGET_MAX).contains(&target) { return; }
    log_state().lock().targets[target as usize] = None;
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_custom_open_raw(
    logger:    Option<LoggerFnRaw>,
    close_fn:  Option<CloseFnRaw>,
    reload_fn: Option<ReloadFnRaw>,
    user_data: *mut libc::c_void,
) -> i32 {
    let logger = match logger { Some(f) => f, None => return -libc::EINVAL };
    let mut state = log_state().lock();
    let idx = (QB_LOG_TARGET_DYNAMIC_START..QB_LOG_TARGET_MAX)
        .find(|&i| state.targets[i as usize].is_none());
    let idx = match idx { Some(i) => i, None => return -libc::ENOSPC };
    state.targets[idx as usize] = Some(Target {
        state: STATE_DISABLED,
        sink: TargetSink::Custom { logger, close: close_fn, reload: reload_fn, user_data },
        format: default_format(), filters: Vec::new(),
        index: idx, priority_bump: 0, extended: false, max_line_len: 0, ellipsis: false,
    });
    idx
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_custom_close(target: i32) {
    if !(QB_LOG_TARGET_DYNAMIC_START..QB_LOG_TARGET_MAX).contains(&target) { return; }
    let mut state = log_state().lock();
    if let Some(Some(t)) = state.targets.get_mut(target as usize) {
        if let TargetSink::Custom { close: Some(close), .. } = &t.sink {
            let close = *close;
            drop(state);
            close(target);
            log_state().lock().targets[target as usize] = None;
            return;
        }
    }
    state.targets[target as usize] = None;
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_blackbox_write_to_file(filename: *const libc::c_char) -> libc::ssize_t {
    if filename.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let path = CStr::from_ptr(filename).to_string_lossy().into_owned();
    let file = match std::fs::File::create(&path) {
        Ok(f) => f, Err(_) => return -libc::EIO as libc::ssize_t,
    };
    let fd = file.into_raw_fd();
    let mut state = log_state().lock();
    if let Some(Some(t)) = state.targets.get_mut(QB_LOG_BLACKBOX as usize) {
        if let TargetSink::Blackbox { rb } = &mut t.sink {
            return rb.write_to_file(fd).map(|n| n as libc::ssize_t)
                      .unwrap_or(-libc::EIO as libc::ssize_t);
        }
    }
    unsafe { libc::close(fd) };
    -libc::ENOENT as libc::ssize_t
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_blackbox_print_from_file(filename: *const libc::c_char) -> i32 {
    if filename.is_null() { return -libc::EINVAL; }
    let path = CStr::from_ptr(filename).to_string_lossy().into_owned();
    let data = match std::fs::read(&path) { Ok(d) => d, Err(_) => return -libc::EIO };
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let len   = u32::from_le_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
        let magic = u32::from_le_bytes(data[pos+4..pos+8].try_into().unwrap());
        if magic != 0xDEAD_BEEF { break; }
        let end = pos + 8 + len;
        if end > data.len() { break; }
        let _ = io::stderr().write_all(&data[pos+8..end]);
        let _ = io::stderr().write_all(b"\n");
        pos = (end + 7) & !7;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_facility2int(fname: *const libc::c_char) -> i32 {
    if fname.is_null() { return -libc::EINVAL; }
    let s = CStr::from_ptr(fname).to_string_lossy();
    match s.as_ref() {
        "kern"   => libc::LOG_KERN,
        "user"   => libc::LOG_USER,
        "mail"   => libc::LOG_MAIL,
        "daemon" => libc::LOG_DAEMON,
        "auth"   => libc::LOG_AUTH,
        "syslog" => libc::LOG_SYSLOG,
        "lpr"    => libc::LOG_LPR,
        "news"   => libc::LOG_NEWS,
        "uucp"   => libc::LOG_UUCP,
        "cron"   => libc::LOG_CRON,
        "local0" => libc::LOG_LOCAL0,
        "local1" => libc::LOG_LOCAL1,
        "local2" => libc::LOG_LOCAL2,
        "local3" => libc::LOG_LOCAL3,
        "local4" => libc::LOG_LOCAL4,
        "local5" => libc::LOG_LOCAL5,
        "local6" => libc::LOG_LOCAL6,
        "local7" => libc::LOG_LOCAL7,
        _ => -libc::EINVAL,
    }
}

#[no_mangle]
pub extern "C" fn qb_log_facility2str(fnum: i32) -> *const libc::c_char {
    let s: &[u8] = match fnum {
        f if f == libc::LOG_KERN   => b"kern\0",
        f if f == libc::LOG_USER   => b"user\0",
        f if f == libc::LOG_MAIL   => b"mail\0",
        f if f == libc::LOG_DAEMON => b"daemon\0",
        f if f == libc::LOG_AUTH   => b"auth\0",
        f if f == libc::LOG_SYSLOG => b"syslog\0",
        f if f == libc::LOG_LPR    => b"lpr\0",
        f if f == libc::LOG_NEWS   => b"news\0",
        f if f == libc::LOG_UUCP   => b"uucp\0",
        f if f == libc::LOG_CRON   => b"cron\0",
        f if f == libc::LOG_LOCAL0 => b"local0\0",
        f if f == libc::LOG_LOCAL1 => b"local1\0",
        f if f == libc::LOG_LOCAL2 => b"local2\0",
        f if f == libc::LOG_LOCAL3 => b"local3\0",
        f if f == libc::LOG_LOCAL4 => b"local4\0",
        f if f == libc::LOG_LOCAL5 => b"local5\0",
        f if f == libc::LOG_LOCAL6 => b"local6\0",
        f if f == libc::LOG_LOCAL7 => b"local7\0",
        _ => b"unknown\0",
    };
    s.as_ptr() as *const libc::c_char
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_tags_stringify_fn_set(
    cb: Option<unsafe extern "C" fn(tags: u32) -> *const libc::c_char>,
) {
    log_state().lock().tags_stringify = cb;
}

#[no_mangle]
pub extern "C" fn qb_log_thread_start() -> i32 {
    let policy = THREAD_POLICY.load(Ordering::Relaxed);
    let priority = THREAD_PRIORITY.load(Ordering::Relaxed);
    let min = unsafe { sched_get_priority_min(policy) };
    let max = unsafe { sched_get_priority_max(policy) };
    if min == -1 || max == -1 || priority < min || priority > max {
        return -libc::EINVAL;
    }
    0
}

#[no_mangle]
pub extern "C" fn qb_log_thread_priority_set(policy: i32, priority: i32) -> i32 {
    THREAD_POLICY.store(policy, Ordering::Relaxed);
    THREAD_PRIORITY.store(priority, Ordering::Relaxed);
    0
}

#[no_mangle]
pub unsafe extern "C" fn qb_log_file_reopen(target: i32, name: *const libc::c_char) -> i32 {
    let filename_override: Option<String> = if name.is_null() { None }
        else { Some(CStr::from_ptr(name).to_string_lossy().into_owned()) };
    let mut state = log_state().lock();
    let entry = match state.targets.get_mut(target as usize) {
        Some(Some(e)) => e, _ => return -libc::EBADF,
    };
    if let TargetSink::File { ref mut file, ref mut filename, .. } = entry.sink {
        let new_path = filename_override.unwrap_or_else(|| filename.clone());
        match std::fs::OpenOptions::new().create(true).append(true).open(&new_path) {
            Ok(f) => { *file = f; *filename = new_path; 0 }
            Err(e) => -(e.raw_os_error().unwrap_or(libc::EIO)),
        }
    } else { -libc::EINVAL }
}
