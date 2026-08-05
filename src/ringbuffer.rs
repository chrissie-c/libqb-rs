//! Shared-memory ring buffer compatible with C libqb's qb_ringbuffer_s.
//!
//! Uses TWO files per ring buffer (matching C libqb):
//!   {name}-header  — shared header (qb_ringbuffer_shared_s layout)
//!   {name}-data    — uint32_t data array
//!
//! Chunk layout (word-based, indices into the u32 data array):
//!   data[write_pt]            = chunk data length in bytes
//!   data[(write_pt+1)%ws]     = magic (0xA1A1A1A1 when committed)
//!   data[(write_pt+2)%ws..]   = chunk data bytes (word-padded)
//!
//! Write/read pointers are u32 word indices into the data array.

use crate::error::{QbError, QbResult};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

// ── Public flag constants (match C libqb) ─────────────────────────────────────

pub const QB_RB_FLAG_CREATE: u32 = 0x01;
pub const QB_RB_FLAG_OVERWRITE: u32 = 0x02;
pub const QB_RB_FLAG_SHARED_THREAD: u32 = 0x04;
pub const QB_RB_FLAG_SHARED_PROCESS: u32 = 0x08;
pub const QB_RB_FLAG_NO_SEMAPHORE: u32 = 0x10;

// ── Chunk magic values (match C libqb) ───────────────────────────────────────

const CHUNK_MAGIC: u32 = 0xA1A1A1A1;
const CHUNK_MAGIC_DEAD: u32 = 0xD0D0D0D0;

// ── Shared header (matches C's qb_ringbuffer_shared_s on Linux x86_64) ───────
//
// C layout:
//   uint32_t write_pt;         offset 0
//   uint32_t read_pt;          offset 4
//   uint32_t word_size;        offset 8
//   char hdr_path[4096];       offset 12
//   char data_path[4096];      offset 4108
//   int32_t ref_count;         offset 8204
//   sem_t posix_sem;           offset 8208  (32 bytes on Linux x86_64)
//   char user_data[1];         offset 8240
// sizeof = 8248 (align 8 padded)

#[repr(C)]
struct RbSharedHdr {
    write_pt: AtomicU32,
    read_pt: AtomicU32,
    word_size: u32,
    hdr_path: [u8; 4096],
    data_path: [u8; 4096],
    ref_count: AtomicI32,
    posix_sem: [u8; 32],
    // user_data follows immediately after this struct in the mapping
}

// sizeof(RbSharedHdr) == 8240; C's sizeof() == 8248 (includes user_data[1] + padding).
// User data starts at offset 8240 in both cases.
const HDR_STRUCT_SIZE: usize = std::mem::size_of::<RbSharedHdr>(); // should be 8240
const C_SIZEOF_SHARED_HDR: usize = 8248; // C's sizeof(qb_ringbuffer_shared_s)

// ── Ring buffer handle ────────────────────────────────────────────────────────

pub struct QbRingbuffer {
    /// Pointer to the shared header mapping.
    hdr: *mut RbSharedHdr,
    /// Pointer to the data array mapping.
    data: *mut u32,
    /// Size of the header mapping in bytes.
    hdr_map_size: usize,
    /// Size of the data mapping in bytes (= word_size * 4, page-rounded).
    data_map_size: usize,
    /// word_size (cached from header).
    word_size: u32,
    /// Path to the header file (used for cleanup).
    hdr_path: String,
    /// Path to the data file (used for cleanup).
    data_path: String,
    /// Whether we created (and own) these files.
    owner: bool,
}

unsafe impl Send for QbRingbuffer {}
unsafe impl Sync for QbRingbuffer {}

impl QbRingbuffer {
    /// Open (or create) a ring buffer at `name` (full absolute path prefix,
    /// without the `-header`/`-data` suffixes).
    ///
    /// `size` is the minimum usable data bytes.
    /// `flags` is a bitmask of QB_RB_FLAG_*.
    /// `user_data_size` is extra bytes at the end of the header file (e.g. 4 for FC flag).
    pub fn open(name: &str, size: usize, flags: u32, user_data_size: usize) -> QbResult<Box<Self>> {
        if size == 0 || name.is_empty() {
            return Err(QbError::new(libc::EINVAL));
        }

        let creating = flags & QB_RB_FLAG_CREATE != 0;
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;

        // Compute the word_size: round size up to a page, then convert to words.
        let real_size = (size + page - 1) & !(page - 1);
        let word_size = (real_size / 4) as u32;

        // Header file: C_SIZEOF_SHARED_HDR + user_data_size bytes.
        let hdr_map_size = C_SIZEOF_SHARED_HDR + user_data_size;
        // Data file: real_size bytes (word_size * 4, page-aligned).
        let data_map_size = real_size;

        let hdr_path = format!("{}-header", name);
        let data_path_str = format!("{}-data", name);

        let hdr_fd = open_mmap_file(&hdr_path, hdr_map_size, creating)?;
        let data_fd = match open_mmap_file(&data_path_str, data_map_size, creating) {
            Ok(fd) => fd,
            Err(e) => {
                unsafe { libc::close(hdr_fd) };
                if creating {
                    let cp = std::ffi::CString::new(hdr_path.clone()).unwrap();
                    unsafe { libc::unlink(cp.as_ptr()) };
                }
                return Err(e);
            }
        };

        // Map header.
        let hdr_map = unsafe {
            libc::mmap(ptr::null_mut(), hdr_map_size,
                       libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, hdr_fd, 0)
        };
        unsafe { libc::close(hdr_fd) };
        if hdr_map == libc::MAP_FAILED {
            unsafe { libc::close(data_fd) };
            if creating {
                let cp1 = std::ffi::CString::new(hdr_path.clone()).unwrap();
                let cp2 = std::ffi::CString::new(data_path_str.clone()).unwrap();
                unsafe { libc::unlink(cp1.as_ptr()); libc::unlink(cp2.as_ptr()) };
            }
            return Err(QbError::from_last_os_error());
        }

        // Map data.
        let data_map = unsafe {
            libc::mmap(ptr::null_mut(), data_map_size,
                       libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, data_fd, 0)
        };
        unsafe { libc::close(data_fd) };
        if data_map == libc::MAP_FAILED {
            unsafe { libc::munmap(hdr_map, hdr_map_size) };
            if creating {
                let cp1 = std::ffi::CString::new(hdr_path.clone()).unwrap();
                let cp2 = std::ffi::CString::new(data_path_str.clone()).unwrap();
                unsafe { libc::unlink(cp1.as_ptr()); libc::unlink(cp2.as_ptr()) };
            }
            return Err(QbError::from_last_os_error());
        }

        let hdr_ptr = hdr_map as *mut RbSharedHdr;
        let data_ptr = data_map as *mut u32;

        if creating {
            // Initialise the shared header.
            unsafe {
                (*hdr_ptr).write_pt = AtomicU32::new(0);
                (*hdr_ptr).read_pt = AtomicU32::new(0);
                (*hdr_ptr).word_size = word_size;
                // Store hdr_path into the header.
                copy_path_to_field(&mut (*hdr_ptr).hdr_path, &hdr_path);
                // Store data_path into the header.
                copy_path_to_field(&mut (*hdr_ptr).data_path, &data_path_str);
                (*hdr_ptr).ref_count = AtomicI32::new(1);
                // posix_sem: zero-initialize (equivalent to sem_init(sem, 1, 0) on Linux).
                (*hdr_ptr).posix_sem = [0u8; 32];
                // Zero the data area.
                ptr::write_bytes(data_ptr, 0, word_size as usize);
                // Sentinel word at word_size (matches C libqb: rb->shared_data[rb->shared_hdr->word_size] = 5).
                // We can't do this safely since word_size is the exact size, but C does it for debug.
                // Skip: our data file is exactly word_size words, no space for sentinel.
            }
        } else {
            unsafe {
                (*hdr_ptr).ref_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let ws = unsafe { (*hdr_ptr).word_size };

        Ok(Box::new(QbRingbuffer {
            hdr: hdr_ptr,
            data: data_ptr,
            hdr_map_size,
            data_map_size,
            word_size: ws,
            hdr_path,
            data_path: data_path_str,
            owner: creating,
        }))
    }

    // ── Pointer accessors ─────────────────────────────────────────────────────

    #[inline]
    fn write_pt(&self) -> u32 {
        unsafe { (*self.hdr).write_pt.load(Ordering::Acquire) }
    }

    #[inline]
    fn read_pt(&self) -> u32 {
        unsafe { (*self.hdr).read_pt.load(Ordering::Acquire) }
    }

    #[inline]
    fn word_at(&self, idx: u32) -> u32 {
        unsafe { *self.data.add((idx % self.word_size) as usize) }
    }

    #[inline]
    fn set_word_at(&self, idx: u32, val: u32) {
        unsafe { *self.data.add((idx % self.word_size) as usize) = val; }
    }

    // ── Space accounting ──────────────────────────────────────────────────────

    fn space_free_words(&self) -> u32 {
        let wp = self.write_pt();
        let rp = self.read_pt();
        let ws = self.word_size;
        if wp >= rp {
            (ws - (wp - rp)).saturating_sub(1)
        } else {
            (rp - wp).saturating_sub(1)
        }
    }

    pub fn chunks_used(&self) -> isize {
        let wp = self.write_pt();
        let rp = self.read_pt();
        if wp == rp {
            return 0;
        }
        // Check that the oldest chunk has the committed magic.
        let magic = self.word_at(rp.wrapping_add(1));
        if magic == CHUNK_MAGIC { 1 } else { 0 }
    }

    pub fn ref_count(&self) -> i32 {
        unsafe { (*self.hdr).ref_count.load(Ordering::SeqCst) }
    }

    pub fn shared_user_data(&self) -> *mut libc::c_void {
        // user_data starts at offset HDR_STRUCT_SIZE in the header mapping.
        unsafe { (self.hdr as *mut u8).add(HDR_STRUCT_SIZE) as *mut libc::c_void }
    }

    // ── Chunk step (matches C's qb_rb_chunk_step) ─────────────────────────────
    //   step = 2 (header words) + ceil(data_len / 4) (data words)

    fn chunk_step(&self, ptr: u32, data_len: u32) -> u32 {
        let words = (data_len / 4) + if data_len % 4 != 0 { 1 } else { 0 };
        (ptr.wrapping_add(2).wrapping_add(words)) % self.word_size
    }

    // ── Write a chunk ─────────────────────────────────────────────────────────

    pub fn chunk_write(&mut self, data: &[u8]) -> QbResult<usize> {
        let len = data.len() as u32;
        let words_needed = 2 + (len / 4) + if len % 4 != 0 { 1 } else { 0 };

        if words_needed >= self.word_size {
            return Err(QbError::new(libc::ENOSPC));
        }
        if self.space_free_words() < words_needed {
            return Err(QbError::new(libc::EAGAIN));
        }

        let wp = self.write_pt();
        // Write size word.
        self.set_word_at(wp, len);
        // Write data bytes (word-by-word with modular addressing).
        copy_bytes_to_ring(self.data, self.word_size, wp.wrapping_add(2), data);
        // Commit: set magic word (release fence so readers see data first).
        let magic_idx = (wp.wrapping_add(1)) % self.word_size;
        unsafe {
            let magic_ptr = self.data.add(magic_idx as usize) as *mut AtomicU32;
            (*magic_ptr).store(CHUNK_MAGIC, Ordering::Release);
        }
        // Advance write pointer.
        let new_wp = self.chunk_step(wp, len);
        unsafe { (*self.hdr).write_pt.store(new_wp, Ordering::Release) };

        Ok(data.len())
    }

    /// Allocate space for a chunk; returns pointer to data area.
    pub fn chunk_alloc(&mut self, len: usize) -> QbResult<*mut u8> {
        let len32 = len as u32;
        let words_needed = 2 + (len32 / 4) + if len32 % 4 != 0 { 1 } else { 0 };

        if words_needed >= self.word_size {
            return Err(QbError::new(libc::ENOSPC));
        }
        if self.space_free_words() < words_needed {
            return Err(QbError::new(libc::EAGAIN));
        }

        let wp = self.write_pt();
        self.set_word_at(wp, len32);
        // Magic = ALLOC (not yet committed).
        self.set_word_at(wp.wrapping_add(1), 0xA110CED0u32);

        // Return pointer to data start in the data array.
        let data_idx = (wp.wrapping_add(2)) % self.word_size;
        Ok(unsafe { self.data.add(data_idx as usize) as *mut u8 })
    }

    /// Commit a previously allocated chunk.
    pub fn chunk_commit(&mut self, len: usize) -> QbResult<()> {
        let len32 = len as u32;
        let wp = self.write_pt();
        // Set length word (in case chunk_alloc didn't get the final size).
        self.set_word_at(wp, len32);
        // Set magic (release).
        let magic_idx = wp.wrapping_add(1) % self.word_size;
        unsafe {
            let magic_ptr = self.data.add(magic_idx as usize) as *mut AtomicU32;
            (*magic_ptr).store(CHUNK_MAGIC, Ordering::Release);
        }
        let new_wp = self.chunk_step(wp, len32);
        unsafe { (*self.hdr).write_pt.store(new_wp, Ordering::Release) };
        Ok(())
    }

    // ── Read a chunk ──────────────────────────────────────────────────────────

    /// Try to read a committed chunk into `buf`.  Returns bytes read or error.
    /// With `ms_timeout = 0`, returns EAGAIN immediately if no data.
    pub fn chunk_read(&mut self, buf: &mut [u8], _ms_timeout: i32) -> QbResult<usize> {
        let wp = self.write_pt();
        let rp = self.read_pt();
        if wp == rp {
            return Err(QbError::new(libc::EAGAIN));
        }

        // Check magic (acquire fence).
        let magic_idx = rp.wrapping_add(1) % self.word_size;
        let magic = unsafe {
            let mp = self.data.add(magic_idx as usize) as *const AtomicU32;
            (*mp).load(Ordering::Acquire)
        };
        if magic != CHUNK_MAGIC {
            return Err(QbError::new(libc::EAGAIN));
        }

        let chunk_len = self.word_at(rp) as usize;
        if buf.len() < chunk_len {
            return Err(QbError::new(libc::ENOBUFS));
        }

        // Copy data from ring (with modular addressing).
        let data_start = rp.wrapping_add(2) % self.word_size;
        copy_bytes_from_ring(self.data, self.word_size, data_start, &mut buf[..chunk_len]);

        // Mark chunk as dead and advance read pointer.
        self.set_word_at(rp.wrapping_add(1), CHUNK_MAGIC_DEAD);
        let new_rp = self.chunk_step(rp, chunk_len as u32);
        unsafe { (*self.hdr).read_pt.store(new_rp, Ordering::Release) };

        Ok(chunk_len)
    }

    // ── File ownership ────────────────────────────────────────────────────────

    /// Write all committed chunks to a file descriptor (for blackbox log dumps).
    pub fn write_to_file(&self, fd: libc::c_int) -> QbResult<usize> {
        let mut rp = self.read_pt();
        let wp = self.write_pt();
        let ws = self.word_size;
        let mut total = 0usize;

        while rp != wp {
            let magic = self.word_at(rp.wrapping_add(1));
            if magic != CHUNK_MAGIC { break; }
            let chunk_len = self.word_at(rp) as usize;
            let data_words = (chunk_len / 4) + if chunk_len % 4 != 0 { 1 } else { 0 };
            let mut buf = vec![0u8; chunk_len];
            let data_start = rp.wrapping_add(2) % ws;
            copy_bytes_from_ring(self.data as *const u32, ws, data_start, &mut buf);
            let written = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
            if written < 0 { return Err(QbError::from_last_os_error()); }
            total += written as usize;
            rp = rp.wrapping_add(2 + data_words as u32) % ws;
        }
        Ok(total)
    }

    pub fn chown(&self, uid: libc::uid_t, gid: libc::gid_t) -> QbResult<()> {
        chown_path(&self.hdr_path, uid, gid)?;
        chown_path(&self.data_path, uid, gid)
    }

    pub fn chmod(&self, mode: libc::mode_t) -> QbResult<()> {
        chmod_path(&self.hdr_path, mode)?;
        chmod_path(&self.data_path, mode)
    }
}

impl Drop for QbRingbuffer {
    fn drop(&mut self) {
        unsafe {
            (*self.hdr).ref_count.fetch_sub(1, Ordering::SeqCst);
            libc::munmap(self.hdr as *mut libc::c_void, self.hdr_map_size);
            libc::munmap(self.data as *mut libc::c_void, self.data_map_size);
        }
        if self.owner {
            for path in [&self.hdr_path, &self.data_path] {
                if let Ok(cp) = std::ffi::CString::new(path.as_str()) {
                    unsafe { libc::unlink(cp.as_ptr()) };
                }
            }
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Open or create a regular file at `path` for mmap, sized to `size` bytes.
fn open_mmap_file(path: &str, size: usize, creating: bool) -> QbResult<libc::c_int> {
    let cpath = std::ffi::CString::new(path).map_err(|_| {
        QbError::new(libc::EINVAL)
    })?;
    let fd = if creating {
        let mut fd = unsafe {
            libc::open(cpath.as_ptr(),
                       libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_EXCL,
                       0o600u32 as libc::c_uint)
        };
        if fd < 0 && unsafe { crate::error::get_errno() } == libc::EEXIST {
            // Stale file from a crashed previous instance — unlink and retry.
            unsafe { libc::unlink(cpath.as_ptr()) };
            fd = unsafe {
                libc::open(cpath.as_ptr(),
                           libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_EXCL,
                           0o600u32 as libc::c_uint)
            };
        }
        fd
    } else {
        unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) }
    };
    if fd < 0 {
        return Err(QbError::from_last_os_error());
    }
    if creating {
        if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
            let e = QbError::from_last_os_error();
            unsafe { libc::close(fd); libc::unlink(cpath.as_ptr()) };
            return Err(e);
        }
    }
    Ok(fd)
}

/// Copy at most 4095 bytes of `src` (NUL-terminated) into a [u8; 4096] field.
fn copy_path_to_field(dst: &mut [u8; 4096], src: &str) {
    let b = src.as_bytes();
    let n = b.len().min(4095);
    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
}

/// Copy bytes from `src` into the word-addressed ring `data[start_idx..]`, wrapping.
fn copy_bytes_to_ring(data: *mut u32, word_size: u32, start_idx: u32, src: &[u8]) {
    let mut byte_off = 0usize;
    let mut word_idx = start_idx;
    while byte_off < src.len() {
        // Build a u32 word from up to 4 bytes.
        let mut word = 0u32;
        let chunk = &src[byte_off..(byte_off + 4).min(src.len())];
        for (i, &b) in chunk.iter().enumerate() {
            word |= (b as u32) << (i * 8);
        }
        unsafe { *data.add((word_idx % word_size) as usize) = word; }
        word_idx = word_idx.wrapping_add(1);
        byte_off += 4;
    }
}

/// Copy bytes from the word-addressed ring `data[start_idx..]` into `dst`, wrapping.
fn copy_bytes_from_ring(data: *const u32, word_size: u32, start_idx: u32, dst: &mut [u8]) {
    let mut byte_off = 0usize;
    let mut word_idx = start_idx;
    while byte_off < dst.len() {
        let word = unsafe { *data.add((word_idx % word_size) as usize) };
        let remaining = dst.len() - byte_off;
        let bytes = remaining.min(4);
        for i in 0..bytes {
            dst[byte_off + i] = ((word >> (i * 8)) & 0xFF) as u8;
        }
        word_idx = word_idx.wrapping_add(1);
        byte_off += 4;
    }
}

fn chown_path(path: &str, uid: libc::uid_t, gid: libc::gid_t) -> QbResult<()> {
    let cp = std::ffi::CString::new(path).map_err(|_| QbError::new(libc::EINVAL))?;
    if unsafe { libc::chown(cp.as_ptr(), uid, gid) } == 0 { Ok(()) }
    else { Err(QbError::from_last_os_error()) }
}

fn chmod_path(path: &str, mode: libc::mode_t) -> QbResult<()> {
    let cp = std::ffi::CString::new(path).map_err(|_| QbError::new(libc::EINVAL))?;
    if unsafe { libc::chmod(cp.as_ptr(), mode) } == 0 { Ok(()) }
    else { Err(QbError::from_last_os_error()) }
}
