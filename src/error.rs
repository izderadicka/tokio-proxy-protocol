use thiserror::Error;

/// Error type for this module
#[derive(Error, Debug)]
pub enum Error {
    #[error("Proxy protocol error: {0}")]
    Proxy(String),

    #[error("IO error: {0}")]
    Io(#[from] tokio::io::Error),

    #[error("Invalid encoding of proxy header: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("Invalid address in proxy header: {0}")]
    IPAddress(#[from] std::net::AddrParseError),

    #[error("Invalid port in proxy header: {0}")]
    Port(#[from] std::num::ParseIntError),

    #[error("Invalid state: {0}")]
    InvalidState(String),
}

/// Convenient Result type, with our Error included
pub type Result<T> = std::result::Result<T, Error>;
