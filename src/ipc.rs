//! IPC wire types shared between client and server.

// ── Wire headers ──────────────────────────────────────────────────────────────

/// Header prepended to every request message (client → server).
#[repr(C, align(8))]
pub struct QbIpcRequestHeader {
    pub id: i32,
    pub size: i32,
}

/// Header prepended to every response message (server → client).
#[repr(C, align(8))]
pub struct QbIpcResponseHeader {
    pub id: i32,
    pub size: i32,
    pub error: i32,
    pub _pad: i32,
}

/// Transport type selector.
/// Values match C libqb's qb_ipc_type_t: QB_IPC_NATIVE=0, QB_IPC_SHM=1, QB_IPC_SOCKET=2.
#[repr(i32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QbIpcType {
    Native = 0,
    Shm = 1,
    Socket = 2,
}

/// Reserved message IDs.
pub const QB_IPC_MSG_NEW_MESSAGE: i32 = 0;
pub const QB_IPC_MSG_USER_START: i32 = 0;
pub const QB_IPC_MSG_AUTHENTICATE: i32 = -1;
pub const QB_IPC_MSG_NEW_EVENT_SOCK: i32 = -2;
pub const QB_IPC_MSG_DISCONNECT: i32 = -3;

/// Maximum length for ring-buffer name strings in the SHM connection response.
pub const SHM_PATH_LEN: usize = 256;

/// Extended auth response sent by the server when using SHM transport.
///
/// The client determines whether this follows the base response by inspecting
/// `QbIpcResponseHeader::size`.
#[repr(C)]
pub struct QbIpcShmConnectionResponse {
    pub hdr: QbIpcResponseHeader,
    pub connection_type: i32,
    pub max_msg_size: u32,
    pub connection: u64, // server-side connection pointer
    pub request: [u8; SHM_PATH_LEN],
    pub response: [u8; SHM_PATH_LEN],
    pub event: [u8; SHM_PATH_LEN],
}

/// Generate the three ring-buffer names for a SHM IPC connection.
/// Returns (request_name, response_name, event_name).
pub fn shm_rb_names(
    service_name: &str,
    server_pid: libc::pid_t,
    client_pid: libc::pid_t,
    sock_fd: i32,
) -> (String, String, String) {
    let base = format!("qb-{}-{}-{}-{}", server_pid, client_pid, sock_fd, service_name);
    (
        format!("{}-req", base),
        format!("{}-rsp", base),
        format!("{}-evt", base),
    )
}

pub fn socket_dir() -> String {
    std::env::var("TMPDIR")
        .or_else(|_| std::env::var("TEMP"))
        .unwrap_or_else(|_| "/tmp".to_string())
}

// ── Platform-aware socket helpers ─────────────────────────────────────────────
//
// On Linux: abstract namespace sockets (sun_path[0] = '\0') so no filesystem
// entry is created.  The abstract name is just `<service_name>`, matching the
// C libqb convention: "\0cmap" for the "cmap" service.
//
// On other platforms (FreeBSD, etc.): regular filesystem sockets under
// socket_dir().  Returns the path string so the caller can unlink it on drop.

use std::os::unix::net::{UnixListener, UnixStream};
use crate::error::{QbError, QbResult};

/// Bind a listening SOCK_STREAM socket for `name`.
/// Returns the listener and, for filesystem sockets, the path to unlink on drop.
pub fn unix_server_listen(name: &str) -> QbResult<(UnixListener, Option<String>)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::FromRawFd;
        // C libqb uses sizeof(struct sockaddr_un) = 110 as the addrlen for abstract
        // sockets (QB_SUN_LEN macro).  Linux matches abstract sockets by exact
        // address — same bytes AND same addrlen — so we must do the same.
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
            // sun_path[0] stays '\0' (abstract marker); name goes to sun_path[1..]
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
        // Must use sizeof(struct sockaddr_un) as addrlen to match C libqb (QB_SUN_LEN).
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
/// On non-Linux this becomes a filesystem path via unix_server_listen/unix_client_connect.
pub fn event_socket_name(service_name: &str, client_pid: libc::pid_t) -> String {
    format!("{}-{}-ev", service_name, client_pid)
}
