use std::fmt;

/// Single error type for the entire crate.
/// No traits, no macros, no error crates.
#[allow(dead_code)]
pub enum Error {
    Io(std::io::Error),
    TerminalQuery,
    InvalidArgs(&'static str),
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::TerminalQuery => f.write_str("terminal query failed"),
            Self::InvalidArgs(m) => write!(f, "invalid args: {m}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl Error {
    #[must_use]
    #[allow(clippy::unused_self)]
    pub const fn exit_code(&self) -> i32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_io_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = Error::Io(inner);
        assert_eq!(err.to_string(), "io: gone");
    }

    #[test]
    fn display_terminal_query() {
        let err = Error::TerminalQuery;
        assert_eq!(err.to_string(), "terminal query failed");
    }

    #[test]
    fn display_invalid_args() {
        let err = Error::InvalidArgs("missing subcommand");
        assert_eq!(err.to_string(), "invalid args: missing subcommand");
    }

    #[test]
    fn from_io_error() {
        let inner = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let err: Error = inner.into();
        assert!(matches!(err, Error::Io(_)));
        assert_eq!(err.to_string(), "io: nope");
    }

    #[test]
    fn exit_code_is_always_one() {
        assert_eq!(Error::TerminalQuery.exit_code(), 1);
        assert_eq!(Error::InvalidArgs("x").exit_code(), 1);
        let io_err = Error::Io(std::io::Error::other(""));
        assert_eq!(io_err.exit_code(), 1);
    }
}
