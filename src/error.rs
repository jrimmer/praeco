//! Error type shared by the whole crate. Every error carries a stable
//! machine-readable code and a plain-language message fit for showing to a user.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Usage,
    InvalidVersion,
    Network,
    RateLimited,
    Api,
    NoRelease,
    NoAsset,
    Unsupported,
    ChecksumMissing,
    ChecksumMismatch,
    Download,
    NeedsTerminal,
    AuthCancelled,
    InstallFailed,
    TerminalClosed,
    RestartFailed,
    Io,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Usage => "usage",
            ErrorCode::InvalidVersion => "invalid_version",
            ErrorCode::Network => "network",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Api => "api",
            ErrorCode::NoRelease => "no_release",
            ErrorCode::NoAsset => "no_asset",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::ChecksumMissing => "checksum_missing",
            ErrorCode::ChecksumMismatch => "checksum_mismatch",
            ErrorCode::Download => "download",
            ErrorCode::NeedsTerminal => "needs_terminal",
            ErrorCode::AuthCancelled => "auth_cancelled",
            ErrorCode::InstallFailed => "install_failed",
            ErrorCode::TerminalClosed => "terminal_closed",
            ErrorCode::RestartFailed => "restart_failed",
            ErrorCode::Io => "io",
        }
    }

    pub fn from_code(s: &str) -> ErrorCode {
        const ALL: [ErrorCode; 17] = [
            ErrorCode::Usage,
            ErrorCode::InvalidVersion,
            ErrorCode::Network,
            ErrorCode::RateLimited,
            ErrorCode::Api,
            ErrorCode::NoRelease,
            ErrorCode::NoAsset,
            ErrorCode::Unsupported,
            ErrorCode::ChecksumMissing,
            ErrorCode::ChecksumMismatch,
            ErrorCode::Download,
            ErrorCode::NeedsTerminal,
            ErrorCode::AuthCancelled,
            ErrorCode::InstallFailed,
            ErrorCode::TerminalClosed,
            ErrorCode::RestartFailed,
            ErrorCode::Io,
        ];
        ALL.into_iter().find(|c| c.as_str() == s).unwrap_or(ErrorCode::InstallFailed)
    }

    /// Process exit code the CLI uses for this error.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Usage => 2,
            ErrorCode::NeedsTerminal => 3,
            ErrorCode::Unsupported => 4,
            ErrorCode::RestartFailed => 5,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Error {
        Error { code, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(ErrorCode::Io, format!("File system problem: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
