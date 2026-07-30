use std::fmt;

/// Translates a `Result<T, QbError>` return to a C `int32_t` errno convention.
/// Success → 0 (or the positive value for functions returning sizes).
/// Failure → negative errno value.
#[derive(Debug)]
pub struct QbError {
    pub errno: i32,
}

impl QbError {
    pub fn new(errno: i32) -> Self {
        QbError { errno: errno.abs() }
    }

    pub fn from_last_os_error() -> Self {
        let e = std::io::Error::last_os_error();
        QbError {
            errno: e.raw_os_error().unwrap_or(libc::EIO),
        }
    }
}

impl fmt::Display for QbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QbError(errno={})", self.errno)
    }
}

impl std::error::Error for QbError {}

impl From<std::io::Error> for QbError {
    fn from(e: std::io::Error) -> Self {
        QbError {
            errno: e.raw_os_error().unwrap_or(libc::EIO),
        }
    }
}

impl From<QbError> for std::io::Error {
    fn from(e: QbError) -> Self {
        std::io::Error::from_raw_os_error(e.errno)
    }
}

pub type QbResult<T> = Result<T, QbError>;

/// Convert a Result to a C int32_t return value (0 = success, -errno = failure).
#[allow(dead_code)]
pub fn result_to_c(r: QbResult<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => -(e.errno.abs()),
    }
}

/// Convert a Result<i32> to a C return value.
#[allow(dead_code)]
pub fn iresult_to_c(r: QbResult<i32>) -> i32 {
    match r {
        Ok(v) => v,
        Err(e) => -(e.errno.abs()),
    }
}

/// Convert a Result<usize> to a C ssize_t return value.
#[allow(dead_code)]
pub fn sresult_to_c(r: QbResult<usize>) -> libc::ssize_t {
    match r {
        Ok(v) => v as libc::ssize_t,
        Err(e) => -(e.errno.abs()) as libc::ssize_t,
    }
}

/// Cross-platform errno setter.
#[inline]
pub unsafe fn set_errno(val: i32) {
    #[cfg(target_os = "linux")]
    { *libc::__errno_location() = val; }
    #[cfg(not(target_os = "linux"))]
    { *libc::__error() = val; }
}

/// Cross-platform errno getter.
#[inline]
pub unsafe fn get_errno() -> i32 {
    #[cfg(target_os = "linux")]
    { *libc::__errno_location() }
    #[cfg(not(target_os = "linux"))]
    { *libc::__error() }
}

/// Macro: set errno and return a null pointer.
#[macro_export]
macro_rules! return_null_err {
    ($errno:expr) => {{
        unsafe { crate::error::set_errno($errno) };
        return std::ptr::null_mut();
    }};
}
