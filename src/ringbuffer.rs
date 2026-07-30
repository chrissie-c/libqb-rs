//! Shared-memory ring buffer.
//!
//! Uses the "double-map" trick: the data area is mapped twice consecutively so
//! that reads and writes spanning the wrap point need no special-case handling.
//!
//! Chunk layout (8-byte aligned):
//!   [u32: data_len][u32: magic=0xDEADBEEF][data_len bytes][padding to 8-byte boundary]
//!
//! The write and read pointers are monotonically increasing u64 values; the
//! actual offset into the data area is `ptr % data_size`.

use crate::error::{QbError, QbResult};
use std::ffi::CStr;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const QB_RB_FLAG_CREATE: u32 = 0x01;
pub const QB_RB_FLAG_OVERWRITE: u32 = 0x02;
pub const QB_RB_FLAG_SHARED_THREAD: u32 = 0x04;
pub const QB_RB_FLAG_SHARED_PROCESS: u32 = 0x08;
pub const QB_RB_FLAG_NO_SEMAPHORE: u32 = 0x10;

const MAGIC_HEADER: u32 = 0x5EBC_5EBC;
const MAGIC_CHUNK: u32 = 0xDEAD_BEEF;
const VERSION: u32 = 1;
const CHUNK_HDR_SIZE: usize = 8; // sizeof(u32) * 2
#[cfg(target_os = "linux")]
const SHM_DIR: &str = "/dev/shm";
#[cfg(not(target_os = "linux"))]
const SHM_DIR: &str = "/tmp";

// ── Shared-memory header (lives at offset 0 of the shm file) ─────────────────

#[repr(C)]
struct RbHeader {
    magic: u32,
    version: u32,
    flags: u32,
    _pad0: u32,
    data_size: u64,         // usable ring buffer data bytes
    user_data_size: u32,
    _pad1: u32,
    ref_count: AtomicI32,
    _pad2: u32,
    write_pt: AtomicU64,    // monotonically increasing write pointer
    read_pt: AtomicU64,     // monotonically increasing read pointer
    name: [u8; 256],
}

const HEADER_SIZE: usize = std::mem::size_of::<RbHeader>();

// ── Chunk header ──────────────────────────────────────────────────────────────

#[repr(C)]
struct ChunkHdr {
    data_len: u32,
    magic: u32,
}

#[inline]
fn chunk_total_size(data_len: usize) -> usize {
    // Round (CHUNK_HDR_SIZE + data_len) up to the nearest 8 bytes.
    (CHUNK_HDR_SIZE + data_len + 7) & !7
}

// ── POSIX named semaphore wrapper ─────────────────────────────────────────────

struct Sem {
    ptr: *mut libc::sem_t,
    name: String,
    owner: bool, // true = we created and must unlink on drop
}

unsafe impl Send for Sem {}
unsafe impl Sync for Sem {}

impl Sem {
    fn create(name: &str) -> QbResult<Self> {
        let cname = std::ffi::CString::new(name).map_err(|_| QbError::new(libc::EINVAL))?;
        let ptr = unsafe {
            libc::sem_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_EXCL,
                0o600u32 as libc::c_uint,
                0u32,
            )
        };
        if std::ptr::eq(ptr, libc::SEM_FAILED) {
            return Err(QbError::from_last_os_error());
        }
        Ok(Sem {
            ptr,
            name: name.to_string(),
            owner: true,
        })
    }

    fn open(name: &str) -> QbResult<Self> {
        let cname = std::ffi::CString::new(name).map_err(|_| QbError::new(libc::EINVAL))?;
        let ptr = unsafe { libc::sem_open(cname.as_ptr(), 0) };
        if std::ptr::eq(ptr, libc::SEM_FAILED) {
            return Err(QbError::from_last_os_error());
        }
        Ok(Sem {
            ptr,
            name: name.to_string(),
            owner: false,
        })
    }

    fn post(&self) {
        unsafe { libc::sem_post(self.ptr) };
    }

    fn wait_timeout_ms(&self, ms: i32) -> QbResult<()> {
        if ms < 0 {
            // Block forever.
            let r = unsafe { libc::sem_wait(self.ptr) };
            if r == 0 {
                return Ok(());
            }
            return Err(QbError::from_last_os_error());
        }
        if ms == 0 {
            let r = unsafe { libc::sem_trywait(self.ptr) };
            if r == 0 {
                return Ok(());
            }
            let e = QbError::from_last_os_error();
            return Err(if e.errno == libc::EAGAIN {
                QbError::new(libc::EAGAIN)
            } else {
                e
            });
        }
        // Timed wait.
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        ts.tv_sec += (ms / 1000) as libc::time_t;
        ts.tv_nsec += (ms % 1000) as libc::c_long * 1_000_000;
        if ts.tv_nsec >= 1_000_000_000 {
            ts.tv_sec += 1;
            ts.tv_nsec -= 1_000_000_000;
        }
        let r = unsafe { libc::sem_timedwait(self.ptr, &ts) };
        if r == 0 {
            Ok(())
        } else {
            Err(QbError::from_last_os_error())
        }
    }
}

impl Drop for Sem {
    fn drop(&mut self) {
        unsafe { libc::sem_close(self.ptr) };
        if self.owner {
            let cname = std::ffi::CString::new(self.name.as_str()).unwrap();
            unsafe { libc::sem_unlink(cname.as_ptr()) };
        }
    }
}

// ── Ring buffer ───────────────────────────────────────────────────────────────

/// Shared-memory ring buffer handle.
pub struct QbRingbuffer {
    /// Pointer to the header in shared memory.
    header: *mut RbHeader,
    /// Base of the double-mapped data area.
    data: *mut u8,
    /// Pointer to the user data area (immediately after the header).
    user_data: *mut libc::c_void,
    /// File descriptor for the shm file.
    fd: libc::c_int,
    /// Total size of the shm file.
    #[allow(dead_code)]
    file_size: usize,
    /// Flags originally passed to open.
    flags: u32,
    /// Name (without path prefix).
    name: String,
    /// Optional semaphore.
    sem: Option<Sem>,
}

unsafe impl Send for QbRingbuffer {}
unsafe impl Sync for QbRingbuffer {}

impl QbRingbuffer {
    /// Open (or create) a named ring buffer.
    pub fn open(name: &str, size: usize, flags: u32, user_data_size: usize) -> QbResult<Box<Self>> {
        if size == 0 || name.is_empty() {
            return Err(QbError::new(libc::EINVAL));
        }
        // Align data_size to a page boundary.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let data_size = (size + page - 1) & !(page - 1);
        let user_data_size_aligned = (user_data_size + 7) & !7;
        // mmap() requires the file offset to be page-aligned.  Pad the header
        // section to the next page boundary so the data region starts at a
        // page-aligned offset inside the file.
        let header_raw = HEADER_SIZE + user_data_size_aligned;
        let data_file_offset = (header_raw + page - 1) & !(page - 1);
        let file_size = data_file_offset + data_size;

        let shm_path = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("{}/{}", SHM_DIR, name)
        };
        let cpath = std::ffi::CString::new(shm_path.clone())
            .map_err(|_| QbError::new(libc::EINVAL))?;

        let (fd, creating) = if flags & QB_RB_FLAG_CREATE != 0 {
            let mut fd = unsafe {
                libc::open(
                    cpath.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600u32 as libc::c_uint,
                )
            };
            if fd < 0 && unsafe { crate::error::get_errno() } == libc::EEXIST {
                // Stale file from a crashed previous instance — unlink and retry.
                unsafe { libc::unlink(cpath.as_ptr()) };
                let sem_name = std::ffi::CString::new(format!("/qbrb-{}", name)).unwrap();
                unsafe { libc::sem_unlink(sem_name.as_ptr()) };
                fd = unsafe {
                    libc::open(
                        cpath.as_ptr(),
                        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                        0o600u32 as libc::c_uint,
                    )
                };
            }
            if fd < 0 {
                return Err(QbError::from_last_os_error());
            }
            (fd, true)
        } else {
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
            if fd < 0 {
                return Err(QbError::from_last_os_error());
            }
            (fd, false)
        };

        if creating {
            // Size the file.
            if unsafe { libc::ftruncate(fd, file_size as libc::off_t) } < 0 {
                unsafe { libc::close(fd) };
                unsafe { libc::unlink(cpath.as_ptr()) };
                return Err(QbError::from_last_os_error());
            }
        }

        // Map header + user data area (offset 0, length = raw header bytes).
        let header = unsafe {
            libc::mmap(
                ptr::null_mut(),
                header_raw,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if header == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            if creating {
                unsafe { libc::unlink(cpath.as_ptr()) };
            }
            return Err(QbError::from_last_os_error());
        }

        let hdr_ptr = header as *mut RbHeader;
        let user_data_ptr = unsafe { (header as *mut u8).add(HEADER_SIZE) as *mut libc::c_void };

        // When opening an existing ring buffer, use the data_size that was
        // written to the header at creation time — the caller's `size` argument
        // may differ (e.g. client was told max_msg_size, not rb_size).
        let data_size = if creating {
            data_size
        } else {
            unsafe { (*hdr_ptr).data_size as usize }
        };

        // Double-map the data area so chunks that wrap around need no special handling.
        // data_file_offset is page-aligned (required by mmap for file-backed mappings).
        let double_map = unsafe {
            // Reserve virtual address space for 2× data_size.
            let base = libc::mmap(
                ptr::null_mut(),
                data_size * 2,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if base == libc::MAP_FAILED {
                libc::munmap(header, header_raw);
                libc::close(fd);
                if creating {
                    libc::unlink(cpath.as_ptr());
                }
                return Err(QbError::from_last_os_error());
            }
            // First copy.
            let m1 = libc::mmap(
                base,
                data_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                data_file_offset as libc::off_t,
            );
            // Second copy (mirror).
            let m2 = libc::mmap(
                (base as *mut u8).add(data_size) as *mut libc::c_void,
                data_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                data_file_offset as libc::off_t,
            );
            if m1 == libc::MAP_FAILED || m2 == libc::MAP_FAILED {
                libc::munmap(base, data_size * 2);
                libc::munmap(header, header_raw);
                libc::close(fd);
                if creating {
                    libc::unlink(cpath.as_ptr());
                }
                return Err(QbError::from_last_os_error());
            }
            base as *mut u8
        };

        if creating {
            // Initialise the header.
            unsafe {
                (*hdr_ptr).magic = MAGIC_HEADER;
                (*hdr_ptr).version = VERSION;
                (*hdr_ptr).flags = flags;
                (*hdr_ptr).data_size = data_size as u64;
                (*hdr_ptr).user_data_size = user_data_size as u32;
                (*hdr_ptr).ref_count = AtomicI32::new(1);
                (*hdr_ptr).write_pt = AtomicU64::new(0);
                (*hdr_ptr).read_pt = AtomicU64::new(0);
                let name_bytes = name.as_bytes();
                let copy_len = name_bytes.len().min(255);
                let name_dst = (*hdr_ptr).name.as_mut_ptr();
                std::ptr::copy_nonoverlapping(name_bytes.as_ptr(), name_dst, copy_len);
                *name_dst.add(copy_len) = 0;
            }
        } else {
            // Sanity-check existing header.
            unsafe {
                if (*hdr_ptr).magic != MAGIC_HEADER || (*hdr_ptr).version != VERSION {
                    libc::munmap(double_map as *mut libc::c_void, data_size * 2);
                    libc::munmap(header, header_raw);
                    libc::close(fd);
                    return Err(QbError::new(libc::EINVAL));
                }
                (*hdr_ptr).ref_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Semaphore (unless NO_SEMAPHORE flag is set).
        let sem = if flags & QB_RB_FLAG_NO_SEMAPHORE == 0 {
            let sem_name = format!("/qbrb-{}", name);
            let sem = if creating {
                // Best-effort unlink in case a previous instance crashed.
                let cs = std::ffi::CString::new(sem_name.clone()).unwrap();
                unsafe { libc::sem_unlink(cs.as_ptr()) };
                Sem::create(&sem_name)?
            } else {
                Sem::open(&sem_name)?
            };
            Some(sem)
        } else {
            None
        };

        Ok(Box::new(QbRingbuffer {
            header: hdr_ptr,
            data: double_map,
            user_data: user_data_ptr,
            fd,
            file_size,
            flags,
            name: name.to_string(),
            sem,
        }))
    }

    #[inline]
    fn data_size(&self) -> u64 {
        unsafe { (*self.header).data_size }
    }

    #[inline]
    fn write_pt(&self) -> u64 {
        unsafe { (*self.header).write_pt.load(Ordering::Acquire) }
    }

    #[inline]
    fn read_pt(&self) -> u64 {
        unsafe { (*self.header).read_pt.load(Ordering::Acquire) }
    }

    pub fn space_free(&self) -> isize {
        let ds = self.data_size() as isize;
        let used = (self.write_pt() - self.read_pt()) as isize;
        ds - used
    }

    pub fn space_used(&self) -> isize {
        (self.write_pt() - self.read_pt()) as isize
    }

    pub fn chunks_used(&self) -> isize {
        // Walk the buffer and count chunks (O(n) but required by API).
        let mut count = 0isize;
        let ds = self.data_size();
        let mut rp = self.read_pt();
        let wp = self.write_pt();
        while rp < wp {
            let pos = (rp % ds) as usize;
            let hdr = unsafe { &*(self.data.add(pos) as *const ChunkHdr) };
            if hdr.magic != MAGIC_CHUNK {
                break;
            }
            rp += chunk_total_size(hdr.data_len as usize) as u64;
            count += 1;
        }
        count
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn shared_user_data(&self) -> *mut libc::c_void {
        self.user_data
    }

    pub fn ref_count(&self) -> i32 {
        unsafe { (*self.header).ref_count.load(Ordering::SeqCst) }
    }

    /// Write a chunk.  Returns the number of data bytes written or -errno.
    pub fn chunk_write(&mut self, data: &[u8]) -> QbResult<usize> {
        let total = chunk_total_size(data.len());
        self.ensure_space(total)?;

        let ds = self.data_size();
        let wp = self.write_pt();
        let pos = (wp % ds) as usize;

        // Write header.
        unsafe {
            let hdr = self.data.add(pos) as *mut ChunkHdr;
            (*hdr).data_len = data.len() as u32;
            (*hdr).magic = MAGIC_CHUNK;
            // Write data.
            ptr::copy_nonoverlapping(data.as_ptr(), self.data.add(pos + CHUNK_HDR_SIZE), data.len());
        }

        // Advance write pointer with a release fence so readers see the data.
        unsafe {
            (*self.header)
                .write_pt
                .fetch_add(total as u64, Ordering::Release);
        }

        if let Some(ref sem) = self.sem {
            sem.post();
        }

        Ok(data.len())
    }

    /// Reserve space for a chunk; returns a mutable slice for the data area.
    pub fn chunk_alloc(&mut self, len: usize) -> QbResult<*mut u8> {
        let total = chunk_total_size(len);
        self.ensure_space(total)?;

        let ds = self.data_size();
        let wp = self.write_pt();
        let pos = (wp % ds) as usize;

        unsafe {
            let hdr = self.data.add(pos) as *mut ChunkHdr;
            (*hdr).data_len = len as u32;
            (*hdr).magic = 0; // not yet committed
            Ok(self.data.add(pos + CHUNK_HDR_SIZE))
        }
    }

    /// Commit a previously allocated chunk.
    pub fn chunk_commit(&mut self, len: usize) -> QbResult<()> {
        let total = chunk_total_size(len);
        let ds = self.data_size();
        let wp = self.write_pt();
        let pos = (wp % ds) as usize;

        unsafe {
            let hdr = self.data.add(pos) as *mut ChunkHdr;
            (*hdr).magic = MAGIC_CHUNK;
            (*self.header)
                .write_pt
                .fetch_add(total as u64, Ordering::Release);
        }

        if let Some(ref sem) = self.sem {
            sem.post();
        }
        Ok(())
    }

    /// Peek at the oldest chunk.  Returns a pointer to the chunk data (within
    /// the shared memory region) and the data length.  Call `chunk_reclaim` to
    /// advance the read pointer.
    pub fn chunk_peek(&self, ms_timeout: i32) -> QbResult<(*const u8, usize)> {
        // Wait until data is available.
        if self.write_pt() == self.read_pt() {
            if let Some(ref sem) = self.sem {
                sem.wait_timeout_ms(ms_timeout)?;
            } else if ms_timeout == 0 {
                return Err(QbError::new(libc::EAGAIN));
            } else {
                // Spin for no-semaphore mode.
                let deadline = if ms_timeout < 0 {
                    u64::MAX
                } else {
                    crate::util::nano_current() + ms_timeout as u64 * 1_000_000
                };
                loop {
                    if self.write_pt() != self.read_pt() {
                        break;
                    }
                    if crate::util::nano_current() >= deadline {
                        return Err(QbError::new(libc::ETIMEDOUT));
                    }
                    std::hint::spin_loop();
                }
            }
        }

        let ds = self.data_size();
        let rp = self.read_pt();
        let pos = (rp % ds) as usize;

        let hdr = unsafe { &*(self.data.add(pos) as *const ChunkHdr) };
        if hdr.magic != MAGIC_CHUNK {
            return Err(QbError::new(libc::EILSEQ));
        }

        let data_ptr = unsafe { self.data.add(pos + CHUNK_HDR_SIZE) };
        Ok((data_ptr, hdr.data_len as usize))
    }

    /// Advance the read pointer past the oldest chunk.  Call after `chunk_peek`.
    pub fn chunk_reclaim(&mut self) {
        let ds = self.data_size();
        let rp = self.read_pt();
        let pos = (rp % ds) as usize;
        let hdr = unsafe { &*(self.data.add(pos) as *const ChunkHdr) };
        let total = chunk_total_size(hdr.data_len as usize) as u64;
        unsafe {
            (*self.header)
                .read_pt
                .fetch_add(total, Ordering::Release);
        }
    }

    /// Peek, copy to `buf`, and reclaim in one call.
    pub fn chunk_read(&mut self, buf: &mut [u8], ms_timeout: i32) -> QbResult<usize> {
        let (ptr, len) = self.chunk_peek(ms_timeout)?;
        if buf.len() < len {
            return Err(QbError::new(libc::ENOBUFS));
        }
        unsafe { ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr(), len) };
        self.chunk_reclaim();
        Ok(len)
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn ensure_space(&mut self, needed: usize) -> QbResult<()> {
        let ds = self.data_size() as usize;
        if needed > ds {
            return Err(QbError::new(libc::ENOSPC));
        }

        loop {
            let wp = self.write_pt();
            let rp = self.read_pt();
            let free = ds as i64 - (wp - rp) as i64;

            if free >= needed as i64 {
                return Ok(());
            }

            if self.flags & QB_RB_FLAG_OVERWRITE == 0 {
                return Err(QbError::new(libc::EAGAIN));
            }

            // Discard the oldest chunk.
            let pos = (rp % self.data_size()) as usize;
            let hdr = unsafe { &*(self.data.add(pos) as *const ChunkHdr) };
            if hdr.magic != MAGIC_CHUNK {
                // Corrupt buffer; reset.
                unsafe {
                    (*self.header).read_pt.store(wp, Ordering::Release);
                }
                return Ok(());
            }
            let skip = chunk_total_size(hdr.data_len as usize) as u64;
            unsafe {
                (*self.header)
                    .read_pt
                    .compare_exchange(rp, rp + skip, Ordering::SeqCst, Ordering::SeqCst)
                    .ok();
            }
        }
    }

    fn file_path(&self) -> String {
        if self.name.starts_with('/') { self.name.clone() } else { format!("{}/{}", SHM_DIR, self.name) }
    }

    pub fn chown(&self, uid: libc::uid_t, gid: libc::gid_t) -> QbResult<()> {
        let path = self.file_path();
        let cpath = std::ffi::CString::new(path).unwrap();
        let r = unsafe { libc::chown(cpath.as_ptr(), uid, gid) };
        if r == 0 { Ok(()) } else { Err(QbError::from_last_os_error()) }
    }

    pub fn chmod(&self, mode: libc::mode_t) -> QbResult<()> {
        let path = self.file_path();
        let cpath = std::ffi::CString::new(path).unwrap();
        let r = unsafe { libc::chmod(cpath.as_ptr(), mode) };
        if r == 0 { Ok(()) } else { Err(QbError::from_last_os_error()) }
    }

    /// Serialize the ring buffer contents to a file descriptor.
    pub fn write_to_file(&self, fd: libc::c_int) -> QbResult<usize> {
        let mut rp = self.read_pt();
        let wp = self.write_pt();
        let ds = self.data_size();
        let mut total = 0usize;

        while rp < wp {
            let pos = (rp % ds) as usize;
            let hdr = unsafe { &*(self.data.add(pos) as *const ChunkHdr) };
            if hdr.magic != MAGIC_CHUNK {
                break;
            }
            let chunk_size = chunk_total_size(hdr.data_len as usize);
            let slice = unsafe { std::slice::from_raw_parts(self.data.add(pos), chunk_size) };
            let written = unsafe { libc::write(fd, slice.as_ptr() as *const _, slice.len()) };
            if written < 0 {
                return Err(QbError::from_last_os_error());
            }
            total += written as usize;
            rp += chunk_size as u64;
        }
        Ok(total)
    }
}

impl Drop for QbRingbuffer {
    fn drop(&mut self) {
        let hdr = self.header;
        let _prev = unsafe { (*hdr).ref_count.fetch_sub(1, Ordering::SeqCst) };

        let header_map_size = unsafe {
            HEADER_SIZE + (((*hdr).user_data_size as usize + 7) & !7)
        };
        let data_size = unsafe { (*hdr).data_size } as usize;

        // Always attempt to unlink.  POSIX guarantees that unlink while other
        // processes hold open fds merely removes the directory entry; the
        // memory stays accessible to existing holders until the last fd is
        // closed.  This also handles the case where the peer exited without
        // running Drop (e.g. via exit(0)), leaving ref_count > 1 even though
        // no other process holds the file open.
        //
        // If unlink fails (e.g. fault injection returning EACCES), fall back to
        // truncate so the file is zeroed and can't be mistaken for a live ring
        // buffer.
        let path = if self.name.starts_with('/') { self.name.clone() } else { format!("{}/{}", SHM_DIR, self.name) };
        if let Ok(cp) = std::ffi::CString::new(path) {
            let r = unsafe { libc::unlink(cp.as_ptr()) };
            if r != 0 && unsafe { crate::error::get_errno() } != libc::ENOENT {
                unsafe { libc::truncate(cp.as_ptr(), 0) };
            }
        }

        // Drop semaphore before unmapping.
        self.sem = None;

        unsafe {
            libc::munmap(self.data as *mut libc::c_void, data_size * 2);
            libc::munmap(self.header as *mut libc::c_void, header_map_size);
            libc::close(self.fd);
        }
    }
}

// ── C API ─────────────────────────────────────────────────────────────────────

/// Open (or create) a named ring buffer.
///
/// `flags` is a bitmask of `QB_RB_FLAG_*` constants.
/// `shared_user_data_size` bytes of user data are reserved after the header.
///
/// Returns a pointer to the ring buffer handle, or null on failure (errno set).
///
/// # Safety
/// `name` must be a valid null-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_open(
    name: *const libc::c_char,
    size: libc::size_t,
    flags: u32,
    shared_user_data_size: libc::size_t,
) -> *mut QbRingbuffer {
    if name.is_null() {
        crate::error::set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    let name_str = match CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => {
            crate::error::set_errno(libc::EINVAL);
            return ptr::null_mut();
        }
    };
    match QbRingbuffer::open(name_str, size, flags, shared_user_data_size) {
        Ok(rb) => Box::into_raw(rb),
        Err(e) => {
            crate::error::set_errno(e.errno);
            ptr::null_mut()
        }
    }
}

/// Close the ring buffer.  The last caller destroys the shared memory.
///
/// # Safety
/// `rb` must be a valid pointer from `qb_rb_open`.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_close(rb: *mut QbRingbuffer) {
    if !rb.is_null() {
        drop(Box::from_raw(rb));
    }
}

/// Returns the name of the ring buffer (pointer into shared memory).
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_name_get(rb: *const QbRingbuffer) -> *const libc::c_char {
    if rb.is_null() {
        return ptr::null();
    }
    (*(*rb).header).name.as_ptr() as *const libc::c_char
}

/// Returns a pointer to the shared user data area.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_shared_user_data_get(rb: *const QbRingbuffer) -> *mut libc::c_void {
    if rb.is_null() {
        return ptr::null_mut();
    }
    (*rb).user_data
}

/// Returns the current reference count.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_refcount_get(rb: *const QbRingbuffer) -> i32 {
    if rb.is_null() {
        return 0;
    }
    (*rb).ref_count()
}

/// Returns the number of free bytes remaining.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_space_free(rb: *const QbRingbuffer) -> libc::ssize_t {
    if rb.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    (*rb).space_free() as libc::ssize_t
}

/// Returns the number of bytes currently stored (including chunk headers).
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_space_used(rb: *const QbRingbuffer) -> libc::ssize_t {
    if rb.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    (*rb).space_used() as libc::ssize_t
}

/// Returns the number of chunks currently stored.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunks_used(rb: *const QbRingbuffer) -> libc::ssize_t {
    if rb.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    (*rb).chunks_used() as libc::ssize_t
}

/// Write a chunk to the ring buffer.
///
/// Returns the number of data bytes written, or a negative errno value.
///
/// # Safety
/// `rb` and `data` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_write(
    rb: *mut QbRingbuffer,
    data: *const libc::c_void,
    len: libc::size_t,
) -> libc::ssize_t {
    if rb.is_null() || data.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    let slice = std::slice::from_raw_parts(data as *const u8, len);
    match (*rb).chunk_write(slice) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Allocate space for a chunk without writing data.  Fill the returned pointer
/// then call `qb_rb_chunk_commit`.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_alloc(
    rb: *mut QbRingbuffer,
    len: libc::size_t,
) -> *mut libc::c_void {
    if rb.is_null() {
        return ptr::null_mut();
    }
    match (*rb).chunk_alloc(len) {
        Ok(p) => p as *mut libc::c_void,
        Err(_) => ptr::null_mut(),
    }
}

/// Commit a chunk allocated with `qb_rb_chunk_alloc`.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_commit(rb: *mut QbRingbuffer, len: libc::size_t) -> i32 {
    if rb.is_null() {
        return -libc::EINVAL;
    }
    match (*rb).chunk_commit(len) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// Peek at the oldest chunk without consuming it.  Sets `*data_out` to point
/// directly into shared memory.  Must call `qb_rb_chunk_reclaim` when done.
///
/// `ms_timeout`: 0 = non-blocking, <0 = block forever, >0 = wait N ms.
///
/// Returns the data length, or a negative errno value.
///
/// # Safety
/// `rb` and `data_out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_peek(
    rb: *const QbRingbuffer,
    data_out: *mut *mut libc::c_void,
    ms_timeout: i32,
) -> libc::ssize_t {
    if rb.is_null() || data_out.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    match (*rb).chunk_peek(ms_timeout) {
        Ok((ptr, len)) => {
            *data_out = ptr as *mut libc::c_void;
            len as libc::ssize_t
        }
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Advance the read pointer past the oldest chunk (call after `qb_rb_chunk_peek`).
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_reclaim(rb: *mut QbRingbuffer) {
    if !rb.is_null() {
        (*rb).chunk_reclaim();
    }
}

/// Peek, copy data into `data_out`, then reclaim — all in one call.
///
/// # Safety
/// `rb` and `data_out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chunk_read(
    rb: *mut QbRingbuffer,
    data_out: *mut libc::c_void,
    len: libc::size_t,
    ms_timeout: i32,
) -> libc::ssize_t {
    if rb.is_null() || data_out.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    let buf = std::slice::from_raw_parts_mut(data_out as *mut u8, len);
    match (*rb).chunk_read(buf, ms_timeout) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Serialize ring buffer contents to a file descriptor.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_write_to_file(
    rb: *const QbRingbuffer,
    fd: i32,
) -> libc::ssize_t {
    if rb.is_null() {
        return -libc::EINVAL as libc::ssize_t;
    }
    match (*rb).write_to_file(fd) {
        Ok(n) => n as libc::ssize_t,
        Err(e) => -(e.errno) as libc::ssize_t,
    }
}

/// Change the owner of the shared memory file.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chown(
    rb: *const QbRingbuffer,
    owner: libc::uid_t,
    group: libc::gid_t,
) -> i32 {
    if rb.is_null() {
        return -libc::EINVAL;
    }
    match (*rb).chown(owner, group) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}

/// Change the permissions of the shared memory file.
///
/// # Safety
/// `rb` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn qb_rb_chmod(rb: *const QbRingbuffer, mode: libc::mode_t) -> i32 {
    if rb.is_null() {
        return -libc::EINVAL;
    }
    match (*rb).chmod(mode) {
        Ok(()) => 0,
        Err(e) => -(e.errno),
    }
}
