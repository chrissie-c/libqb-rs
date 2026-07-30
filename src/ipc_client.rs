//! IPC client.
//!
//! The client connects to a server via a Unix domain socket.  After the
//! handshake the server may upgrade the data path to a shared-memory ring
//! buffer; the control channel (and fallback) remains the Unix socket.
//!
//! Two logical channels per connection:
//!   1. Request/response: `qb_ipcc_send` + `qb_ipcc_recv`
//!   2. Async events:     `qb_ipcc_event_recv`

use crate::error::{QbError, QbResult};
use crate::ipc::{
    unix_client_connect, event_socket_name,
    QbIpcRequestHeader, QbIpcResponseHeader, QbIpcShmConnectionResponse, QbIpcType,
    QB_IPC_MSG_AUTHENTICATE,
};
use crate::ringbuffer::{QbRingbuffer, QB_RB_FLAG_SHARED_PROCESS};
use std::os::unix::net::UnixStream;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::ptr;

// ── Connection ────────────────────────────────────────────────────────────────

pub struct QbIpccConnection {
    stream: UnixStream,
    event_stream: Option<UnixStream>,
    // SHM transport (populated when server responds with connection_type=Shm).
    shm_request_rb: Option<Box<QbRingbuffer>>,
    shm_response_rb: Option<Box<QbRingbuffer>>,
    shm_event_rb: Option<Box<QbRingbuffer>>,
    // Counts notification bytes consumed by recv() when rsp_rb was empty (event bytes
    // that arrived before the response).  event_recv() drains these first so it never
    // double-consumes bytes from the control socket.
    shm_pending_events: u32,
    ipc_type: QbIpcType,
    max_msg_size: usize,
    context: *mut libc::c_void,
    pid: libc::pid_t,
    uid: libc::uid_t,
    gid: libc::gid_t,
    service_name: String,
    flow_control_max: u32,
    /// False until `finish_handshake` completes; set by async connect path.
    handshake_done: bool,
}

unsafe impl Send for QbIpccConnection {}

impl QbIpccConnection {
    /// Connect synchronously (send auth + wait for response + set up event channel).
    pub fn connect(name: &str, max_msg_size: usize) -> QbResult<Box<Self>> {
        let stream = unix_client_connect(name)?;

        let (pid, uid, gid) = peer_credentials(stream.as_raw_fd())?;

        let mut conn = Box::new(QbIpccConnection {
            stream,
            event_stream: None,
            shm_request_rb: None,
            shm_response_rb: None,
            shm_event_rb: None,
            shm_pending_events: 0,
            ipc_type: QbIpcType::Socket,
            max_msg_size,
            context: ptr::null_mut(),
            pid,
            uid,
            gid,
            service_name: name.to_string(),
            flow_control_max: 1,
            handshake_done: false,
        });

        conn.send_auth(max_msg_size)?;
        conn.finish_handshake()?;

        Ok(conn)
    }

    /// Begin an async connection: connect to the service, send the auth
    /// request, and return the connection fd.  The caller adds the fd to their
    /// event loop; when it becomes readable they call `connect_continue`.
    pub fn connect_async(name: &str, max_msg_size: usize) -> QbResult<(Box<Self>, std::os::unix::io::RawFd)> {
        let stream = unix_client_connect(name)?;

        let fd = stream.as_raw_fd();
        let (pid, uid, gid) = peer_credentials(fd)?;

        let mut conn = Box::new(QbIpccConnection {
            stream,
            event_stream: None,
            shm_request_rb: None,
            shm_response_rb: None,
            shm_event_rb: None,
            shm_pending_events: 0,
            ipc_type: QbIpcType::Socket,
            max_msg_size,
            context: ptr::null_mut(),
            pid,
            uid,
            gid,
            service_name: name.to_string(),
            flow_control_max: 1,
            handshake_done: false,
        });

        conn.send_auth(max_msg_size)?;

        Ok((conn, fd))
    }

    /// Finish an async connection started by `connect_async`.  Must be called
    /// when the fd returned by `connect_async` becomes readable.
    ///
    /// On error the connection is invalid; the C wrapper drops it.
    pub fn connect_continue(&mut self) -> QbResult<()> {
        if self.handshake_done {
            return Ok(());
        }
        self.finish_handshake()
    }

    /// Phase 1: send the authentication request to the server.
    fn send_auth(&mut self, max_msg_size: usize) -> QbResult<()> {
        let hdr_size = std::mem::size_of::<QbIpcRequestHeader>();
        let req = QbIpcRequestHeader {
            id: QB_IPC_MSG_AUTHENTICATE,
            size: hdr_size as i32 + 8,  // +4 max_msg +4 pid
        };
        let req_bytes = unsafe {
            std::slice::from_raw_parts(&req as *const _ as *const u8, hdr_size)
        };
        let max_bytes = (max_msg_size as u32).to_le_bytes();
        let pid_bytes = (unsafe { libc::getpid() } as u32).to_le_bytes();

        self.stream.write_all(req_bytes).map_err(QbError::from)?;
        self.stream.write_all(&max_bytes).map_err(QbError::from)?;
        self.stream.write_all(&pid_bytes).map_err(QbError::from)?;
        Ok(())
    }

    /// Phase 2: read the server's auth response and set up the data channel.
    fn finish_handshake(&mut self) -> QbResult<()> {
        // Read the base response header first to learn total size.
        let hdr_size = std::mem::size_of::<QbIpcResponseHeader>();
        let mut hdr_buf = [0u8; std::mem::size_of::<QbIpcResponseHeader>()];
        self.stream.read_exact(&mut hdr_buf).map_err(QbError::from)?;
        let resp_hdr = unsafe { &*(hdr_buf.as_ptr() as *const QbIpcResponseHeader) };

        let total_size = resp_hdr.size as usize;
        if total_size < hdr_size {
            return Err(QbError::new(libc::EPROTO));
        }

        // Read the rest of the response.
        let remaining = total_size - hdr_size;
        let mut rest_buf = vec![0u8; remaining];
        if remaining > 0 {
            self.stream.read_exact(&mut rest_buf).map_err(QbError::from)?;
        }

        if resp_hdr.error != 0 {
            return Err(QbError::new(resp_hdr.error.abs()));
        }

        let shm_extra = std::mem::size_of::<QbIpcShmConnectionResponse>() - hdr_size;
        if remaining >= shm_extra {
            // SHM response: parse connection_type, max_msg_size, ring buffer names.
            let connection_type = i32::from_ne_bytes(rest_buf[0..4].try_into().unwrap());
            let server_max = u32::from_ne_bytes(rest_buf[4..8].try_into().unwrap());
            if server_max as usize > self.max_msg_size {
                self.max_msg_size = server_max as usize;
            }
            if connection_type == QbIpcType::Shm as i32 {
                // Extract ring buffer names from the fixed-size path fields.
                let path_len = crate::ipc::SHM_PATH_LEN;
                // Layout after hdr: i32 connection_type(4) + u32 max_msg_size(4) + u64 connection(8) = 16
                let name_off = 4 + 4 + 8;
                let req_bytes = &rest_buf[name_off..name_off + path_len];
                let rsp_bytes = &rest_buf[name_off + path_len..name_off + path_len * 2];
                let evt_bytes = &rest_buf[name_off + path_len * 2..name_off + path_len * 3];
                let cstr_name = |b: &[u8]| -> String {
                    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                    String::from_utf8_lossy(&b[..end]).into_owned()
                };
                let req_name = cstr_name(req_bytes);
                let rsp_name = cstr_name(rsp_bytes);
                let evt_name = cstr_name(evt_bytes);

                let rb_flags = QB_RB_FLAG_SHARED_PROCESS;
                self.shm_request_rb = Some(
                    QbRingbuffer::open(&req_name, self.max_msg_size, rb_flags, std::mem::size_of::<i32>())?
                );
                self.shm_response_rb = Some(
                    QbRingbuffer::open(&rsp_name, self.max_msg_size, rb_flags, 0)
                        .map_err(|e| { self.shm_request_rb = None; e })?
                );
                self.shm_event_rb = Some(
                    QbRingbuffer::open(&evt_name, self.max_msg_size, rb_flags, 0)
                        .map_err(|e| { self.shm_request_rb = None; self.shm_response_rb = None; e })?
                );
                self.ipc_type = QbIpcType::Shm;
            }
        } else {
            // Socket response: 4 extra bytes = server max_msg_size.
            if remaining >= 4 {
                let server_max = u32::from_le_bytes(rest_buf[0..4].try_into().unwrap());
                if server_max as usize > self.max_msg_size {
                    self.max_msg_size = server_max as usize;
                }
            }
            // Connect to the event socket that the server created for our pid.
            let ev_name = event_socket_name(&self.service_name, unsafe { libc::getpid() });
            if let Ok(s) = unix_client_connect(&ev_name) {
                self.event_stream = Some(s);
            }
        }

        self.handshake_done = true;
        Ok(())
    }

    pub fn disconnect(&self) {
        // The stream is closed on drop; shutdown gracefully.
        let req = QbIpcRequestHeader {
            id: crate::ipc::QB_IPC_MSG_DISCONNECT,
            size: std::mem::size_of::<QbIpcRequestHeader>() as i32,
        };
        // Use MSG_NOSIGNAL so that a dead server returns EPIPE instead of
        // delivering SIGPIPE to our process.
        let _ = unsafe {
            libc::send(
                self.stream.as_raw_fd(),
                &req as *const _ as *const libc::c_void,
                std::mem::size_of::<QbIpcRequestHeader>(),
                libc::MSG_NOSIGNAL,
            )
        };
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn is_connected(&self) -> bool {
        // Check connection status via MSG_PEEK on the raw fd.
        let mut buf = [0u8; 1];
        let r = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        // If recv returns 0, the peer closed the connection.
        // EAGAIN/EWOULDBLOCK means the connection is alive but no data.
        if r == 0 {
            return false;
        }
        if r < 0 {
            let e = io::Error::last_os_error();
            return e.kind() == io::ErrorKind::WouldBlock
                || e.raw_os_error() == Some(libc::EAGAIN);
        }
        true
    }

    /// Send a request.  `msg` must begin with a `QbIpcRequestHeader`.
    pub fn send(&mut self, msg: &[u8]) -> QbResult<usize> {
        if msg.len() > self.max_msg_size {
            return Err(QbError::new(libc::EMSGSIZE));
        }
        if self.ipc_type == QbIpcType::Shm {
            let rb = self.shm_request_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            rb.chunk_write(msg)?;
            // Signal server with 1 byte on the setup socket.
            let one: u8 = 1;
            let r = unsafe { libc::send(self.stream.as_raw_fd(), &one as *const _ as *const libc::c_void, 1, libc::MSG_NOSIGNAL) };
            if r < 0 { return Err(QbError::from_last_os_error()); }
            return Ok(msg.len());
        }
        let mut sent = 0;
        while sent < msg.len() {
            let r = unsafe {
                libc::send(self.stream.as_raw_fd(), msg[sent..].as_ptr() as *const libc::c_void, msg.len() - sent, libc::MSG_NOSIGNAL)
            };
            if r < 0 { return Err(QbError::from_last_os_error()); }
            sent += r as usize;
        }
        Ok(msg.len())
    }

    /// Send a scatter-gather request.
    pub fn sendv(&mut self, iov: &[libc::iovec]) -> QbResult<usize> {
        if self.ipc_type == QbIpcType::Shm {
            let total: usize = iov.iter().map(|i| i.iov_len).sum();
            if total > self.max_msg_size { return Err(QbError::new(libc::EMSGSIZE)); }
            let rb = self.shm_request_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            let ptr = rb.chunk_alloc(total)?;
            let mut off = 0;
            for iv in iov {
                unsafe { std::ptr::copy_nonoverlapping(iv.iov_base as *const u8, ptr.add(off), iv.iov_len) };
                off += iv.iov_len;
            }
            rb.chunk_commit(total)?;
            let one: u8 = 1;
            unsafe { libc::send(self.stream.as_raw_fd(), &one as *const _ as *const libc::c_void, 1, libc::MSG_NOSIGNAL) };
            return Ok(total);
        }
        let msg = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: iov.as_ptr() as *mut libc::iovec,
            msg_iovlen: iov.len() as _,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        let r = unsafe { libc::sendmsg(self.stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if r < 0 { Err(QbError::from_last_os_error()) } else { Ok(r as usize) }
    }

    /// Receive a response into `buf`.  Returns bytes read.
    pub fn recv(&mut self, buf: &mut [u8], ms_timeout: i32) -> QbResult<usize> {
        let hdr_size = std::mem::size_of::<QbIpcResponseHeader>();
        if buf.len() < hdr_size {
            return Err(QbError::new(libc::EINVAL));
        }

        if self.ipc_type == QbIpcType::Shm {
            // The server writes to rsp_rb then sends a 1-byte notification.  The
            // event channel shares the same control socket, so an event notification
            // can arrive before the response notification.  When that happens we
            // consume the event byte, find rsp_rb empty, and must keep waiting.
            // We track each such "orphaned" byte in shm_pending_events so that
            // event_recv() can drain evt_rb without waiting for another socket byte.
            let deadline_ms = if ms_timeout < 0 { i64::MAX } else { ms_timeout as i64 };
            let start = crate::util::nano_current();
            loop {
                let elapsed_ms = ((crate::util::nano_current() - start) / 1_000_000) as i64;
                let remaining_ms = if ms_timeout < 0 { -1i32 } else {
                    let r = deadline_ms - elapsed_ms;
                    if r <= 0 { return Err(QbError::new(libc::ETIMEDOUT)); }
                    r as i32
                };
                poll_fd(self.stream.as_raw_fd(), remaining_ms)?;
                let mut byte = [0u8; 1];
                unsafe { libc::recv(self.stream.as_raw_fd(), byte.as_mut_ptr() as *mut libc::c_void, 1, libc::MSG_DONTWAIT) };
                let rb = self.shm_response_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
                match rb.chunk_read(buf, 0) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.errno == libc::EAGAIN => {
                        // This was an event notification; save it for event_recv().
                        self.shm_pending_events += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        set_timeout(self.stream.as_raw_fd(), ms_timeout)?;
        let r = self.stream.read(&mut buf[..hdr_size]);
        match r {
            Ok(0) => return Err(QbError::new(libc::ENOTCONN)),
            Ok(n) if n < hdr_size => {
                set_timeout(self.stream.as_raw_fd(), -1)?;
                self.stream.read_exact(&mut buf[n..hdr_size]).map_err(QbError::from)?;
                set_timeout(self.stream.as_raw_fd(), ms_timeout)?;
            }
            Ok(_) => {}
            Err(e) => return Err(timeout_err(e, ms_timeout)),
        }

        let msg_size = {
            let hdr = unsafe { &*(buf.as_ptr() as *const QbIpcResponseHeader) };
            (hdr.size as usize).clamp(hdr_size, buf.len())
        };

        if msg_size > hdr_size {
            set_timeout(self.stream.as_raw_fd(), -1)?;
            self.stream.read_exact(&mut buf[hdr_size..msg_size]).map_err(QbError::from)?;
            set_timeout(self.stream.as_raw_fd(), ms_timeout)?;
        }

        Ok(msg_size)
    }

    /// Send a request then immediately receive the response.
    pub fn sendv_recv(
        &mut self,
        iov: &[libc::iovec],
        buf: &mut [u8],
        ms_timeout: i32,
    ) -> QbResult<usize> {
        self.sendv(iov)?;
        self.recv(buf, ms_timeout)
    }

    /// Receive an async event from the server.
    pub fn event_recv(&mut self, buf: &mut [u8], ms_timeout: i32) -> QbResult<usize> {
        let hdr_size = std::mem::size_of::<QbIpcResponseHeader>();
        if buf.len() < hdr_size {
            return Err(QbError::new(libc::EINVAL));
        }

        if self.ipc_type == QbIpcType::Shm {
            // If recv() consumed a notification byte that turned out to be for an
            // event (rsp_rb was empty), it incremented shm_pending_events.  Use
            // that credit before reading another byte from the control socket.
            if self.shm_pending_events == 0 {
                poll_fd(self.stream.as_raw_fd(), ms_timeout)?;
                let mut byte = [0u8; 1];
                unsafe { libc::recv(self.stream.as_raw_fd(), byte.as_mut_ptr() as *mut libc::c_void, 1, libc::MSG_DONTWAIT) };
            } else {
                self.shm_pending_events -= 1;
            }
            let rb = self.shm_event_rb.as_mut().ok_or_else(|| QbError::new(libc::ENOTCONN))?;
            return rb.chunk_read(buf, 0).map_err(|e| if e.errno == libc::EAGAIN { QbError::new(libc::ETIMEDOUT) } else { e });
        }

        let stream = match self.event_stream.as_mut() {
            Some(s) => s,
            None => return Err(QbError::new(libc::ENOTCONN)),
        };

        set_timeout(stream.as_raw_fd(), ms_timeout)?;
        stream.read_exact(&mut buf[..hdr_size]).map_err(|e| timeout_err(e, ms_timeout))?;

        let msg_size = {
            let hdr = unsafe { &*(buf.as_ptr() as *const QbIpcResponseHeader) };
            (hdr.size as usize).clamp(hdr_size, buf.len())
        };

        if msg_size > hdr_size {
            set_timeout(stream.as_raw_fd(), -1)?;
            stream.read_exact(&mut buf[hdr_size..msg_size]).map_err(QbError::from)?;
            set_timeout(stream.as_raw_fd(), ms_timeout)?;
        }

        Ok(msg_size)
    }

    pub fn auth_get(&self) -> (libc::pid_t, libc::uid_t, libc::gid_t) {
        (self.pid, self.uid, self.gid)
    }

    pub fn buffer_size(&self) -> i32 {
        self.max_msg_size as i32
    }

    pub fn fc_enable_max_set(&mut self, max: u32) {
        self.flow_control_max = max.clamp(1, 2);
    }

    pub fn context_set(&mut self, ctx: *mut libc::c_void) {
        self.context = ctx;
    }

    pub fn context_get(&self) -> *mut libc::c_void {
        self.context
    }
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
    Ok((unsafe { libc::getpid() }, uid, gid))
}

/// Poll a single fd for POLLIN with an optional timeout.
fn poll_fd(fd: RawFd, ms_timeout: i32) -> QbResult<()> {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let timeout = if ms_timeout < 0 { -1 } else { ms_timeout };
    let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
    if r < 0 {
        Err(QbError::from_last_os_error())
    } else if r == 0 {
        Err(QbError::new(libc::ETIMEDOUT))
    } else if pfd.revents & (libc::POLLHUP | libc::POLLNVAL) != 0 {
        Err(QbError::new(libc::ENOTCONN))
    } else {
        Ok(())
    }
}

/// On Linux, SO_RCVTIMEO expiry returns EAGAIN.  Callers that set a timeout
/// expect ETIMEDOUT, matching the original libqb behaviour.
fn timeout_err(e: io::Error, ms_timeout: i32) -> QbError {
    let errno = e.raw_os_error().unwrap_or(libc::EIO);
    if ms_timeout > 0 && (errno == libc::EAGAIN || errno == libc::EWOULDBLOCK) {
        QbError::new(libc::ETIMEDOUT)
    } else {
        QbError::new(errno)
    }
}

fn set_timeout(fd: RawFd, ms_timeout: i32) -> QbResult<()> {
    let tv = if ms_timeout < 0 {
        libc::timeval { tv_sec: 0, tv_usec: 0 } // no timeout
    } else if ms_timeout == 0 {
        // Non-blocking: set a very short timeout.
        libc::timeval { tv_sec: 0, tv_usec: 1 }
    } else {
        libc::timeval {
            tv_sec: (ms_timeout / 1000) as libc::time_t,
            tv_usec: ((ms_timeout % 1000) * 1000) as libc::suseconds_t,
        }
    };
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if r < 0 {
        Err(QbError::from_last_os_error())
    } else {
        Ok(())
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// Connect to an IPC service.
///
/// # Safety
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_connect(
    name: *const libc::c_char,
    max_msg_size: libc::size_t,
) -> *mut QbIpccConnection {
    if name.is_null() {
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
    match QbIpccConnection::connect(name_str, max_msg_size) {
        Ok(c) => Box::into_raw(c),
        Err(e) => {
            crate::error::set_errno(e.errno);
            ptr::null_mut()
        }
    }
}

/// Disconnect from the service and free the connection.
///
/// # Safety
/// `c` must be a valid pointer from `qb_ipcc_connect`.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_disconnect(c: *mut QbIpccConnection) {
    if !c.is_null() {
        (*c).disconnect();
        drop(Box::from_raw(c));
    }
}

/// # Safety
/// `c` and `fd` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_fd_get(
    c: *const QbIpccConnection,
    fd: *mut i32,
) -> i32 {
    if c.is_null() || fd.is_null() { return -libc::EINVAL; }
    // For SHM, the setup socket is used for all event notifications.
    // For socket transport, use the dedicated event socket if available.
    *fd = if (*c).ipc_type == QbIpcType::Shm {
        (*c).fd()
    } else {
        match (*c).event_stream.as_ref() {
            Some(s) => s.as_raw_fd(),
            None => (*c).fd(),
        }
    };
    0
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_is_connected(c: *const QbIpccConnection) -> i32 {
    if c.is_null() { return 0; }
    (*c).is_connected() as i32
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_get_buffer_size(c: *const QbIpccConnection) -> i32 {
    if c.is_null() { return -libc::EINVAL; }
    (*c).buffer_size()
}

/// # Safety
/// `c` and `pid`/`uid`/`gid` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_auth_get(
    c: *const QbIpccConnection,
    pid: *mut libc::pid_t,
    uid: *mut libc::uid_t,
    gid: *mut libc::gid_t,
) -> i32 {
    if c.is_null() { return -libc::EINVAL; }
    let (p, u, g) = (*c).auth_get();
    if !pid.is_null() { *pid = p; }
    if !uid.is_null() { *uid = u; }
    if !gid.is_null() { *gid = g; }
    0
}

/// Send a request to the server.
///
/// # Safety
/// `c` and `msg_ptr` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_send(
    c: *mut QbIpccConnection,
    msg_ptr: *const libc::c_void,
    msg_len: libc::size_t,
) -> libc::ssize_t {
    if c.is_null() || msg_ptr.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let buf = std::slice::from_raw_parts(msg_ptr as *const u8, msg_len);
    match (*c).send(buf) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// # Safety
/// `c` and `iov` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_sendv(
    c: *mut QbIpccConnection,
    iov: *const libc::iovec,
    iov_len: libc::size_t,
) -> libc::ssize_t {
    if c.is_null() || iov.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let iovs = std::slice::from_raw_parts(iov, iov_len);
    match (*c).sendv(iovs) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Receive a response.
///
/// # Safety
/// `c` and `msg_ptr` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_recv(
    c: *mut QbIpccConnection,
    msg_ptr: *mut libc::c_void,
    msg_len: libc::size_t,
    ms_timeout: i32,
) -> libc::ssize_t {
    if c.is_null() || msg_ptr.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let buf = std::slice::from_raw_parts_mut(msg_ptr as *mut u8, msg_len);
    match (*c).recv(buf, ms_timeout) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Send a request and receive the response in one call.
///
/// # Safety
/// All pointers must be valid.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_sendv_recv(
    c: *mut QbIpccConnection,
    iov: *const libc::iovec,
    iov_len: u32,
    msg_ptr: *mut libc::c_void,
    msg_len: libc::size_t,
    ms_timeout: i32,
) -> libc::ssize_t {
    if c.is_null() || iov.is_null() || msg_ptr.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    let iovs = std::slice::from_raw_parts(iov, iov_len as usize);
    let buf = std::slice::from_raw_parts_mut(msg_ptr as *mut u8, msg_len);
    match (*c).sendv_recv(iovs, buf, ms_timeout) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Receive an async event from the server.
///
/// # Safety
/// `c` and `msg_ptr` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_event_recv(
    c: *mut QbIpccConnection,
    msg_ptr: *mut libc::c_void,
    msg_len: libc::size_t,
    ms_timeout: i32,
) -> libc::ssize_t {
    if c.is_null() || msg_ptr.is_null() { return -libc::EINVAL as libc::ssize_t; }
    let buf = std::slice::from_raw_parts_mut(msg_ptr as *mut u8, msg_len);
    match (*c).event_recv(buf, ms_timeout) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_context_set(
    c: *mut QbIpccConnection,
    context: *mut libc::c_void,
) {
    if !c.is_null() { (*c).context_set(context); }
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_context_get(c: *const QbIpccConnection) -> *mut libc::c_void {
    if c.is_null() { return ptr::null_mut(); }
    (*c).context_get()
}

/// # Safety
/// `c` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_fc_enable_max_set(c: *mut QbIpccConnection, max: u32) -> i32 {
    if c.is_null() { return -libc::EINVAL; }
    (*c).fc_enable_max_set(max);
    0
}

/// Probe the kernel's maximum datagram size.
///
/// We always support the requested size (socket transport has no kernel
/// datagram limit), so return the requested size directly.
#[no_mangle]
pub extern "C" fn qb_ipcc_verify_dgram_max_msg_size(max_msg_size: libc::size_t) -> i32 {
    max_msg_size as i32
}

/// Begin an asynchronous connection to an IPC service.
///
/// Returns a connection handle and writes the setup fd into `*connect_fd`.
/// The caller adds the fd to their event loop; when it becomes readable
/// they call `qb_ipcc_connect_continue()` to finish the handshake.
///
/// On error returns NULL (errno set).  On success the connection is *not* yet
/// usable for sending/receiving — use `qb_ipcc_connect_continue` first.
///
/// # Safety
/// `name` must be a valid null-terminated C string.  `connect_fd` must be
/// a valid non-null pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_connect_async(
    name: *const libc::c_char,
    max_msg_size: libc::size_t,
    connect_fd: *mut i32,
) -> *mut QbIpccConnection {
    if name.is_null() || connect_fd.is_null() {
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
    match QbIpccConnection::connect_async(name_str, max_msg_size) {
        Ok((c, fd)) => {
            *connect_fd = fd;
            Box::into_raw(c)
        }
        Err(e) => {
            crate::error::set_errno(e.errno);
            ptr::null_mut()
        }
    }
}

/// Finish an asynchronous connection started by `qb_ipcc_connect_async()`.
///
/// Call this when the fd returned by `qb_ipcc_connect_async()` becomes
/// readable in the application's event loop.
///
/// Returns 0 on success.  On error the connection is freed and must not be
/// used again; the error value (positive errno) is returned as negative.
///
/// # Safety
/// `c` must be a valid pointer from `qb_ipcc_connect_async`.  After a
/// non-zero return the pointer is invalid.
#[no_mangle]
pub unsafe extern "C" fn qb_ipcc_connect_continue(
    c: *mut QbIpccConnection,
) -> i32 {
    if c.is_null() {
        return -libc::EINVAL;
    }
    let result = (*c).connect_continue();
    match result {
        Ok(()) => 0,
        Err(e) => {
            // Mirror original behaviour: free the connection on error.
            drop(Box::from_raw(c));
            -(e.errno)
        }
    }
}
