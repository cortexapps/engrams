//! Connect error codes and the one error type the client surfaces.

use std::fmt;

/// The Connect error codes (the gRPC status set, minus OK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Canceled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

impl Code {
    /// Parse the wire spelling (`"not_found"`). Unknown spellings map to
    /// [`Code::Unknown`], as connect-es does.
    pub fn from_wire(s: &str) -> Code {
        match s {
            "canceled" => Code::Canceled,
            "unknown" => Code::Unknown,
            "invalid_argument" => Code::InvalidArgument,
            "deadline_exceeded" => Code::DeadlineExceeded,
            "not_found" => Code::NotFound,
            "already_exists" => Code::AlreadyExists,
            "permission_denied" => Code::PermissionDenied,
            "resource_exhausted" => Code::ResourceExhausted,
            "failed_precondition" => Code::FailedPrecondition,
            "aborted" => Code::Aborted,
            "out_of_range" => Code::OutOfRange,
            "unimplemented" => Code::Unimplemented,
            "internal" => Code::Internal,
            "unavailable" => Code::Unavailable,
            "data_loss" => Code::DataLoss,
            "unauthenticated" => Code::Unauthenticated,
            _ => Code::Unknown,
        }
    }

    /// The code for a non-2xx response that carries no Connect error body
    /// (the Connect protocol's HTTP-to-code table, as connect-es applies it).
    pub fn from_http_status(status: u16) -> Code {
        match status {
            400 => Code::Internal,
            401 => Code::Unauthenticated,
            403 => Code::PermissionDenied,
            404 => Code::Unimplemented,
            429 | 502 | 503 | 504 => Code::Unavailable,
            _ => Code::Unknown,
        }
    }

    /// The enum-name spelling (`NotFound`), which is what the CLI prints.
    pub fn name(self) -> &'static str {
        match self {
            Code::Canceled => "Canceled",
            Code::Unknown => "Unknown",
            Code::InvalidArgument => "InvalidArgument",
            Code::DeadlineExceeded => "DeadlineExceeded",
            Code::NotFound => "NotFound",
            Code::AlreadyExists => "AlreadyExists",
            Code::PermissionDenied => "PermissionDenied",
            Code::ResourceExhausted => "ResourceExhausted",
            Code::FailedPrecondition => "FailedPrecondition",
            Code::Aborted => "Aborted",
            Code::OutOfRange => "OutOfRange",
            Code::Unimplemented => "Unimplemented",
            Code::Internal => "Internal",
            Code::Unavailable => "Unavailable",
            Code::DataLoss => "DataLoss",
            Code::Unauthenticated => "Unauthenticated",
        }
    }
}

/// An RPC or transport failure. Renders as `<CodeName>: <message>`, the line
/// the CLI prints after `engrams: `.
#[derive(Debug)]
pub struct Error {
    pub code: Code,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
        }
    }

    /// A reqwest failure, mapped the way connect-node maps socket errors:
    /// connection failures are `Unavailable`, timeouts `DeadlineExceeded`,
    /// anything else `Unknown`.
    pub fn transport(err: reqwest::Error) -> Self {
        let code = if err.is_connect() {
            Code::Unavailable
        } else if err.is_timeout() {
            Code::DeadlineExceeded
        } else {
            Code::Unknown
        };
        // reqwest's Display nests the source ("error sending request for url
        // (…): connection refused"); the innermost cause is the useful line.
        let mut msg = err.to_string();
        let mut src = std::error::Error::source(&err);
        while let Some(s) = src {
            msg = s.to_string();
            src = s.source();
        }
        Error { code, message: msg }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.name(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::transport(err)
    }
}
