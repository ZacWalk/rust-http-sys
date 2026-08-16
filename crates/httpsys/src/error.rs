//! Error types for the `httpsys` crate.

use std::fmt;

use windows::Win32::Foundation::WIN32_ERROR;

/// All errors returned by this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A Win32 status code returned from an `Http*` or IOCP call.
    ///
    /// This is the plain Win32 value (e.g. `5` for `ERROR_ACCESS_DENIED`),
    /// never an `HRESULT`.
    Win32(u32),
    /// The request headers did not fit in the receive buffer, even after a
    /// retry. Raise [`ServerConfig::request_buffer_bytes`] if this shows up.
    ///
    /// [`ServerConfig::request_buffer_bytes`]: crate::ServerConfig::request_buffer_bytes
    HeadersTooLarge { needed: u32 },
    /// The request queue is shut down or the handle is invalid.
    QueueClosed,
}

impl Error {
    /// The underlying Win32 status code, if this is an [`Error::Win32`].
    pub fn win32_code(&self) -> Option<u32> {
        match self {
            Error::Win32(code) => Some(*code),
            _ => None,
        }
    }

    /// Map a raw Win32 status code (0 = success) into a `Result`.
    pub(crate) fn check(ec: u32) -> Result<()> {
        if ec == 0 {
            Ok(())
        } else {
            Err(Error::Win32(ec))
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Win32(code) => {
                let msg = windows::core::Error::from(WIN32_ERROR(*code))
                    .message()
                    .trim()
                    .to_owned();
                if msg.is_empty() {
                    write!(f, "Win32 error {code}")
                } else {
                    write!(f, "Win32 error {code}: {msg}")
                }
            }
            Error::HeadersTooLarge { needed } => {
                write!(f, "request headers too large: {needed} bytes needed")
            }
            Error::QueueClosed => f.write_str("request queue closed"),
        }
    }
}

impl std::error::Error for Error {}

/// Recovers the Win32 code from an `HRESULT` produced by a `FACILITY_WIN32`
/// API, which is what every call in this crate returns.
impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Win32((e.code().0 as u32) & 0xFFFF)
    }
}

/// Convenient `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_display_includes_os_message() {
        let text = Error::Win32(5).to_string(); // ERROR_ACCESS_DENIED
        assert!(text.starts_with("Win32 error 5:"), "got {text}");
    }

    #[test]
    fn check_maps_zero_to_ok() {
        assert!(Error::check(0).is_ok());
        assert_eq!(Error::check(87), Err(Error::Win32(87)));
    }

    #[test]
    fn hresult_round_trips_to_win32() {
        let hr = windows::core::Error::from(WIN32_ERROR(5));
        assert_eq!(Error::from(hr).win32_code(), Some(5));
    }
}
