use std::io;
use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("operation timed out after {0:?}")]
    Timeout(Duration),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("{0:?} is not a valid DNS name for TLS")]
    InvalidDnsName(String),

    #[error("server rejected {command}: {message}")]
    Rejected { command: String, message: String },

    #[error("malformed response from server: {0}")]
    Protocol(String),

    #[error("server closed the connection")]
    Closed,

    #[error("server does not advertise {0}, which this account requires")]
    Unsupported(&'static str),

    /// The server sent data before the TLS handshake began.
    ///
    /// A conforming server must discard anything buffered when it answers `STLS`. Data sitting in
    /// our read buffer at that point is either a badly broken server or an active attacker
    /// injecting plaintext commands that we would otherwise attribute to the encrypted session
    /// (the STARTTLS command-injection class of bug). Either way we refuse to continue.
    #[error("refusing STARTTLS: server pipelined {0} bytes before the TLS handshake")]
    StartTlsInjection(usize),
}

impl Error {
    /// Whether a retry with backoff could plausibly succeed.
    ///
    /// Authentication and capability failures are terminal; transport failures are not.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Io(_) | Error::Timeout(_) | Error::Closed => true,
            Error::Rejected { .. }
            | Error::Protocol(_)
            | Error::Tls(_)
            | Error::InvalidDnsName(_)
            | Error::Unsupported(_)
            | Error::StartTlsInjection(_) => false,
        }
    }
}
