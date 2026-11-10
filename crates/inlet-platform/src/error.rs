use std::fmt;
use std::io;

/// Errors from the platform layer.
#[derive(Debug)]
pub enum Error {
    /// The feature does not exist on this OS or desktop, or needs something the
    /// process does not have (an app bundle, a tray host).
    Unsupported(&'static str),
    /// The input was refused before anything touched the OS.
    InvalidInput(&'static str),
    /// A file, socket or pipe that should be private to this user is not.
    Insecure(&'static str),
    /// Another instance exists (its pipe or lock is there) but did not take the
    /// command line within the deadline. Exit; do not start a second instance.
    NotResponding,
    /// An I/O error.
    Io(io::Error),
    /// An error reported by an OS API.
    Os(String),
}

/// Result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "not supported here: {what}"),
            Error::InvalidInput(what) => write!(f, "invalid input: {what}"),
            Error::Insecure(what) => write!(f, "refused, not private to this user: {what}"),
            Error::NotResponding => f.write_str("another instance is running but does not respond"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
            Error::Os(err) => write!(f, "OS error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for Error {
    fn from(err: windows::core::Error) -> Self {
        Error::Os(err.to_string())
    }
}

#[cfg(target_os = "linux")]
impl From<zbus::Error> for Error {
    fn from(err: zbus::Error) -> Self {
        Error::Os(err.to_string())
    }
}
