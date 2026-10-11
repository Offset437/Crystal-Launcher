//! Shared PlayOnline credential codecs, record formats, and protocol fixtures.
//! The retained Rust sign-in driver supports comparisons; Crystal's retail
//! launches authenticate through the installed native POL core instead.

#[allow(dead_code)]
mod assembly;
pub mod catalog;
pub mod friends;
mod friend_presence;
pub mod chat;
pub mod chat_transport;
pub mod credential;
#[allow(dead_code)]
mod crypto;
pub mod entropy;
pub mod identity;
pub mod keepalive;
pub mod loader;
pub mod lobby_selection;
pub mod pol_records;
pub mod profile;
pub mod retail;
pub mod rsa;
pub mod session;
pub mod transport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidName,
    InvalidOtp,
    InvalidFrame,
    Checksum,
    ServerStatus(u8),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidName => {
                "profile login name must contain 1 to 16 ASCII characters without controls"
            }
            Self::InvalidOtp => "one-time password must contain six decimal digits",
            Self::InvalidFrame => "invalid profile frame",
            Self::Checksum => "profile body checksum mismatch",
            Self::ServerStatus(_) => "profile server refused the request",
        })
    }
}

impl std::error::Error for Error {}
