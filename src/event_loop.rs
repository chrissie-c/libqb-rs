//! Event loop: fd watchers, one-shot timers, one-shot jobs, and signal handlers.
//!
//! Uses epoll on Linux and kqueue on FreeBSD, both with level-triggered semantics.
//! Signals are handled via signalfd on Linux and EVFILT_SIGNAL on BSD.

use crate::error::{QbError, QbResult};
use std::collections::{BinaryHeap, HashMap, VecDeque};
#[cfg(not(target_os = "linux"))]
use std::collections::HashSet;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Pointer to the most recently created QbLoop. NULL (0) when none exists.
static DEFAULT_LOOP: AtomicUsize = AtomicUsize::new(0);

/// Resolve a possibly-NULL loop pointer to a concrete one.
unsafe fn resolve_loop(l: *mut QbLoop) -> *mut QbLoop {
    if !l.is_null() { return l; }
    DEFAULT_LOOP.load(Ordering::Relaxed) as *mut QbLoop
}

// ── Priority ──────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum QbLoopPriority {
    Low = 0,
    Med = 1,
    High = 2,
}

const N_PRI: usize = 3;

// ── Callback types ────────────────────────────────────────────────────────────

pub type QbLoopPollDispatchFn =
    unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32;
pub type QbLoopJobDispatchFn = unsafe extern "C" fn(data: *mut libc::c_void);
pub type QbLoopTimerDispatchFn = unsafe extern "C" fn(data: *mut libc::c_void);
pub type QbLoopSignalDispatchFn =
    unsafe extern "C" fn(rsignal: i32, data: *mut libc::c_void) -> i32;
pub type QbLoopPollLowFdsEventFn = unsafe extern "C" fn(not_enough: i32, fds_available: i32);

pub type QbLoopTimerHandle = u64;
pub type QbLoopSignalHandle = u64;

// ── Internal entries ──────────────────────────────────────────────────────────

struct JobEntry {
    cb: QbLoopJobDispatchFn,
    data: *mut libc::c_void,
}
unsafe impl Send for JobEntry {}

struct TimerEntry {
    expires_ns: u64,
    id: QbLoopTimerHandle,
    #[allow(dead_code)]
    priority: QbLoopPriority,
    cb: QbLoopTimerDispatchFn,
    data: *mut libc::c_void,
    cancelled: bool,
}
unsafe impl Send for TimerEntry {}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.expires_ns == other.expires_ns && self.id == other.id
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.expires_ns.cmp(&self.expires_ns).then(other.id.cmp(&self.id))
    }
}

struct FdEntry {
    priority: QbLoopPriority,
    events: i32,
    data: *mut libc::c_void,
    cb: QbLoopPollDispatchFn,
}
unsafe impl Send for FdEntry {}

struct SignalEntry {
    id: QbLoopSignalHandle,
    #[allow(dead_code)]
    priority: QbLoopPriority,
    signum: i32,
    data: *mut libc::c_void,
    cb: QbLoopSignalDispatchFn,
}
unsafe impl Send for SignalEntry {}

// ── Loop ──────────────────────────────────────────────────────────────────────

pub struct QbLoop {
    /// epoll fd on Linux, kqueue fd on BSD.
    poll_fd: RawFd,

    jobs: [VecDeque<JobEntry>; N_PRI],
    timers: BinaryHeap<TimerEntry>,
    fd_watchers: HashMap<RawFd, FdEntry>,
    signal_entries: Vec<SignalEntry>,

    /// signalfd descriptor (Linux only).
    #[cfg(target_os = "linux")]
    signal_fd: Option<RawFd>,
    /// Set of signal numbers currently registered in kqueue (non-Linux only).
    #[cfg(not(target_os = "linux"))]
    kq_signals: HashSet<i32>,

    watched_signals: libc::sigset_t,
    running: bool,
    next_timer_id: QbLoopTimerHandle,
    next_signal_id: QbLoopSignalHandle,
    low_fds_cb: Option<QbLoopPollLowFdsEventFn>,
}

unsafe impl Send for QbLoop {}

impl QbLoop {
    pub fn new() -> QbResult<Box<Self>> {
        #[cfg(target_os = "linux")]
        let poll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        let poll_fd = unsafe { libc::kqueue() };

        if poll_fd < 0 {
            return Err(QbError::from_last_os_error());
        }

        let mut watched_signals: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut watched_signals) };

        Ok(Box::new(QbLoop {
            poll_fd,
            jobs: std::array::from_fn(|_| VecDeque::new()),
            timers: BinaryHeap::new(),
            fd_watchers: HashMap::new(),
            signal_entries: Vec::new(),
            #[cfg(target_os = "linux")]
            signal_fd: None,
            #[cfg(not(target_os = "linux"))]
            kq_signals: HashSet::new(),
            watched_signals,
            running: false,
            next_timer_id: 1,
            next_signal_id: 1,
            low_fds_cb: None,
        }))
    }

    // ── Platform backend: fd watchers ─────────────────────────────────────────

    #[cfg(target_os = "linux")]
    fn backend_add(&self, fd: RawFd, ep_events: u32) -> QbResult<()> {
        let mut ev = libc::epoll_event { events: ep_events, u64: fd as u64 };
        let r = unsafe { libc::epoll_ctl(self.poll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if r < 0 { Err(QbError::from_last_os_error()) } else { Ok(()) }
    }

    #[cfg(not(target_os = "linux"))]
    fn backend_add(&self, fd: RawFd, events: i32) -> QbResult<()> {
        let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
        let mut n = 0usize;
        if events & (libc::POLLIN as i32 | libc::POLLPRI as i32) != 0 {
            changes[n] = make_fd_kevent(fd, libc::EVFILT_READ, libc::EV_ADD | libc::EV_ENABLE);
            n += 1;
        }
        if events & libc::POLLOUT as i32 != 0 {
            changes[n] = make_fd_kevent(fd, libc::EVFILT_WRITE, libc::EV_ADD | libc::EV_ENABLE);
            n += 1;
        }
        if n == 0 { return Ok(()); }
        let r = unsafe {
            libc::kevent(self.poll_fd, changes.as_ptr(), n as i32,
                         std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 { Err(QbError::from_last_os_error()) } else { Ok(()) }
    }

    #[cfg(target_os = "linux")]
    fn backend_mod(&self, fd: RawFd, ep_events: u32) -> QbResult<()> {
        let mut ev = libc::epoll_event { events: ep_events, u64: fd as u64 };
        let r = unsafe { libc::epoll_ctl(self.poll_fd, libc::EPOLL_CTL_MOD, fd, &mut ev) };
        if r < 0 { Err(QbError::from_last_os_error()) } else { Ok(()) }
    }

    #[cfg(not(target_os = "linux"))]
    fn backend_mod(&self, fd: RawFd, old_events: i32, new_events: i32) -> QbResult<()> {
        // Delete old filters then add new ones.
        let mut del: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
        let mut nd = 0usize;
        if old_events & (libc::POLLIN as i32 | libc::POLLPRI as i32) != 0 {
            del[nd] = make_fd_kevent(fd, libc::EVFILT_READ, libc::EV_DELETE);
            nd += 1;
        }
        if old_events & libc::POLLOUT as i32 != 0 {
            del[nd] = make_fd_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE);
            nd += 1;
        }
        if nd > 0 {
            unsafe { libc::kevent(self.poll_fd, del.as_ptr(), nd as i32, std::ptr::null_mut(), 0, std::ptr::null()); }
        }
        self.backend_add(fd, new_events)
    }

    #[cfg(target_os = "linux")]
    fn backend_del(&self, fd: RawFd) {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        unsafe { libc::epoll_ctl(self.poll_fd, libc::EPOLL_CTL_DEL, fd, &mut ev) };
    }

    #[cfg(not(target_os = "linux"))]
    fn backend_del(&self, fd: RawFd) {
        let changes = [
            make_fd_kevent(fd, libc::EVFILT_READ,  libc::EV_DELETE),
            make_fd_kevent(fd, libc::EVFILT_WRITE, libc::EV_DELETE),
        ];
        unsafe { libc::kevent(self.poll_fd, changes.as_ptr(), 2, std::ptr::null_mut(), 0, std::ptr::null()); }
    }

    #[cfg(target_os = "linux")]
    fn poll_events_to_backend(events: i32) -> u32 {
        let mut ep: u32 = 0;
        if events & (libc::POLLIN as i32 | libc::POLLPRI as i32) != 0 {
            ep |= libc::EPOLLIN as u32 | libc::EPOLLRDHUP as u32;
        }
        if events & libc::POLLOUT as i32 != 0 {
            ep |= libc::EPOLLOUT as u32;
        }
        ep | libc::EPOLLERR as u32 | libc::EPOLLHUP as u32
    }

    #[cfg(target_os = "linux")]
    fn epoll_to_revents(ep: u32) -> i32 {
        let mut r = 0i32;
        if ep & (libc::EPOLLIN as u32 | libc::EPOLLRDHUP as u32 | libc::EPOLLHUP as u32) != 0 {
            r |= libc::POLLIN as i32;
        }
        if ep & libc::EPOLLOUT as u32 != 0 { r |= libc::POLLOUT as i32; }
        if ep & libc::EPOLLERR as u32 != 0 { r |= libc::POLLERR as i32; }
        if ep & libc::EPOLLPRI as u32 != 0 { r |= libc::POLLPRI as i32; }
        r
    }

    // ── Job API ───────────────────────────────────────────────────────────────

    pub fn job_add(
        &mut self,
        priority: QbLoopPriority,
        data: *mut libc::c_void,
        cb: QbLoopJobDispatchFn,
    ) -> QbResult<()> {
        self.jobs[priority as usize].push_back(JobEntry { cb, data });
        Ok(())
    }

    pub fn job_del(
        &mut self,
        priority: QbLoopPriority,
        data: *mut libc::c_void,
        cb: QbLoopJobDispatchFn,
    ) -> QbResult<()> {
        self.jobs[priority as usize].retain(|j| {
            !(j.data == data && j.cb as usize == cb as usize)
        });
        Ok(())
    }

    // ── Timer API ─────────────────────────────────────────────────────────────

    pub fn timer_add(
        &mut self,
        priority: QbLoopPriority,
        nsec_duration: u64,
        data: *mut libc::c_void,
        cb: QbLoopTimerDispatchFn,
    ) -> QbLoopTimerHandle {
        let id = self.next_timer_id;
        self.next_timer_id += 1;
        let expires_ns = crate::util::nano_current() + nsec_duration;
        self.timers.push(TimerEntry { expires_ns, id, priority, cb, data, cancelled: false });
        id
    }

    pub fn timer_del(&mut self, handle: QbLoopTimerHandle) -> QbResult<()> {
        for t in self.timers.iter_mut_unsafe() {
            if t.id == handle && !t.cancelled {
                t.cancelled = true;
                return Ok(());
            }
        }
        Err(QbError::new(libc::EINVAL))
    }

    pub fn timer_is_running(&self, handle: QbLoopTimerHandle) -> bool {
        self.timers.iter().any(|t| t.id == handle && !t.cancelled)
    }

    pub fn timer_expire_time_get(&self, handle: QbLoopTimerHandle) -> u64 {
        self.timers.iter()
            .find(|t| t.id == handle && !t.cancelled)
            .map(|t| t.expires_ns)
            .unwrap_or(0)
    }

    pub fn timer_expire_time_remaining(&self, handle: QbLoopTimerHandle) -> u64 {
        let abs = self.timer_expire_time_get(handle);
        if abs == 0 { return 0; }
        abs.saturating_sub(crate::util::nano_current())
    }

    // ── Fd watcher API ────────────────────────────────────────────────────────

    pub fn poll_add(
        &mut self,
        priority: QbLoopPriority,
        fd: RawFd,
        events: i32,
        data: *mut libc::c_void,
        cb: QbLoopPollDispatchFn,
    ) -> QbResult<()> {
        #[cfg(target_os = "linux")]
        self.backend_add(fd, Self::poll_events_to_backend(events))?;
        #[cfg(not(target_os = "linux"))]
        self.backend_add(fd, events)?;
        self.fd_watchers.insert(fd, FdEntry { priority, events, data, cb });
        Ok(())
    }

    pub fn poll_mod(
        &mut self,
        priority: QbLoopPriority,
        fd: RawFd,
        events: i32,
        data: *mut libc::c_void,
        cb: QbLoopPollDispatchFn,
    ) -> QbResult<()> {
        let entry = self.fd_watchers.get_mut(&fd).ok_or(QbError::new(libc::ENOENT))?;
        #[cfg(not(target_os = "linux"))]
        let old_events = entry.events;
        entry.priority = priority;
        entry.events = events;
        entry.data = data;
        entry.cb = cb;
        #[cfg(target_os = "linux")]
        return self.backend_mod(fd, Self::poll_events_to_backend(events));
        #[cfg(not(target_os = "linux"))]
        return self.backend_mod(fd, old_events, events);
    }

    pub fn poll_del(&mut self, fd: RawFd) -> QbResult<()> {
        if self.fd_watchers.remove(&fd).is_none() {
            return Err(QbError::new(libc::ENOENT));
        }
        self.backend_del(fd);
        Ok(())
    }

    // ── Signal API ────────────────────────────────────────────────────────────

    pub fn signal_add(
        &mut self,
        priority: QbLoopPriority,
        sig: i32,
        data: *mut libc::c_void,
        cb: QbLoopSignalDispatchFn,
    ) -> QbLoopSignalHandle {
        let id = self.next_signal_id;
        self.next_signal_id += 1;
        self.signal_entries.push(SignalEntry { id, priority, signum: sig, data, cb });
        self.rebuild_signal_watchers();
        id
    }

    pub fn signal_del(&mut self, handle: QbLoopSignalHandle) -> QbResult<()> {
        let before = self.signal_entries.len();
        self.signal_entries.retain(|s| s.id != handle);
        if self.signal_entries.len() < before {
            self.rebuild_signal_watchers();
            Ok(())
        } else {
            Err(QbError::new(libc::ENOENT))
        }
    }

    fn rebuild_signal_watchers(&mut self) {
        #[cfg(target_os = "linux")]
        self.rebuild_signal_linux();
        #[cfg(not(target_os = "linux"))]
        self.rebuild_signal_bsd();
    }

    #[cfg(target_os = "linux")]
    fn rebuild_signal_linux(&mut self) {
        if let Some(old_fd) = self.signal_fd.take() {
            self.backend_del(old_fd);
            unsafe { libc::close(old_fd) };
        }

        let mut new_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut new_mask) };
        for s in &self.signal_entries {
            unsafe { libc::sigaddset(&mut new_mask, s.signum) };
        }
        self.watched_signals = new_mask;

        if self.signal_entries.is_empty() { return; }

        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &new_mask, std::ptr::null_mut()) };

        let sfd = unsafe {
            libc::signalfd(-1, &new_mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
        };
        if sfd < 0 { return; }

        // Use u64::MAX as a sentinel to distinguish signalfd events from fd events.
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLERR | libc::EPOLLHUP) as u32,
            u64: u64::MAX,
        };
        unsafe { libc::epoll_ctl(self.poll_fd, libc::EPOLL_CTL_ADD, sfd, &mut ev) };
        self.signal_fd = Some(sfd);
    }

    #[cfg(not(target_os = "linux"))]
    fn rebuild_signal_bsd(&mut self) {
        let new_signals: HashSet<i32> =
            self.signal_entries.iter().map(|s| s.signum).collect();

        // Remove kevents for signals no longer needed.
        for &sig in &self.kq_signals {
            if !new_signals.contains(&sig) {
                let ke = make_signal_kevent(sig, libc::EV_DELETE);
                unsafe { libc::kevent(self.poll_fd, &ke, 1, std::ptr::null_mut(), 0, std::ptr::null()); }
            }
        }

        // Add kevents for newly registered signals.
        for &sig in &new_signals {
            if !self.kq_signals.contains(&sig) {
                let ke = make_signal_kevent(sig, libc::EV_ADD | libc::EV_ENABLE);
                unsafe { libc::kevent(self.poll_fd, &ke, 1, std::ptr::null_mut(), 0, std::ptr::null()); }
            }
        }

        // Update block mask.
        let mut new_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut new_mask) };
        for &sig in &new_signals {
            unsafe { libc::sigaddset(&mut new_mask, sig) };
        }
        self.watched_signals = new_mask;
        if !new_signals.is_empty() {
            unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &new_mask, std::ptr::null_mut()) };
        }

        self.kq_signals = new_signals;
    }

    // ── Main loop ─────────────────────────────────────────────────────────────

    pub fn run(&mut self) {
        self.running = true;
        while self.running {
            let timeout_ms = self.next_timer_timeout()
                .map(|d| d.as_millis() as i32)
                .unwrap_or(-1);

            #[cfg(target_os = "linux")]
            self.run_iteration_linux(timeout_ms);
            #[cfg(not(target_os = "linux"))]
            self.run_iteration_bsd(timeout_ms);
        }
    }

    #[cfg(target_os = "linux")]
    fn run_iteration_linux(&mut self, timeout_ms: i32) {
        const MAX_EVENTS: usize = 64;
        let mut raw = [libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
        let n = unsafe {
            libc::epoll_wait(self.poll_fd, raw.as_mut_ptr(), MAX_EVENTS as i32, timeout_ms)
        };

        let mut sfd_ready = false;
        let mut fd_ready: Vec<(RawFd, i32)> = Vec::new();
        if n > 0 {
            for ev in &raw[..n as usize] {
                if ev.u64 == u64::MAX {
                    sfd_ready = true;
                } else {
                    fd_ready.push((ev.u64 as RawFd, Self::epoll_to_revents(ev.events)));
                }
            }
        }

        self.dispatch_jobs_pri(QbLoopPriority::High);
        self.dispatch_timers();
        if sfd_ready { self.dispatch_signals_linux(); }
        for (fd, revents) in fd_ready { self.dispatch_fd(fd, revents); }
        self.dispatch_jobs_pri(QbLoopPriority::Med);
        self.dispatch_jobs_pri(QbLoopPriority::Low);
    }

    #[cfg(not(target_os = "linux"))]
    fn run_iteration_bsd(&mut self, timeout_ms: i32) {
        const MAX_EVENTS: usize = 64;
        let mut raw: [libc::kevent; MAX_EVENTS] = unsafe { std::mem::zeroed() };
        let n = if timeout_ms < 0 {
            unsafe { libc::kevent(self.poll_fd, std::ptr::null(), 0, raw.as_mut_ptr(), MAX_EVENTS as i32, std::ptr::null()) }
        } else {
            let ts = libc::timespec {
                tv_sec: (timeout_ms / 1000) as libc::time_t,
                tv_nsec: ((timeout_ms % 1000) * 1_000_000) as libc::c_long,
            };
            unsafe { libc::kevent(self.poll_fd, std::ptr::null(), 0, raw.as_mut_ptr(), MAX_EVENTS as i32, &ts) }
        };

        let mut fd_ready: Vec<(RawFd, i32)> = Vec::new();
        let mut sig_ready: Vec<i32> = Vec::new();
        if n > 0 {
            for ev in &raw[..n as usize] {
                if ev.filter == libc::EVFILT_READ || ev.filter == libc::EVFILT_WRITE {
                    fd_ready.push((ev.ident as RawFd, kq_to_revents(ev.filter, ev.flags)));
                } else if ev.filter == libc::EVFILT_SIGNAL {
                    sig_ready.push(ev.ident as i32);
                }
            }
        }

        self.dispatch_jobs_pri(QbLoopPriority::High);
        self.dispatch_timers();
        for signum in sig_ready { self.dispatch_signal_num(signum); }
        for (fd, revents) in fd_ready { self.dispatch_fd(fd, revents); }
        self.dispatch_jobs_pri(QbLoopPriority::Med);
        self.dispatch_jobs_pri(QbLoopPriority::Low);
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    // ── Internal dispatch ─────────────────────────────────────────────────────

    fn next_timer_timeout(&self) -> Option<std::time::Duration> {
        let has_jobs = self.jobs.iter().any(|q| !q.is_empty());
        let now = crate::util::nano_current();
        let timer_ns = self.timers.iter()
            .filter(|t| !t.cancelled)
            .map(|t| t.expires_ns.saturating_sub(now))
            .min();

        match (timer_ns, has_jobs) {
            (Some(t), true)  => Some(std::time::Duration::from_nanos(t.min(50_000_000))),
            (Some(t), false) => Some(std::time::Duration::from_nanos(t)),
            (None, true)     => Some(std::time::Duration::from_millis(50)),
            (None, false)    => None,
        }
    }

    fn dispatch_fd(&mut self, fd: RawFd, revents: i32) {
        if let Some(entry) = self.fd_watchers.get(&fd) {
            let cb = entry.cb;
            let data = entry.data;
            unsafe { cb(fd, revents, data) };
        }
    }

    fn dispatch_signal_num(&mut self, signum: i32) {
        let handlers: Vec<(QbLoopSignalDispatchFn, *mut libc::c_void)> = self
            .signal_entries.iter()
            .filter(|s| s.signum == signum)
            .map(|s| (s.cb, s.data))
            .collect();
        for (cb, data) in handlers {
            unsafe { cb(signum, data) };
        }
    }

    #[cfg(target_os = "linux")]
    fn dispatch_signals_linux(&mut self) {
        let sfd = match self.signal_fd { Some(f) => f, None => return };
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe {
                libc::read(
                    sfd,
                    &mut info as *mut _ as *mut libc::c_void,
                    std::mem::size_of::<libc::signalfd_siginfo>(),
                )
            };
            if r <= 0 { break; }
            self.dispatch_signal_num(info.ssi_signo as i32);
        }
    }

    fn dispatch_timers(&mut self) {
        let now = crate::util::nano_current();
        let mut fired: Vec<TimerEntry> = Vec::new();
        while let Some(t) = self.timers.peek() {
            if t.expires_ns <= now {
                fired.push(self.timers.pop().unwrap());
            } else {
                break;
            }
        }
        for t in fired {
            if !t.cancelled {
                unsafe { (t.cb)(t.data) };
            }
        }
    }

    fn dispatch_jobs_pri(&mut self, priority: QbLoopPriority) {
        let pri = priority as usize;
        let count = self.jobs[pri].len();
        for _ in 0..count {
            if let Some(j) = self.jobs[pri].pop_front() {
                unsafe { (j.cb)(j.data) };
            }
        }
    }
}

// ── kqueue helpers ────────────────────────────────────────────────────────────

#[cfg(not(target_os = "linux"))]
fn make_fd_kevent(fd: RawFd, filter: libc::c_short, flags: u16) -> libc::kevent {
    libc::kevent {
        ident: fd as libc::uintptr_t,
        filter,
        flags,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
        ext: [0; 4],
    }
}

#[cfg(not(target_os = "linux"))]
fn make_signal_kevent(signum: i32, flags: u16) -> libc::kevent {
    libc::kevent {
        ident: signum as libc::uintptr_t,
        filter: libc::EVFILT_SIGNAL,
        flags,
        fflags: 0,
        data: 0,
        udata: std::ptr::null_mut(),
        ext: [0; 4],
    }
}

#[cfg(not(target_os = "linux"))]
fn kq_to_revents(filter: libc::c_short, flags: u16) -> i32 {
    let mut r = 0i32;
    if filter == libc::EVFILT_READ  { r |= libc::POLLIN as i32; }
    if filter == libc::EVFILT_WRITE { r |= libc::POLLOUT as i32; }
    if flags & libc::EV_EOF   != 0 { r |= libc::POLLHUP as i32 | libc::POLLIN as i32; }
    if flags & libc::EV_ERROR != 0 { r |= libc::POLLERR as i32; }
    r
}

// ── BinaryHeap mutable iterator workaround ────────────────────────────────────

trait BinaryHeapMutIter<T> {
    fn iter_mut_unsafe(&mut self) -> std::slice::IterMut<'_, T>;
}

impl<T: Ord> BinaryHeapMutIter<T> for BinaryHeap<T> {
    fn iter_mut_unsafe(&mut self) -> std::slice::IterMut<'_, T> {
        // SAFETY: mutating `cancelled` does not change the ordering key.
        unsafe {
            let slice: &mut [T] =
                std::slice::from_raw_parts_mut(self.as_slice().as_ptr() as *mut T, self.len());
            slice.iter_mut()
        }
    }
}

impl Drop for QbLoop {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(sfd) = self.signal_fd.take() {
            unsafe { libc::close(sfd) };
        }
        unsafe { libc::close(self.poll_fd) };
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn qb_loop_create() -> *mut QbLoop {
    match QbLoop::new() {
        Ok(l) => {
            let ptr = Box::into_raw(l);
            DEFAULT_LOOP.store(ptr as usize, Ordering::Relaxed);
            ptr
        }
        Err(e) => {
            unsafe { crate::error::set_errno(e.errno) };
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_destroy(l: *mut QbLoop) {
    if !l.is_null() {
        let _ = DEFAULT_LOOP.compare_exchange(
            l as usize, 0, Ordering::Relaxed, Ordering::Relaxed,
        );
        drop(Box::from_raw(l));
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_run(l: *mut QbLoop) {
    if !l.is_null() { (*l).run(); }
}

/// # Safety
/// `l` must be a valid pointer (or NULL to target the default loop).
#[no_mangle]
pub unsafe extern "C" fn qb_loop_stop(l: *mut QbLoop) {
    let l = resolve_loop(l);
    if !l.is_null() { (*l).stop(); }
}

// ── Jobs ──────────────────────────────────────────────────────────────────────

/// # Safety
/// `l` must be a valid pointer, or NULL to target the default loop.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_job_add(
    l: *mut QbLoop,
    priority: i32,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(data: *mut libc::c_void)>,
) -> i32 {
    let priority = match priority {
        0 => QbLoopPriority::Low,
        1 => QbLoopPriority::Med,
        2 => QbLoopPriority::High,
        _ => return -libc::EINVAL,
    };
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    let l = resolve_loop(l);
    if l.is_null() { return -libc::EINVAL; }
    match (*l).job_add(priority, data, cb) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_job_del(
    l: *mut QbLoop,
    priority: i32,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(data: *mut libc::c_void)>,
) -> i32 {
    let priority = match priority {
        0 => QbLoopPriority::Low,
        1 => QbLoopPriority::Med,
        2 => QbLoopPriority::High,
        _ => return -libc::EINVAL,
    };
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    let l = resolve_loop(l);
    if l.is_null() { return -libc::EINVAL; }
    match (*l).job_del(priority, data, cb) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

// ── Timers ────────────────────────────────────────────────────────────────────

/// # Safety
/// `l` and `handle_out` must be valid pointers; `l` may be NULL to use the default loop.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_timer_add(
    l: *mut QbLoop,
    priority: QbLoopPriority,
    nsec_duration: u64,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(data: *mut libc::c_void)>,
    handle_out: *mut QbLoopTimerHandle,
) -> i32 {
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    let l = resolve_loop(l);
    if l.is_null() { return -libc::EINVAL; }
    let h = (*l).timer_add(priority, nsec_duration, data, cb);
    if !handle_out.is_null() { *handle_out = h; }
    0
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_timer_del(l: *mut QbLoop, th: QbLoopTimerHandle) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    match (*l).timer_del(th) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_timer_is_running(l: *const QbLoop, th: QbLoopTimerHandle) -> i32 {
    if l.is_null() { return 0; }
    (*l).timer_is_running(th) as i32
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_timer_expire_time_get(l: *const QbLoop, th: QbLoopTimerHandle) -> u64 {
    if l.is_null() { return 0; }
    (*l).timer_expire_time_get(th)
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_timer_expire_time_remaining(l: *const QbLoop, th: QbLoopTimerHandle) -> u64 {
    if l.is_null() { return 0; }
    (*l).timer_expire_time_remaining(th)
}

// ── Fd watchers ───────────────────────────────────────────────────────────────

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_poll_add(
    l: *mut QbLoop,
    priority: QbLoopPriority,
    fd: i32,
    events: i32,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32>,
) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    match (*l).poll_add(priority, fd, events, data, cb) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_poll_mod(
    l: *mut QbLoop,
    priority: QbLoopPriority,
    fd: i32,
    events: i32,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32>,
) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    match (*l).poll_mod(priority, fd, events, data, cb) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_poll_del(l: *mut QbLoop, fd: i32) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    match (*l).poll_del(fd) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `l` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_poll_low_fds_event_set(
    l: *mut QbLoop,
    cb: Option<unsafe extern "C" fn(not_enough: i32, fds_available: i32)>,
) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    (*l).low_fds_cb = cb;
    0
}

// ── Signals ───────────────────────────────────────────────────────────────────

/// # Safety
/// `l` and `handle_out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_signal_add(
    l: *mut QbLoop,
    priority: QbLoopPriority,
    sig: i32,
    data: *mut libc::c_void,
    cb: Option<unsafe extern "C" fn(rsignal: i32, data: *mut libc::c_void) -> i32>,
    handle_out: *mut QbLoopSignalHandle,
) -> i32 {
    if l.is_null() { return -libc::EINVAL; }
    let cb = match cb { Some(f) => f, None => return -libc::EINVAL };
    let h = (*l).signal_add(priority, sig, data, cb);
    if !handle_out.is_null() { *handle_out = h; }
    0
}

/// # Safety
/// `l` must be a valid pointer, or NULL to target the default loop.
#[no_mangle]
pub unsafe extern "C" fn qb_loop_signal_del(l: *mut QbLoop, handle: QbLoopSignalHandle) -> i32 {
    let l = resolve_loop(l);
    if l.is_null() { return -libc::EINVAL; }
    match (*l).signal_del(handle) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}
