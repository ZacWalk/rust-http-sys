//! Error types for the `httpsys` crate.

use std::fmt;

/// All errors returned by this crate.
#[derive(Debug)]
pub enum Error {
    /// A Win32 / NTSTATUS error code returned from an `Http*` call.
    Win32 {
        /// HRESULT-style error code.
        code: i32,
        /// Optional human-readable description provided by the OS.
        message: Option<String>,
    },
    /// Receive buffer was too small. Contains the size the kernel reported it
    /// needed, when known.
    BufferTooSmall { needed: Option<u32> },
    /// The request queue is shut down or the handle is invalid.
    QueueClosed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Win32 { code, message } => match message {
                Some(m) => write!(f, "Win32 error 0x{code:08x}: {m}"),
                None => write!(f, "Win32 error 0x{code:08x}"),
            },
            Error::BufferTooSmall { needed: Some(n) } => {
                write!(f, "buffer too small: {n} bytes needed")
            }
            Error::BufferTooSmall { needed: None } => f.write_str("buffer too small"),
            Error::QueueClosed => f.write_str("request queue closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Win32 {
            code: e.code().0,
            message: Some(e.message().to_string_lossy()),
        }
    }
}

impl Error {
    /// Construct an error from a raw Win32 status code.
    pub(crate) fn from_win32(ec: u32) -> Self {
        let we = windows::core::Error::from(windows::Win32::Foundation::WIN32_ERROR(ec));
        Error::Win32 {
            code: we.code().0,
            message: Some(we.message().to_string_lossy()),
        }
    }

    /// Map a raw Win32 status code (0 = success) into a `Result`.
    pub(crate) fn check(ec: u32) -> std::result::Result<(), Self> {
        if ec == 0 {
            Ok(())
        } else {
            Err(Error::from_win32(ec))
        }
    }
}

/// Convenient `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
