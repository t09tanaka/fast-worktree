use std::fmt;

/// A failure that ends the command. The message is printed after
/// `fast-worktree: ` and the process exits non-zero.
#[derive(Debug)]
pub struct Error(pub String);

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

/// Return early with a formatted [`Error`].
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error(format!($($arg)*)))
    };
}
pub(crate) use bail;
