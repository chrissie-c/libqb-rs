//! IPC wire types shared between client and server.

// ── Wire headers ──────────────────────────────────────────────────────────────

/// Header prepended to every request message (client → server).
///
///
/// Matches C libqb's `struct qb_ipc_request_header` with per-field align(8):
///   int32_t id   __attribute__((aligned(8)));  // offset 0, +4 pad
///   int32_t size __attribute__((aligned(8)));  // offset 8, +4 pad
/// Total: 16 bytes.
#[repr(C, align(8))]
pub struct QbIpcRequestHeader {
    pub id: i32,
    _pad_id: i32,
    pub size: i32,
    _pad_size: i32,
}

impl QbIpcRequestHeader {
    pub fn new(id: i32, size: i32) -> Self {
        Self { id, _pad_id: 0, size, _pad_size: 0 }
    }
}

/// Header prepended to every response message (server → client).
///
/// Matches C libqb's `struct qb_ipc_response_header` with per-field align(8):
///   int32_t id    __attribute__((aligned(8)));  // offset 0, +4 pad
///   int32_t size  __attribute__((aligned(8)));  // offset 8, +4 pad
///   int32_t error __attribute__((aligned(8)));  // offset 16, +4 pad
/// Total: 24 bytes.
#[repr(C, align(8))]
pub struct QbIpcResponseHeader {
    pub id: i32,
    _pad_id: i32,
    pub size: i32,
    _pad_size: i32,
    pub error: i32,
    _pad_error: i32,
}

impl QbIpcResponseHeader {
    pub fn new(id: i32, size: i32, error: i32) -> Self {
        Self { id, _pad_id: 0, size, _pad_size: 0, error, _pad_error: 0 }
    }
}

/// Transport type selector.
/// Values match C libqb's qb_ipc_type_t: QB_IPC_SOCKET=0, QB_IPC_SHM=1.
#[repr(i32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QbIpcType {
    Socket = 0,
    Shm = 1,
    Native = 4,
}

/// Reserved message IDs.
pub const QB_IPC_MSG_NEW_MESSAGE: i32 = 0;
pub const QB_IPC_MSG_USER_START: i32 = 0;
pub const QB_IPC_MSG_AUTHENTICATE: i32 = -1;
pub const QB_IPC_MSG_NEW_EVENT_SOCK: i32 = -2;
pub const QB_IPC_MSG_DISCONNECT: i32 = -3;

/// PATH_MAX on Linux (matches C libqb's usage of PATH_MAX in connection response).
pub const PATH_MAX: usize = 4096;

/// Connection response sent by the server on the setup socket.
///
/// Matches C libqb's `struct qb_ipc_connection_response` exactly:
///   struct qb_ipc_response_header hdr;   // 24 bytes (offset 0)
///   int32_t connection_type;              // 4 bytes  (offset 24)
///   uint32_t max_msg_size;               // 4 bytes  (offset 28)
///   intptr_t connection;                 // 8 bytes  (offset 32)
///   char request[PATH_MAX];              // 4096 bytes (offset 40)
///   char response[PATH_MAX];             // 4096 bytes (offset 4136)
///   char event[PATH_MAX];               // 4096 bytes (offset 8232)
/// Total: 12328 bytes.
#[repr(C)]
pub struct QbIpcShmConnectionResponse {
    pub hdr: QbIpcResponseHeader,
    pub connection_type: i32,
    pub max_msg_size: u32,
    pub connection: u64,
    pub request: [u8; PATH_MAX],
    pub response: [u8; PATH_MAX],
    pub event: [u8; PATH_MAX],
}

impl QbIpcShmConnectionResponse {
    pub fn zeroed() -> Self {
        // SAFETY: all-zeros is valid for this struct (no references, no enums with invariants)
        unsafe { std::mem::zeroed() }
    }
}

pub fn socket_dir() -> String {
    std::env::var("TMPDIR")
        .or_else(|_| std::env::var("TEMP"))
        .unwrap_or_else(|_| "/tmp".to_string())
}

// ── Platform-aware socket helpers ─────────────────────────────────────────────

use std::os::unix::net::{UnixListener, UnixStream};
use crate::error::{QbError, QbResult};

/// Bind a listening SOCK_STREAM socket for `name`.
/// Returns the listener and, for filesystem sockets, the path to unlink on drop.
pub fn unix_server_listen(name: &str) -> QbResult<(UnixListener, Option<String>)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::FromRawFd;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 { return Err(QbError::from_last_os_error()); }
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as _;
            let name_b = name.as_bytes();
            if name_b.len() + 1 > addr.sun_path.len() {
                libc::close(fd);
                return Err(QbError::new(libc::ENAMETOOLONG));
            }
            let dst = (addr.sun_path.as_mut_ptr() as *mut u8).add(1);
            std::ptr::copy_nonoverlapping(name_b.as_ptr(), dst, name_b.len());
            let addrlen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, addrlen) < 0 {
                let e = QbError::from_last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if libc::listen(fd, 16) < 0 {
                let e = QbError::from_last_os_error();
                libc::close(fd);
                return Err(e);
            }
            Ok((UnixListener::from_raw_fd(fd), None))
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = format!("{}/{}.sock", socket_dir(), name);
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).map_err(QbError::from)?;
        Ok((l, Some(path)))
    }
}

/// Connect a SOCK_STREAM socket to the server for `name`.
pub fn unix_client_connect(name: &str) -> QbResult<UnixStream> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::FromRawFd;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 { return Err(QbError::from_last_os_error()); }
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as _;
            let name_b = name.as_bytes();
            if name_b.len() + 1 > addr.sun_path.len() {
                libc::close(fd);
                return Err(QbError::new(libc::ENAMETOOLONG));
            }
            let dst = (addr.sun_path.as_mut_ptr() as *mut u8).add(1);
            std::ptr::copy_nonoverlapping(name_b.as_ptr(), dst, name_b.len());
            let addrlen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addrlen) < 0 {
                let e = QbError::from_last_os_error();
                libc::close(fd);
                return Err(e);
            }
            Ok(UnixStream::from_raw_fd(fd))
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path = format!("{}/{}.sock", socket_dir(), name);
        UnixStream::connect(&path).map_err(QbError::from)
    }
}

/// Abstract name used for the per-connection event socket (socket transport).
pub fn event_socket_name(service_name: &str, client_pid: libc::pid_t) -> String {
    format!("{}-{}-ev", service_name, client_pid)
}
