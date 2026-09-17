use thiserror::Error;

/// A general error wrapper for this crate.
#[derive(Debug, Error)]
pub enum Error {
    /// Errors that can occur when working with raw bencoded data.
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),

    /// Errors that can occur when working with torrent structures.
    #[error("Torrent error: {0}")]
    Torrent(#[from] crate::torrent::Error),
}
