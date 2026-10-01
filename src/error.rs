//! The crate-wide error type.
//!
//! Each module of this crate defines its own error enum that describes what can go wrong in that
//! module: [`bencode::Error`](crate::bencode::Error) for raw bencode,
//! [`torrent::Error`](crate::torrent::Error) for torrent metainfo, and
//! [`torrent::builder::Error`](crate::torrent::builder::Error) for building torrents from files.
//!
//! [`enum@Error`] wraps the first two so that functions that span both layers, such as
//! [`parse_torrent`](crate::parse_torrent), can return a single error type. Both conversions are
//! provided through [`From`], so the `?` operator works with either of them.

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
