use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),
    #[error("Torrent error: {0}")]
    Torrent(#[from] crate::torrent::Error),
}
