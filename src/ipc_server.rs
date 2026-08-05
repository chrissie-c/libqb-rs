//! IPC server.

use crate::error::{QbError, QbResult};
use crate::ipc::{
    event_socket_name, unix_server_listen,
    QbIpcRequestHeader, QbIpcResponseHeader, QbIpcShmConnectionResponse, QbIpcType,
    QB_IPC_MSG_AUTHENTICATE, QB_IPC_MSG_DISCONNECT, PATH_MAX,
};
use crate::ringbuffer::{QbRingbuffer, QB_RB_FLAG_CREATE, QB_RB_FLAG_SHARED_PROCESS};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::ptr;

// ── Statistics ────────────────────────────────────────────────────────────────

#[repr(C)]
pub struct QbIpcsStats {
    pub active_connections: u32,
    pub closed_connections: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct QbIpcsConnectionStats {
    pub client_pid: libc::pid_t,
    pub client_uid: libc::uid_t,
    pub client_gid: libc::gid_t,
    pub requests: u64,
    pub responses: u64,
    pub events: u64,
    pub send_retries: u64,
    pub recv_retries: u64,
    pub flow_control_state: u32,
    pub flow_control_count: u64,
}

#[repr(C)]
pub struct QbIpcsConnectionStats2 {
    pub client_pid: libc::pid_t,
    pub requests: u64,
    pub responses: u64,
    pub events: u64,
    pub send_retries: u64,
    pub recv_retries: u64,
    pub flow_control_state: i32,
    pub flow_control_count: u64,
    pub event_q_length: u32,
}

#[repr(i32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum QbIpcsRateLimit {
    Fast = 0,
    Normal = 1,
    Slow = 2,
    Off = 3,
    Off2 = 4,
}

// ── Callback types ────────────────────────────────────────────────────────────

pub type QbIpcsDispatchFnT =
    unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32;
pub type QbIpcsDispatchAddFn = unsafe extern "C" fn(
    priority: i32,
    fd: i32,
    events: i32,
    data: *mut libc::c_void,
    dispatch_fn: Option<QbIpcsDispatchFnT>,
) -> i32;
pub type QbIpcsDispatchModFn = unsafe extern "C" fn(
    priority: i32,
    fd: i32,
    events: i32,
    data: *mut libc::c_void,
    dispatch_fn: Option<QbIpcsDispatchFnT>,
) -> i32;
pub type QbIpcsDispatchDelFn = unsafe extern "C" fn(fd: i32) -> i32;
pub type QbIpcsJobAddFn = unsafe extern "C" fn(
    priority: i32,
    data: *mut libc::c_void,
    dispatch_fn: Option<unsafe extern "C" fn(data: *mut libc::c_void)>,
) -> i32;

pub type QbIpcsConnectionAcceptFn =
    unsafe extern "C" fn(c: *mut QbIpcsConnection, uid: libc::uid_t, gid: libc::gid_t) -> i32;
pub type QbIpcsConnectionCreatedFn = unsafe extern "C" fn(c: *mut QbIpcsConnection);
pub type QbIpcsConnectionClosedFn = unsafe extern "C" fn(c: *mut QbIpcsConnection) -> i32;
pub type QbIpcsConnectionDestroyedFn = unsafe extern "C" fn(c: *mut QbIpcsConnection);
pub type QbIpcsMsgProcessFn =
    unsafe extern "C" fn(c: *mut QbIpcsConnection, data: *mut libc::c_void, size: libc::size_t) -> i32;

// ── Handler structs ───────────────────────────────────────────────────────────

#[repr(C)]
#[allow(clippy::type_complexity)]
pub struct QbIpcsPollHandlers {
    pub job_add: Option<unsafe extern "C" fn(priority: crate::event_loop::QbLoopPriority, data: *mut libc::c_void, dispatch_fn: Option<unsafe extern "C" fn(data: *mut libc::c_void)>) -> i32>,
    pub dispatch_add: Option<unsafe extern "C" fn(priority: crate::event_loop::QbLoopPriority, fd: i32, events: i32, data: *mut libc::c_void, dispatch_fn: Option<unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32>) -> i32>,
    pub dispatch_mod: Option<unsafe extern "C" fn(priority: crate::event_loop::QbLoopPriority, fd: i32, events: i32, data: *mut libc::c_void, dispatch_fn: Option<unsafe extern "C" fn(fd: i32, revents: i32, data: *mut libc::c_void) -> i32>) -> i32>,
    pub dispatch_del: Option<unsafe extern "C" fn(fd: i32) -> i32>,
}

#[repr(C)]
pub struct QbIpcsServiceHandlers {
    pub connection_accept: Option<unsafe extern "C" fn(c: *mut QbIpcsConnection, uid: libc::uid_t, gid: libc::gid_t) -> i32>,
    pub connection_created: Option<unsafe extern "C" fn(c: *mut QbIpcsConnection)>,
    pub msg_process: Option<unsafe extern "C" fn(c: *mut QbIpcsConnection, data: *mut libc::c_void, size: libc::size_t) -> i32>,
    pub connection_closed: Option<unsafe extern "C" fn(c: *mut QbIpcsConnection) -> i32>,
    pub connection_destroyed: Option<unsafe extern "C" fn(c: *mut QbIpcsConnection)>,
}

// ── Connection ────────────────────────────────────────────────────────────────

pub struct QbIpcsConnection {
    stream: UnixStream,
    event_stream: Option<UnixStream>,
    // SHM transport (populated when ipc_type == Shm).
    shm_request_rb: Option<Box<QbRingbuffer>>,
    shm_response_rb: Option<Box<QbRingbuffer>>,
    shm_event_rb: Option<Box<QbRingbuffer>>,
    // Directory created by mkdtemp for SHM ring buffer files.
    shm_dir: Option<String>,
    ipc_type: QbIpcType,
    #[allow(dead_code)]
    pid: libc::pid_t,
    #[allow(dead_code)]
    uid: libc::uid_t,
    #[allow(dead_code)]
    gid: libc::gid_t,
    max_msg_size: usize,
    context: *mut libc::c_void,
    service_context: *mut libc::c_void,
    service_id: i32,
    pub ref_count: i32,
    stats: QbIpcsConnectionStats,
    event_q_length: u32,
    auth_uid: libc::uid_t,
    auth_gid: libc::gid_t,
    auth_mode: libc::mode_t,
}

unsafe impl Send for QbIpcsConnection {}

impl QbIpcsConnection {
    fn new(
        stream: UnixStream,
        ipc_type: QbIpcType,
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
        max_msg_size: usize,
        service_context: *mut libc::c_void,
        service_id: i32,
    ) -> Box<Self> {
        Box::new(QbIpcsConnection {
            stream,
            event_stream: None,
            shm_request_rb: None,
            shm_response_rb: None,
            shm_event_rb: None,
            shm_dir: None,
            ipc_type,
            pid,
            uid,
            gid,
            max_msg_size,
            context: ptr::null_mut(),
            service_context,
            service_id,
            ref_count: 1,
            stats: QbIpcsConnectionStats {
                client_pid: pid,
                client_uid: uid,
                client_gid: gid,
                requests: 0,
                responses: 0,
                events: 0,
                send_retries: 0,
                recv_retries: 0,
                flow_control_state: 0,
                flow_control_count: 0,
            },
            event_q_length: 0,
            auth_uid: uid,
            auth_gid: gid,
            auth_mode: 0o600,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn send_response(&mut self, data: &[u8]) -> QbResult<usize> {
        if self.ipc_type == QbIpcType::Shm {
            let rb = self.shm_response_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            rb.chunk_write(data)?;
            // Signal client: 1 byte on the setup socket.
            let one: u8 = 1;
            unsafe { libc::send(self.stream.as_raw_fd(), &one as *const _ as *const libc::c_void, 1, libc::MSG_NOSIGNAL) };
        } else {
            self.stream.write_all(data).map_err(QbError::from)?;
        }
        self.stats.responses += 1;
        Ok(data.len())
    }

    pub fn send_event(&mut self, data: &[u8]) -> QbResult<usize> {
        if data.len() > self.max_msg_size {
            return Err(QbError::new(libc::EMSGSIZE));
        }
        if self.ipc_type == QbIpcType::Shm {
            let rb = self.shm_event_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            rb.chunk_write(data)?;
            let one: u8 = 1;
            unsafe { libc::send(self.stream.as_raw_fd(), &one as *const _ as *const libc::c_void, 1, libc::MSG_NOSIGNAL) };
        } else {
            let stream = match self.event_stream.as_mut() {
                Some(s) => s,
                None => return Err(QbError::new(libc::ENOTCONN)),
            };
            stream.write_all(data).map_err(QbError::from)?;
        }
        self.stats.events += 1;
        self.event_q_length += 1;
        Ok(data.len())
    }

    pub fn send_response_iov(&mut self, iov: *const libc::iovec, iov_len: usize) -> QbResult<usize> {
        if self.ipc_type == QbIpcType::Shm {
            let rb = self.shm_response_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            let total: usize = (0..iov_len).map(|i| unsafe { (*iov.add(i)).iov_len }).sum();
            let ptr = rb.chunk_alloc(total)?;
            let mut off = 0;
            for i in 0..iov_len {
                let iv = unsafe { &*iov.add(i) };
                unsafe { std::ptr::copy_nonoverlapping(iv.iov_base as *const u8, ptr.add(off), iv.iov_len) };
                off += iv.iov_len;
            }
            rb.chunk_commit(total)?;
            let one: u8 = 1;
            unsafe { libc::send(self.stream.as_raw_fd(), &one as *const _ as *const libc::c_void, 1, libc::MSG_NOSIGNAL) };
            self.stats.responses += 1;
            Ok(total)
        } else {
            let r = unsafe { libc::writev(self.stream.as_raw_fd(), iov, iov_len as i32) };
            if r < 0 { Err(QbError::from_last_os_error()) } else { self.stats.responses += 1; Ok(r as usize) }
        }
    }

    pub fn auth_set(&mut self, uid: libc::uid_t, gid: libc::gid_t, mode: libc::mode_t) {
        self.auth_uid = uid;
        self.auth_gid = gid;
        self.auth_mode = mode;
    }

    pub fn stats_get_2(&self) -> Box<QbIpcsConnectionStats2> {
        Box::new(QbIpcsConnectionStats2 {
            client_pid: self.stats.client_pid,
            requests: self.stats.requests,
            responses: self.stats.responses,
            events: self.stats.events,
            send_retries: self.stats.send_retries,
            recv_retries: self.stats.recv_retries,
            flow_control_state: self.stats.flow_control_state as i32,
            flow_control_count: self.stats.flow_control_count,
            event_q_length: self.event_q_length,
        })
    }

    pub fn stats_clear(&mut self) {
        let pid = self.stats.client_pid;
        self.stats.requests = 0;
        self.stats.responses = 0;
        self.stats.events = 0;
        self.stats.send_retries = 0;
        self.stats.recv_retries = 0;
        self.stats.flow_control_state = 0;
        self.stats.flow_control_count = 0;
        self.stats.client_pid = pid;
        self.event_q_length = 0;
    }
}

// ── Service ───────────────────────────────────────────────────────────────────

pub struct QbIpcsService {
    listener: UnixListener,
    service_name: String,
    #[allow(dead_code)]
    service_id: u32,
    #[allow(dead_code)]
    ipc_type: QbIpcType,
    connections: HashMap<RawFd, Box<QbIpcsConnection>>,
    service_handlers: QbIpcsServiceHandlers,
    poll_handlers: Option<QbIpcsPollHandlers>,
    context: *mut libc::c_void,
    ref_count: i32,
    stats: QbIpcsStats,
    max_buf_size: u32,
    rate_limit: QbIpcsRateLimit,
    socket_path: Option<String>,
}

unsafe impl Send for QbIpcsService {}

impl QbIpcsService {
    pub fn create(
        name: &str,
        service_id: u32,
        ipc_type: QbIpcType,
        handlers: QbIpcsServiceHandlers,
    ) -> QbResult<Box<Self>> {
        let (listener, socket_path) = unix_server_listen(name)?;

        Ok(Box::new(QbIpcsService {
            listener,
            service_name: name.to_string(),
            service_id,
            ipc_type,
            connections: HashMap::new(),
            service_handlers: handlers,
            poll_handlers: None,
            context: ptr::null_mut(),
            ref_count: 1,
            stats: QbIpcsStats {
                active_connections: 0,
                closed_connections: 0,
            },
            max_buf_size: 64 * 1024,
            rate_limit: QbIpcsRateLimit::Normal,
            socket_path,
        }))
    }

    /// Accept a pending connection, perform handshake, set up data channel.
    pub fn accept_connection(&mut self) -> QbResult<RawFd> {
        let (mut stream, _) = self.listener.accept().map_err(QbError::from)?;
        let fd = stream.as_raw_fd();

        let (pid_from_cred, uid, gid) = peer_credentials(fd)?;

        // Read auth request: qb_ipc_connection_request (C layout, 24 bytes total):
        //   hdr.id       at offset 0  (4 bytes)
        //   hdr._pad_id  at offset 4  (4 bytes, padding)
        //   hdr.size     at offset 8  (4 bytes)
        //   hdr._pad_sz  at offset 12 (4 bytes, padding)
        //   max_msg_size at offset 16 (4 bytes)
        //   padding      at offset 20 (4 bytes, align(8))
        let mut req_buf = [0u8; 24];
        stream.read_exact(&mut req_buf).map_err(QbError::from)?;

        let hdr = unsafe { &*(req_buf.as_ptr() as *const QbIpcRequestHeader) };
        if hdr.id != QB_IPC_MSG_AUTHENTICATE {
            return Err(QbError::new(libc::EPROTO));
        }
        // max_msg_size is at offset 16 in C's qb_ipc_connection_request.
        let client_max = u32::from_le_bytes(req_buf[16..20].try_into().unwrap());
        let pid = pid_from_cred;
        let effective_max = (self.max_buf_size as usize).max(client_max as usize);

        let mut conn = QbIpcsConnection::new(
            stream, self.ipc_type, pid, uid, gid, effective_max, self.context, self.service_id as i32,
        );

        let accepted = if let Some(cb) = self.service_handlers.connection_accept {
            unsafe { cb(conn.as_mut(), uid, gid) }
        } else {
            0
        };

        if self.ipc_type == QbIpcType::Shm {
            self.finish_accept_shm(conn, fd, pid, uid, gid, accepted, effective_max)
        } else {
            self.finish_accept_socket(conn, fd, pid, accepted, effective_max)
        }
    }

    fn finish_accept_socket(
        &mut self,
        mut conn: Box<QbIpcsConnection>,
        fd: RawFd,
        pid: libc::pid_t,
        accepted: i32,
        effective_max: usize,
    ) -> QbResult<RawFd> {
        let ev_name = event_socket_name(&self.service_name, pid);
        let (ev_listener, ev_fs_path) = unix_server_listen(&ev_name)?;

        let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
        unsafe {
            libc::setsockopt(
                ev_listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }

        // Send the full connection response (must be sizeof(qb_ipc_connection_response) = 12328 bytes).
        let mut sock_resp = QbIpcShmConnectionResponse::zeroed();
        sock_resp.hdr = QbIpcResponseHeader::new(
            QB_IPC_MSG_AUTHENTICATE,
            std::mem::size_of::<QbIpcShmConnectionResponse>() as i32,
            if accepted == 0 { 0 } else { accepted },
        );
        sock_resp.connection_type = QbIpcType::Socket as i32;
        sock_resp.max_msg_size = effective_max as u32;
        sock_resp.connection = conn.as_mut() as *mut QbIpcsConnection as u64;
        let resp_bytes = unsafe {
            std::slice::from_raw_parts(
                &sock_resp as *const _ as *const u8,
                std::mem::size_of::<QbIpcShmConnectionResponse>(),
            )
        };
        conn.stream.write_all(resp_bytes).map_err(QbError::from)?;

        if accepted != 0 {
            if let Some(ref p) = ev_fs_path { let _ = std::fs::remove_file(p); }
            return Err(QbError::new(accepted.unsigned_abs() as i32));
        }

        let ev_result = ev_listener.accept();
        if let Ok((ev_stream, _)) = ev_result {
            conn.event_stream = Some(ev_stream);
        }
        if let Some(ref p) = ev_fs_path { let _ = std::fs::remove_file(p); }

        if let Some(cb) = self.service_handlers.connection_created {
            unsafe { cb(conn.as_mut()) };
        }

        self.stats.active_connections += 1;
        self.connections.insert(fd, conn);
        Ok(fd)
    }

    fn finish_accept_shm(
        &mut self,
        mut conn: Box<QbIpcsConnection>,
        fd: RawFd,
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
        accepted: i32,
        effective_max: usize,
    ) -> QbResult<RawFd> {
        let server_pid = unsafe { libc::getpid() };

        // Create a temporary directory matching C libqb's mkdtemp pattern:
        //   /dev/shm/qb-<server_pid>-<client_pid>-<fd>-XXXXXX
        // Then ring buffer names are: <dir>/qb-{request,response,event}-<service>
        let tmpl = format!("/dev/shm/qb-{}-{}-{}-XXXXXX\0", server_pid, pid, fd);
        let mut tmpl_bytes = tmpl.into_bytes();
        let dir = unsafe {
            let p = libc::mkdtemp(tmpl_bytes.as_mut_ptr() as *mut libc::c_char);
            if p.is_null() {
                return Err(QbError::from_last_os_error());
            }
            // Set directory permissions to 0770.
            libc::chmod(p, 0o770);
            // chown — may fail if not root; ignore.
            libc::chown(p, uid, gid);
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        };

        // Ring buffer base names (without -header/-data suffixes).
        let req_name = format!("{}/qb-request-{}", dir, self.service_name);
        let rsp_name = format!("{}/qb-response-{}", dir, self.service_name);
        let evt_name = format!("{}/qb-event-{}", dir, self.service_name);

        let rb_flags = QB_RB_FLAG_CREATE | QB_RB_FLAG_SHARED_PROCESS;
        let rb_size = effective_max;
        let req_rb = QbRingbuffer::open(&req_name, rb_size, rb_flags, std::mem::size_of::<i32>())?;
        let rsp_rb = match QbRingbuffer::open(&rsp_name, rb_size, rb_flags, 0) {
            Ok(rb) => rb,
            Err(e) => {
                drop(req_rb);
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let evt_rb = match QbRingbuffer::open(&evt_name, rb_size, rb_flags, 0) {
            Ok(rb) => rb,
            Err(e) => {
                drop(req_rb); drop(rsp_rb);
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };

        let _ = req_rb.chown(uid, gid);
        let _ = rsp_rb.chown(uid, gid);
        let _ = evt_rb.chown(uid, gid);

        // Build connection response matching struct qb_ipc_connection_response (12328 bytes).
        let mut shm_resp = QbIpcShmConnectionResponse::zeroed();
        shm_resp.hdr = QbIpcResponseHeader::new(
            QB_IPC_MSG_AUTHENTICATE,
            std::mem::size_of::<QbIpcShmConnectionResponse>() as i32,
            if accepted == 0 { 0 } else { accepted },
        );
        shm_resp.connection_type = QbIpcType::Shm as i32;
        shm_resp.max_msg_size = effective_max as u32;
        shm_resp.connection = conn.as_mut() as *mut QbIpcsConnection as u64;

        fn copy_rb_name(dst: &mut [u8; PATH_MAX], src: &str) {
            let b = src.as_bytes();
            let n = b.len().min(PATH_MAX - 1);
            dst[..n].copy_from_slice(&b[..n]);
        }
        copy_rb_name(&mut shm_resp.request, &req_name);
        copy_rb_name(&mut shm_resp.response, &rsp_name);
        copy_rb_name(&mut shm_resp.event, &evt_name);


        let resp_bytes = unsafe {
            std::slice::from_raw_parts(
                &shm_resp as *const _ as *const u8,
                std::mem::size_of::<QbIpcShmConnectionResponse>(),
            )
        };
        conn.stream.write_all(resp_bytes).map_err(QbError::from)?;

        if accepted != 0 {
            // Rejected — clean up ring buffers and directory.
            drop(req_rb); drop(rsp_rb); drop(evt_rb);
            let _ = std::fs::remove_dir_all(&dir);
            return Err(QbError::new(accepted.unsigned_abs() as i32));
        }

        conn.shm_request_rb = Some(req_rb);
        conn.shm_response_rb = Some(rsp_rb);
        conn.shm_event_rb = Some(evt_rb);
        conn.shm_dir = Some(dir);

        if let Some(cb) = self.service_handlers.connection_created {
            unsafe { cb(conn.as_mut()) };
        }

        self.stats.active_connections += 1;
        self.connections.insert(fd, conn);
        Ok(fd)
    }

    /// Dispatch a readable event on a connection fd.
    pub fn dispatch_connection(&mut self, fd: RawFd) -> QbResult<()> {
        let (max, is_shm) = self.connections.get(&fd)
            .map(|c| (c.max_msg_size, c.ipc_type == QbIpcType::Shm))
            .ok_or_else(|| QbError::new(libc::ENOENT))?;

        if is_shm {
            self.dispatch_connection_shm(fd, max)
        } else {
            self.dispatch_connection_socket(fd, max)
        }
    }

    fn dispatch_connection_shm(&mut self, fd: RawFd, max: usize) -> QbResult<()> {
        let rate_limit = self.rate_limit;

        // Consume the 1-byte notification from the setup socket.
        let mut byte = [0u8; 1];
        let r = unsafe {
            libc::recv(fd, byte.as_mut_ptr() as *mut libc::c_void, 1, libc::MSG_DONTWAIT)
        };
        if r == 0 {
            return self.close_connection(fd);
        }
        // r < 0 with EAGAIN is OK — spurious wakeup; still check the RB.

        if rate_limit == QbIpcsRateLimit::Off {
            return Ok(());
        }

        // Drain all pending chunks from the request ring buffer.
        loop {
            let chunks = {
                let conn = match self.connections.get(&fd) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                match conn.shm_request_rb.as_ref() {
                    Some(rb) => rb.chunks_used(),
                    None => return Ok(()),
                }
            };
            if chunks <= 0 { break; }

            let mut buf = vec![0u8; max];
            let msg_size = {
                let conn = match self.connections.get_mut(&fd) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                match conn.shm_request_rb.as_mut().unwrap().chunk_read(&mut buf, 0) {
                    Ok(n) => n,
                    Err(_) => break,
                }
            };

            let hdr_size = std::mem::size_of::<QbIpcRequestHeader>();
            if msg_size >= hdr_size {
                let msg_id = unsafe { (*(buf.as_ptr() as *const QbIpcRequestHeader)).id };
                if msg_id == QB_IPC_MSG_DISCONNECT {
                    return self.close_connection(fd);
                }
            }

            {
                let conn = match self.connections.get_mut(&fd) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                conn.stats.requests += 1;
            }

            let conn_ptr = match self.connections.get_mut(&fd) {
                Some(c) => c.as_mut() as *mut QbIpcsConnection,
                None => return Ok(()),
            };
            if let Some(cb) = self.service_handlers.msg_process {
                unsafe { cb(conn_ptr, buf.as_mut_ptr() as *mut libc::c_void, msg_size) };
            }
        }

        Ok(())
    }

    fn dispatch_connection_socket(&mut self, fd: RawFd, max: usize) -> QbResult<()> {
        let rate_limit = self.rate_limit;
        let hdr_size = std::mem::size_of::<QbIpcRequestHeader>();
        let mut buf = vec![0u8; max];

        let hdr_result = {
            let conn = match self.connections.get_mut(&fd) {
                Some(c) => c,
                None => return Err(QbError::new(libc::ENOENT)),
            };
            match conn.stream.read(&mut buf[..hdr_size]) {
                Ok(0) | Err(_) => Err(()),
                Ok(n) if n < hdr_size => conn.stream.read_exact(&mut buf[n..hdr_size]).map_err(|_| ()),
                Ok(_) => Ok(()),
            }
        };

        if hdr_result.is_err() {
            return self.close_connection(fd);
        }

        let (msg_id, msg_size) = {
            let hdr = unsafe { &*(buf.as_ptr() as *const QbIpcRequestHeader) };
            let size = if hdr.size as usize >= hdr_size { (hdr.size as usize).min(max) } else { hdr_size };
            (hdr.id, size)
        };

        if msg_id == QB_IPC_MSG_DISCONNECT {
            return self.close_connection(fd);
        }

        if msg_size > hdr_size {
            let body_result = {
                let conn = match self.connections.get_mut(&fd) {
                    Some(c) => c,
                    None => return Err(QbError::new(libc::ENOENT)),
                };
                conn.stream.read_exact(&mut buf[hdr_size..msg_size]).map_err(|_| ())
            };
            if body_result.is_err() {
                return self.close_connection(fd);
            }
        }

        {
            let conn = match self.connections.get_mut(&fd) {
                Some(c) => c,
                None => return Err(QbError::new(libc::ENOENT)),
            };
            conn.stats.requests += 1;
        }

        if rate_limit == QbIpcsRateLimit::Off {
            return Ok(());
        }

        let conn_ptr = match self.connections.get_mut(&fd) {
            Some(c) => c.as_mut() as *mut QbIpcsConnection,
            None => return Err(QbError::new(libc::ENOENT)),
        };

        if let Some(cb) = self.service_handlers.msg_process {
            unsafe { cb(conn_ptr, buf.as_mut_ptr() as *mut libc::c_void, msg_size) };
        } else {
        }

        Ok(())
    }

    fn close_connection(&mut self, fd: RawFd) -> QbResult<()> {
        // Remove from event loop first so no more callbacks fire for this fd.
        if let Some(ref ph) = self.poll_handlers {
            if let Some(del) = ph.dispatch_del {
                unsafe { del(fd) };
            }
        }

        if let Some(mut conn) = self.connections.remove(&fd) {
            self.stats.active_connections = self.stats.active_connections.saturating_sub(1);
            self.stats.closed_connections += 1;

            if let Some(cb) = self.service_handlers.connection_closed {
                unsafe { cb(conn.as_mut()) };
            }

            if let Some(cb) = self.service_handlers.connection_destroyed {
                unsafe { cb(conn.as_mut()) };
            }

            // Take ownership of the SHM dir path before dropping ring buffers.
            let shm_dir = conn.shm_dir.take();
            // Drop ring buffers (unlinks header/data files).
            conn.shm_request_rb.take();
            conn.shm_response_rb.take();
            conn.shm_event_rb.take();
            // Now remove the empty directory.
            if let Some(dir) = shm_dir {
                let _ = std::fs::remove_dir_all(&dir);
            }
            // conn drops here, closing streams.
        }
        Ok(())
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn connection_first(&self) -> Option<*mut QbIpcsConnection> {
        self.connections.values().next().map(|c| c.as_ref() as *const _ as *mut _)
    }

    pub fn connection_next(
        &self,
        current: *const QbIpcsConnection,
    ) -> Option<*mut QbIpcsConnection> {
        let mut found = false;
        for c in self.connections.values() {
            if found {
                return Some(c.as_ref() as *const _ as *mut _);
            }
            if std::ptr::eq(c.as_ref(), current) {
                found = true;
            }
        }
        None
    }

    pub fn stats_get(&mut self, clear: bool) -> QbIpcsStats {
        let s = QbIpcsStats {
            active_connections: self.stats.active_connections,
            closed_connections: self.stats.closed_connections,
        };
        if clear {
            self.stats.closed_connections = 0;
        }
        s
    }
}

impl Drop for QbIpcsService {
    fn drop(&mut self) {
        if let Some(ref path) = self.socket_path {
            let _ = std::fs::remove_file(path);
        }
        // Drain connections respecting external ref-counts.
        let conns: Vec<Box<QbIpcsConnection>> =
            self.connections.drain().map(|(_, v)| v).collect();
        for mut conn in conns {
            // Decrement for the service's implicit ownership share.
            conn.ref_count -= 1;
            if conn.ref_count > 0 {
                // External references remain; leak and let unref free it.
                let _ = Box::into_raw(conn);
            }
            // else: drop normally
        }
    }
}

// ── Static event-loop callbacks ───────────────────────────────────────────────

unsafe extern "C" fn accept_callback(_fd: i32, _revents: i32, data: *mut libc::c_void) -> i32 {
    let service = &mut *(data as *mut QbIpcsService);
    match service.accept_connection() {
        Ok(conn_fd) => {
            if let Some(ref ph) = service.poll_handlers {
                if let Some(add) = ph.dispatch_add {
                    add(
                        crate::event_loop::QbLoopPriority::Med,
                        conn_fd,
                        libc::POLLIN as i32,
                        data,
                        Some(dispatch_callback),
                    );
                }
            }
        }
        Err(_) => {}
    }
    0
}

unsafe extern "C" fn dispatch_callback(fd: i32, _revents: i32, data: *mut libc::c_void) -> i32 {
    let service = &mut *(data as *mut QbIpcsService);
    let _ = service.dispatch_connection(fd);
    0
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn peer_credentials(fd: RawFd) -> QbResult<(libc::pid_t, libc::uid_t, libc::gid_t)> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r < 0 {
        return Err(QbError::from_last_os_error());
    }
    Ok((cred.pid, cred.uid, cred.gid))
}

#[cfg(not(target_os = "linux"))]
fn peer_credentials(fd: RawFd) -> QbResult<(libc::pid_t, libc::uid_t, libc::gid_t)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if r < 0 {
        return Err(QbError::from_last_os_error());
    }
    // pid is not available from the socket on FreeBSD; caller reads it from the auth message.
    Ok((0, uid, gid))
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// # Safety
/// `name` must be a valid null-terminated C string.  `handlers` must be valid.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_create(
    name: *const libc::c_char,
    service_id: u32,
    ipc_type: QbIpcType,
    handlers: *const QbIpcsServiceHandlers,
) -> *mut QbIpcsService {
    if name.is_null() || handlers.is_null() {
        crate::error::set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    let name_str = match std::ffi::CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => {
            crate::error::set_errno(libc::EINVAL);
            return ptr::null_mut();
        }
    };
    let resolved_type = if ipc_type == QbIpcType::Native {
        #[cfg(target_os = "linux")]
        { QbIpcType::Shm }
        #[cfg(not(target_os = "linux"))]
        { QbIpcType::Socket }
    } else {
        ipc_type
    };
    let h = QbIpcsServiceHandlers {
        connection_accept: (*handlers).connection_accept,
        connection_created: (*handlers).connection_created,
        msg_process: (*handlers).msg_process,
        connection_closed: (*handlers).connection_closed,
        connection_destroyed: (*handlers).connection_destroyed,
    };
    match QbIpcsService::create(name_str, service_id, resolved_type, h) {
        Ok(s) => Box::into_raw(s),
        Err(e) => {
            crate::error::set_errno(e.errno);
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_destroy(s: *mut QbIpcsService) {
    if !s.is_null() {
        drop(Box::from_raw(s));
    }
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_ref(s: *mut QbIpcsService) {
    if !s.is_null() { (*s).ref_count += 1; }
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_unref(s: *mut QbIpcsService) {
    if !s.is_null() {
        (*s).ref_count -= 1;
        if (*s).ref_count <= 0 {
            drop(Box::from_raw(s));
        }
    }
}

/// # Safety
/// `s` and `handlers` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_poll_handlers_set(
    s: *mut QbIpcsService,
    handlers: *const QbIpcsPollHandlers,
) {
    if s.is_null() || handlers.is_null() { return; }
    (*s).poll_handlers = Some(QbIpcsPollHandlers {
        job_add: (*handlers).job_add,
        dispatch_add: (*handlers).dispatch_add,
        dispatch_mod: (*handlers).dispatch_mod,
        dispatch_del: (*handlers).dispatch_del,
    });
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_service_context_set(
    s: *mut QbIpcsService,
    context: *mut libc::c_void,
) {
    if !s.is_null() { (*s).context = context; }
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_service_context_get(s: *const QbIpcsService) -> *mut libc::c_void {
    if s.is_null() { return ptr::null_mut(); }
    (*s).context
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_enforce_buffer_size(
    s: *mut QbIpcsService,
    max_buf_size: u32,
) -> i32 {
    if s.is_null() { return -libc::EINVAL; }
    (*s).max_buf_size = max_buf_size;
    0
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_request_rate_limit(
    s: *mut QbIpcsService,
    rl: QbIpcsRateLimit,
) {
    if !s.is_null() { (*s).rate_limit = rl; }
}

/// # Safety
/// `c` and `data` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_response_send(
    c: *mut QbIpcsConnection,
    data: *const libc::c_void,
    size: libc::size_t,
) -> libc::ssize_t {
    if c.is_null() || data.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let buf = std::slice::from_raw_parts(data as *const u8, size);
    match (*c).send_response(buf) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// # Safety
/// `c` and `iov` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_response_sendv(
    c: *mut QbIpcsConnection,
    iov: *const libc::iovec,
    iov_len: libc::size_t,
) -> libc::ssize_t {
    if c.is_null() || iov.is_null() { return -libc::EINVAL as libc::ssize_t; }
    match (*c).send_response_iov(iov, iov_len) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// # Safety
/// `c` and `data` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_event_send(
    c: *mut QbIpcsConnection,
    data: *const libc::c_void,
    size: libc::size_t,
) -> libc::ssize_t {
    if c.is_null() || data.is_null() { return -libc::EINVAL as libc::ssize_t; }
    // Check size BEFORE creating the slice to avoid UB.
    if size > (*c).max_msg_size {
        return -libc::EMSGSIZE as libc::ssize_t;
    }
    let buf = std::slice::from_raw_parts(data as *const u8, size);
    match (*c).send_event(buf) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_ref(c: *mut QbIpcsConnection) {
    if !c.is_null() { (*c).ref_count += 1; }
}

/// # Safety
/// `c` must be a valid pointer.  After this call, if ref_count reaches 0
/// the connection is freed and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_unref(c: *mut QbIpcsConnection) {
    if c.is_null() { return; }
    (*c).ref_count -= 1;
    if (*c).ref_count <= 0 {
        drop(Box::from_raw(c));
    }
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_context_set(
    c: *mut QbIpcsConnection,
    context: *mut libc::c_void,
) {
    if !c.is_null() { (*c).context = context; }
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_context_get(c: *const QbIpcsConnection) -> *mut libc::c_void {
    if c.is_null() { return ptr::null_mut(); }
    (*c).context
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_service_context_get(
    c: *const QbIpcsConnection,
) -> *mut libc::c_void {
    if c.is_null() { return ptr::null_mut(); }
    (*c).service_context
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_get_buffer_size(c: *const QbIpcsConnection) -> i32 {
    if c.is_null() { return -libc::EINVAL; }
    (*c).max_msg_size as i32
}

/// # Safety
/// `s` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_first_get(
    s: *mut QbIpcsService,
) -> *mut QbIpcsConnection {
    if s.is_null() { return ptr::null_mut(); }
    let c = (*s).connection_first().unwrap_or(ptr::null_mut());
    if !c.is_null() { (*c).ref_count += 1; }
    c
}

/// # Safety
/// `s` and `current` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_next_get(
    s: *mut QbIpcsService,
    current: *mut QbIpcsConnection,
) -> *mut QbIpcsConnection {
    if s.is_null() || current.is_null() { return ptr::null_mut(); }
    let c = (*s).connection_next(current).unwrap_or(ptr::null_mut());
    if !c.is_null() { (*c).ref_count += 1; }
    c
}

/// # Safety
/// `s` and `stats_out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_stats_get(
    s: *mut QbIpcsService,
    stats_out: *mut QbIpcsStats,
    clear: i32,
) -> i32 {
    if s.is_null() || stats_out.is_null() { return -libc::EINVAL; }
    let st = (*s).stats_get(clear != 0);
    *stats_out = st;
    0
}

/// Register the listener socket with the external event loop.
///
/// # Safety
/// `s` must be a valid pointer from `qb_ipcs_create`.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_run(s: *mut QbIpcsService) -> i32 {
    if s.is_null() { return -libc::EINVAL; }
    let fd = (*s).listener_fd();
    if let Some(ref ph) = (*s).poll_handlers {
        if let Some(add) = ph.dispatch_add {
            let r = add(
                crate::event_loop::QbLoopPriority::Med,
                fd,
                libc::POLLIN as i32,
                s as *mut libc::c_void,
                Some(accept_callback),
            );
            if r < 0 { return r; }
        }
    }
    0
}

/// Initiate a server-side disconnect by shutting down the connection stream.
///
/// The event loop will then detect HUP/EOF and call close_connection, which
/// invokes the connection_closed and connection_destroyed callbacks.
///
/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_disconnect(c: *mut QbIpcsConnection) {
    if c.is_null() { return; }
    libc::shutdown((*c).stream.as_raw_fd(), libc::SHUT_RDWR);
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_service_id_get(c: *const QbIpcsConnection) -> i32 {
    if c.is_null() { return -libc::EINVAL; }
    (*c).service_id
}

/// # Safety
/// `c` and `iov` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_event_sendv(
    c: *mut QbIpcsConnection,
    iov: *const libc::iovec,
    iov_len: i32,
) -> i32 {
    if c.is_null() || iov.is_null() || iov_len <= 0 {
        return -libc::EINVAL;
    }
    let mut buf: Vec<u8> = Vec::new();
    for i in 0..iov_len as usize {
        let iov_entry = &*iov.add(i);
        if !iov_entry.iov_base.is_null() && iov_entry.iov_len > 0 {
            let slice = std::slice::from_raw_parts(
                iov_entry.iov_base as *const u8,
                iov_entry.iov_len,
            );
            buf.extend_from_slice(slice);
        }
    }
    match (*c).send_event(&buf) {
        Ok(n) => n as i32,
        Err(e) => -(e.errno),
    }
}

/// # Safety
/// `c` and `stats_out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_stats_get(
    c: *mut QbIpcsConnection,
    stats_out: *mut QbIpcsConnectionStats,
    clear: i32,
) -> i32 {
    if c.is_null() || stats_out.is_null() { return -libc::EINVAL; }
    *stats_out = (*c).stats;
    if clear != 0 {
        (*c).stats_clear();
    }
    0
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_stats_get_2(
    c: *mut QbIpcsConnection,
    clear: i32,
) -> *mut QbIpcsConnectionStats2 {
    if c.is_null() {
        crate::error::set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    let stats = (*c).stats_get_2();
    if clear != 0 {
        (*c).stats_clear();
    }
    Box::into_raw(stats)
}

/// # Safety
/// `conn` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcs_connection_auth_set(
    conn: *mut QbIpcsConnection,
    uid: libc::uid_t,
    gid: libc::gid_t,
    mode: libc::mode_t,
) {
    if !conn.is_null() {
        (*conn).auth_set(uid, gid, mode);
    }
}
