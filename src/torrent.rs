//! Types and logic for `.torrent` metainfo files.
//!
//! This module contains the complete type hierarchy that mirrors the structure
//! of a BitTorrent metainfo file, along with parsing (via [`TryFrom<Bencode>`])
//! and two construction paths:
//!
//! - [`builder::TorrentBuilder`] — wraps an [`InfoBuf`] you already have.
//! - [`factory::TorrentFactory`] — reads files from disk and computes piece hashes.
//!
//! # Type hierarchy
//!
//! ```text
//! Torrent<'a>
//! ├── announce: Option<Url>
//! ├── announce_list: Option<Vec<Vec<Url>>>
//! ├── url_list: Option<Vec<Url>>         ← web-seed URLs (BEP 19)
//! ├── creation_date: Option<u64>
//! ├── comment / created_by / encoding: Option<Cow<'a, str>>
//! └── info: Info<'a>
//!     ├── name: Cow<'a, str>
//!     ├── piece_length: NonZeroU64
//!     ├── pieces: Cow<'a, [[u8; 20]]>   ← one 20-byte SHA-1 hash per piece
//!     ├── private: bool
//!     ├── source: Option<Cow<'a, str>>  ← private-tracker source tag
//!     └── file_mode: FileMode<'a>
//!         ├── Single { length, md5sum }
//!         └── Multi  { files: Vec<FileInfo<'a>> }
//!                         ├── length
//!                         ├── md5sum
//!                         └── path: Vec<Cow<'a, str>>
//! ```
//!
//! # Equality
//!
//! [`Torrent`], [`Info`], [`FileMode`], and [`FileInfo`] all derive [`PartialEq`] and
//! [`Eq`], so you can compare two parsed or constructed values directly:
//!
//! ```no_run
//! # use bitors::{bencode::Parser, torrent::Torrent};
//! # let bytes1 = Vec::new(); let bytes2 = Vec::new();
//! let t1: Torrent<'_> = Parser::new(&bytes1).parse().unwrap().try_into().unwrap();
//! let t2: Torrent<'_> = Parser::new(&bytes2).parse().unwrap().try_into().unwrap();
//! assert_eq!(t1, t2);
//! ```
//!
//! # Info hash
//!
//! [`Torrent::info_hash`] (and its delegate [`Info::info_hash`]) return the 20-byte
//! SHA-1 hash of the Bencoded `info` dictionary — the standard torrent identifier
//! exchanged with trackers and embedded in magnet links.
//!
//! # Lifetimes and owned variants
//!
//! Every type carries a lifetime parameter `'a` because string and byte-slice
//! fields may borrow from the original source buffer when the value is produced
//! by the Bencode parser.  Call `into_owned()` on any value to obtain its
//! `'static` alias:
//!
//! | Borrowing type | `'static` alias |
//! |---|---|
//! | `Torrent<'a>` | [`TorrentBuf`] |
//! | `Info<'a>` | [`InfoBuf`] |
//! | `FileMode<'a>` | [`FileModeBuf`] |
//! | `FileInfo<'a>` | [`FileInfoBuf`] |
//!
//! # Round-trip encoding
//!
//! Any value can be converted back to a [`Bencode`] tree via the `From`
//! implementations in [`crate::bencode`], and then written to a file with
//! [`Bencode::encode_to_writer`](crate::bencode::Bencode::encode_to_writer).

pub mod builder;
pub mod factory;

use std::{borrow::Cow, collections::BTreeMap, num::NonZeroU64, path::PathBuf};

use sha1::{Digest, Sha1};
use thiserror::Error;
use url::Url;

use crate::{
    bencode::Bencode,
    magnet::MagnetLink,
    torrent::{
        builder::TorrentBuilder,
        factory::{TorrentFactory, state::Empty},
    },
};

/// An internal extension trait for `BTreeMap` to simplify extracting optional
/// and required fields from Bencoded dictionaries.
///
/// All methods take `&mut self` and **remove** the entry from the map, transferring
/// ownership of the `Bencode<'a>` value to the caller.  This is essential for
/// consuming `TryFrom` impls: the map is obtained via [`Bencode::into_dict`], so
/// its entries carry lifetime `'a` (borrowed from the original source buffer), and
/// returning them by value rather than by reference keeps that lifetime intact after
/// the map itself is dropped.
trait DictExt<'a> {
    /// Removes the entry for `key` and returns the owned value, or `None` if absent.
    fn opt(&mut self, key: &[u8]) -> Option<Bencode<'a>>;

    /// Removes the entry for `key` and returns the owned value, or an error if absent.
    fn require(&mut self, key: &[u8]) -> Result<Bencode<'a>, Error>;

    /// Removes the entry for `key` and returns it interpreted as a UTF-8 string.
    ///
    /// Returns `Ok(None)` if the key is absent, `Ok(Some(&str))` if present and
    /// valid UTF-8, or an error if the value is not a byte string or is not valid
    /// UTF-8.
    fn opt_str(&mut self, key: &[u8]) -> Result<Option<&'a str>, Error>;

    /// Removes the entry for `key` and returns it interpreted as a UTF-8 string,
    /// or an error if the key is absent or the value is not a valid UTF-8 string.
    fn require_str(&mut self, key: &[u8]) -> Result<&'a str, Error>;
}

impl<'a> DictExt<'a> for BTreeMap<&'a [u8], Bencode<'a>> {
    /// Removes the need in `b"...".as_slice()` in normal `BTreeMap::get` calls.
    fn opt(&mut self, key: &[u8]) -> Option<Bencode<'a>> {
        self.remove(key)
    }
    
    fn require(&mut self, key: &[u8]) -> Result<Bencode<'a>, Error> {
        self.opt(key).ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }

    fn opt_str(&mut self, key: &[u8]) -> Result<Option<&'a str>, Error> {
        self.opt(key)
            .map(|b| b.as_str())
            .transpose()
            .map_err(Error::from)
    }

    fn require_str(&mut self, key: &[u8]) -> Result<&'a str, Error> {
        self.opt_str(key)?.ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }
}

/// Represents the root data structure of a parsed `.torrent` file.
///
/// This struct contains all the top-level metadata required by a BitTorrent client
/// to connect to trackers and understand the contents of the torrent.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Torrent<'a> {
    /// A dictionary that describes the file(s) of the torrent.
    pub info: Info<'a>,
    /// The primary announce URL of the tracker.
    pub announce: Option<Url>,
    /// An optional list of backup trackers (Tiered trackers).
    pub announce_list: Option<Vec<Vec<Url>>>,
    /// A list of web-seed URLs ([BEP 19]) from which clients may retrieve content over
    /// HTTP or HTTPS when peers are scarce.  `None` if the field is absent from the
    /// `.torrent` file.
    ///
    /// [BEP 19]: https://www.bittorrent.org/beps/bep_0019.html
    pub url_list: Option<Vec<Url>>,
    /// The creation time of the torrent, in standard POSIX epoch format.
    pub creation_date: Option<u64>,
    /// Free-form textual comments of the author.
    pub comment: Option<Cow<'a, str>>,
    /// Name and version of the program used to create the .torrent.
    pub created_by: Option<Cow<'a, str>>,
    /// The string encoding format used to generate the pieces part of the info dictionary
    /// in the .torrent metainfo file (e.g., "UTF-8").
    pub encoding: Option<Cow<'a, str>>,
}

impl Torrent<'_> {
    /// Creates a new [`TorrentBuilder`] pre-loaded with the given `info` dictionary.
    ///
    /// Use the builder to attach optional metadata (announce URLs, creation date,
    /// comments, etc.) before constructing a final [`TorrentBuf`].
    #[must_use]
    pub fn builder(info: InfoBuf) -> TorrentBuilder {
        TorrentBuilder::new(info)
    }

    /// Creates a new [`TorrentFactory`] in its initial [`Empty`] state.
    ///
    /// The factory uses a typestate pattern to guide construction of a torrent,
    /// enforcing at compile-time that all required fields are provided before
    /// the final value is assembled.
    #[must_use]
    pub fn factory() -> TorrentFactory<Empty> {
        TorrentFactory::new()
    }

    /// Returns all tracker URLs as a tiered list, normalised to the
    /// `announce-list` format defined in [BEP 12].
    ///
    /// - If only `announce` is present, it is wrapped in a single-tier list:
    ///   `[[announce]]`.
    /// - If `announce-list` is present it is returned as-is (the standalone
    ///   `announce` field is ignored per BEP 12).
    /// - If neither field is set (only valid for private torrents), an empty
    ///   `Vec` is returned.
    ///
    /// [BEP 12]: https://www.bittorrent.org/beps/bep_0012.html
    #[must_use]
    pub fn trackers(&self) -> Vec<Vec<&Url>> {
        match (&self.announce, &self.announce_list) {
            (Some(url), None) => vec![vec![url]],
            (_, Some(tiers)) => tiers.iter().map(|tier| tier.iter().collect()).collect(),
            (None, None) => vec![],
        }
    }

    /// Computes the 20-byte SHA-1 info hash for this torrent.
    ///
    /// The info hash is calculated by Bencoding the [`Info`] dictionary and then hashing
    /// the resulting bytes with SHA-1.  It uniquely identifies the torrent's content to
    /// trackers and other peers, and is the canonical identifier used in magnet links.
    ///
    /// This is a convenience wrapper around [`Info::info_hash`].
    #[must_use]
    pub fn info_hash(&self) -> [u8; 20] {
        self.info.info_hash()
    }

    /// Returns the total size of all content in this torrent, in bytes.
    ///
    /// For single-file torrents this is the length of that file.  For multi-file torrents
    /// this is the sum of the lengths of all files in the torrent.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        match &self.info.file_mode {
            FileMode::Single { length, .. } => *length,
            FileMode::Multi { files } => files.iter().map(|f| f.length).sum(),
        }
    }

    /// Returns the number of files described by this torrent.
    ///
    /// Single-file torrents always return `1`.  Multi-file torrents return the number
    /// of entries in the `files` list.
    #[must_use]
    pub fn file_count(&self) -> usize {
        match &self.info.file_mode {
            FileMode::Single { .. } => 1,
            FileMode::Multi { files } => files.len(),
        }
    }

    /// Generates a [`MagnetLink`] for this torrent.
    ///
    /// The returned value captures the info hash, display name, tracker URLs (flattened
    /// from all tiers), and total content size.  Convert it to a URI string with
    /// [`Display`](std::fmt::Display) (hex) or
    /// [`MagnetLink::to_uri_base32`](crate::magnet::MagnetLink::to_uri_base32).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use bitors::parse_torrent;
    ///
    /// let bytes = std::fs::read("ubuntu.torrent")?;
    /// let torrent = parse_torrent(&bytes)?;
    ///
    /// println!("{}", torrent.magnet_link());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn magnet_link(&self) -> MagnetLink {
        self.into()
    }

    /// Converts the `Torrent` struct back into a `Bencode` representation.
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    /// Converts this `Torrent<'a>` into a [`TorrentBuf`] (`Torrent<'static>`) by
    /// cloning any borrowed data into owned allocations.
    ///
    /// This is useful when you need to store or return a torrent without being
    /// constrained by the lifetime of the original source bytes.
    #[must_use]
    pub fn into_owned(self) -> TorrentBuf {
        TorrentBuf {
            info: self.info.into_owned(),
            announce: self.announce,
            announce_list: self.announce_list,
            url_list: self.url_list,
            creation_date: self.creation_date,
            comment: self.comment.map(|c| Cow::Owned(c.into_owned())),
            created_by: self.created_by.map(|c| Cow::Owned(c.into_owned())),
            encoding: self.encoding.map(|c| Cow::Owned(c.into_owned())),
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for Torrent<'a> {
    type Error = Error;

    /// Attempts to parse a `Torrent` from a generic `Bencode` structure.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if:
    /// - The bencode is not a dictionary.
    /// - The required `info` field is missing or invalid.
    /// - Announce URLs are malformed.
    /// - Data types for specific fields do not match the specification.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut map = bencode.into_dict()?;

        let info: Info<'_> = map.require(b"info")?.try_into()?;

        let announce = map.opt_str(b"announce")?.map(Url::parse).transpose()?;

        let announce_list = map
            .opt(b"announce-list")
            .map(|b| {
                b.as_list()?
                    .iter()
                    .map(|b| {
                        b.as_list()?
                            .iter()
                            .map(|b| Ok::<Url, Error>(Url::parse(b.as_str()?)?))
                            .collect::<Result<Vec<Url>, _>>()
                    })
                    .collect::<Result<Vec<Vec<Url>>, _>>()
            })
            .transpose()?;

        let url_list = map
            .opt(b"url-list")
            .map(|b| {
                b.as_list()?
                    .iter()
                    .map(|b| Ok::<Url, Error>(Url::parse(b.as_str()?)?))
                    .collect::<Result<Vec<Url>, _>>()
            })
            .transpose()?;

        let creation_date = map
            .opt(b"creation date")
            .map(|b| -> Result<u64, Error> {
                b.as_int()?
                    .try_into()
                    .map_err(|_| Error::IllegalFieldValue("creation date"))
            })
            .transpose()?;

        let comment = map.opt_str(b"comment")?.map(Cow::Borrowed);

        let created_by = map.opt_str(b"created by")?.map(Cow::Borrowed);

        let encoding = map.opt_str(b"encoding")?.map(Cow::Borrowed);

        Ok(Self {
            info,
            announce,
            announce_list,
            url_list,
            creation_date,
            comment,
            created_by,
            encoding,
        })
    }
}

/// An owned, lifetime-free version of [`Torrent`].
///
/// All borrowed string slices and byte slices are replaced with heap-allocated
/// equivalents, making this value self-contained and `'static`.
pub type TorrentBuf = Torrent<'static>;

/// Represents the `info` dictionary within a torrent file.
///
/// This structure holds the critical data describing the payload (the files to download),
/// including file names, piece sizes, and the cryptographic hashes used to verify data integrity.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Info<'a> {
    /// In the single file case, the name of the file.
    /// In the multiple file case, the name of the directory in which to store all the files.
    pub name: Cow<'a, str>,
    /// The number of bytes in each piece the files are split into.
    pub piece_length: NonZeroU64,
    /// An array of 20-byte SHA1 hashes, one for each piece in the torrent.
    pub pieces: Cow<'a, [[u8; 20]]>,
    /// If true, the client must not obtain peer data from the DHT or PEX.
    /// It must only rely on the specified tracker(s).
    pub private: bool,
    /// An optional source tag, commonly set by private trackers to distinguish their
    /// copies of a torrent from copies seeded elsewhere.  Changing this field produces
    /// a different info hash, so two torrents with the same files but different `source`
    /// values are treated as distinct by clients.
    pub source: Option<Cow<'a, str>>,
    /// Dictates whether this torrent represents a single file or a directory of multiple files.
    pub file_mode: FileMode<'a>,
}

impl Info<'_> {
    /// Computes the 20-byte SHA-1 info hash of this `Info` dictionary.
    ///
    /// The hash is produced by Bencoding the dictionary and feeding the resulting bytes
    /// into SHA-1.  This value is the canonical torrent identifier: it is exchanged with
    /// trackers, embedded in magnet links, and used by peers to confirm they are sharing
    /// the same content.
    ///
    /// Because Bencode encoding is deterministic, two `Info` values that compare equal
    /// via [`PartialEq`] will always produce the same hash.
    #[must_use]
    pub fn info_hash(&self) -> [u8; 20] {
        let mut hasher = digest_io::IoWrapper(Sha1::new());
        #[expect(clippy::missing_panics_doc, reason = "infallible")]
        self.to_bencode()
            .encode_to_writer(&mut hasher)
            .expect("Writing to hasher should not fail");
        hasher.0.finalize().into()
    }

    /// Converts the `Info` struct back into a `Bencode` representation.
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    /// Converts this `Info<'a>` into an [`InfoBuf`] (`Info<'static>`) by cloning
    /// any borrowed data into owned allocations.
    #[must_use]
    pub fn into_owned(self) -> InfoBuf {
        InfoBuf {
            name: Cow::Owned(self.name.into_owned()),
            piece_length: self.piece_length,
            pieces: Cow::Owned(self.pieces.into_owned()),
            private: self.private,
            source: self.source.map(|c| Cow::Owned(c.into_owned())),
            file_mode: self.file_mode.into_owned(),
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for Info<'a> {
    type Error = Error;

    /// Attempts to parse an `Info` struct from a `Bencode` dictionary.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if required fields are missing, if `pieces` is not
    /// perfectly divisible by 20 bytes, or if data types are incorrect.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut map = bencode.into_dict()?;

        let name = map.require_str(b"name")?;

        let piece_length = map
            .require(b"piece length")?
            .as_int()?
            .try_into()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::IllegalFieldValue("piece length"))?;

        let pieces = map.require(b"pieces")?.as_bytes()?;
        let (pieces, []) = pieces.as_chunks() else {
            return Err(Error::InvalidPiecesLength);
        };

        let private = match map.opt(b"private") {
            Some(b) => match b.as_int()? {
                0 => false,
                1 => true,
                _ => return Err(Error::IllegalFieldValue("private")),
            },
            None => false,
        };

        let source = map.opt_str(b"source")?.map(Cow::Borrowed);

        let file_mode = if let Some(b) = map.opt(b"files") {
            let files = b
                .into_list()?
                .into_iter()
                .map(FileInfo::try_from)
                .collect::<Result<Vec<FileInfo>, _>>()?;

            FileMode::Multi { files }
        } else {
            let length = map
                .require(b"length")?
                .as_int()?
                .try_into()
                .map_err(|_| Error::IllegalFieldValue("length"))?;

            let md5sum = map.opt_str(b"md5sum")?.map(Cow::Borrowed);

            FileMode::Single { length, md5sum }
        };

        Ok(Self {
            name: Cow::Borrowed(name),
            piece_length,
            pieces: Cow::Borrowed(pieces),
            private,
            source,
            file_mode,
        })
    }
}

/// An owned, lifetime-free version of [`Info`].
///
/// See [`TorrentBuf`] for the same concept applied to the top-level torrent.
pub type InfoBuf = Info<'static>;

/// Defines the structure of the payload contained within the torrent.
///
/// BitTorrent supports both single-file payloads and multi-file directory payloads.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileMode<'a> {
    /// Represents a torrent containing exactly one file.
    Single {
        /// The length of the file in bytes.
        length: u64,
        /// An optional 32-character hexadecimal string corresponding to the MD5 sum of the file.
        md5sum: Option<Cow<'a, str>>,
    },
    /// Represents a torrent containing a directory of multiple files.
    Multi {
        /// A list detailing each individual file inside the torrent directory.
        files: Vec<FileInfo<'a>>,
    },
}

impl FileMode<'_> {
    /// Returns `true` if this torrent carries exactly one file.
    #[must_use]
    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single { .. })
    }

    /// Returns `true` if this torrent carries multiple files inside a directory.
    #[must_use]
    pub fn is_multi(&self) -> bool {
        !self.is_single()
    }

    /// Converts this `FileMode<'a>` into a [`FileModeBuf`] (`FileMode<'static>`)
    /// by cloning any borrowed string data into owned allocations.
    #[must_use]
    pub fn into_owned(self) -> FileModeBuf {
        match self {
            Self::Single { length, md5sum } => FileModeBuf::Single {
                length,
                md5sum: md5sum.map(|c| Cow::Owned(c.into_owned())),
            },
            Self::Multi { files } => FileModeBuf::Multi {
                files: files.into_iter().map(FileInfo::into_owned).collect(),
            },
        }
    }
}

/// An owned, lifetime-free version of [`FileMode`].
pub type FileModeBuf = FileMode<'static>;

/// Metadata for a single file within a multi-file torrent.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfo<'a> {
    /// The length of the file in bytes.
    pub length: u64,
    /// An optional 32-character hexadecimal string corresponding to the MD5 sum of the file.
    pub md5sum: Option<Cow<'a, str>>,
    /// A list containing one or more string elements that together represent the path and filename.
    /// Each element corresponds to a directory name or (for the last element) the filename.
    pub path: Vec<Cow<'a, str>>,
}

impl FileInfo<'_> {
    /// Reconstructs the file's full relative path as a [`PathBuf`].
    ///
    /// Each element of [`FileInfo::path`] becomes one path component, so
    /// `["docs", "readme.txt"]` yields `docs/readme.txt` (or the OS-appropriate
    /// separator).
    #[must_use]
    pub fn full_path(&self) -> PathBuf {
        let mut full_path = PathBuf::new();
        self.path
            .iter()
            .for_each(|comp| full_path.push(comp.to_string()));
        full_path
    }

    /// Converts the `FileInfo` struct back into a `Bencode` representation.
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    /// Converts this `FileInfo<'a>` into a [`FileInfoBuf`] (`FileInfo<'static>`)
    /// by cloning all borrowed string data into owned allocations.
    #[must_use]
    pub fn into_owned(self) -> FileInfoBuf {
        FileInfoBuf {
            length: self.length,
            md5sum: self.md5sum.map(|c| Cow::Owned(c.into_owned())),
            path: self
                .path
                .into_iter()
                .map(|c| Cow::Owned(c.into_owned()))
                .collect(),
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileInfo<'a> {
    type Error = Error;

    /// Attempts to parse a `FileInfo` struct from a `Bencode` dictionary.
    ///
    /// # Errors
    ///
    /// Returns an error if the `length` or `path` fields are missing or invalid.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut map = bencode.into_dict()?;

        let length = map
            .require(b"length")?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = map.opt_str(b"md5sum")?.map(Cow::Borrowed);

        let path = map
            .require(b"path")?
            .as_list()?
            .iter()
            .map(|b| b.as_str().map(Cow::Borrowed))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            length,
            md5sum,
            path,
        })
    }
}

/// An owned, lifetime-free version of [`FileInfo`].
pub type FileInfoBuf = FileInfo<'static>;

/// Errors that can occur during the parsing and validation of a `.torrent` file.
#[derive(Debug, Error)]
pub enum Error {
    /// Indicates an underlying failure when parsing the Bencode data structure.
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),

    /// Indicates an I/O related failure.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Indicates that an announce URL could not be parsed properly.
    #[error("URL parsing error: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// Indicates a field mandated by the BitTorrent specification is missing.
    #[error("Missing required field: {0}")]
    MissingField(String),

    /// Indicates a field was found, but contained an invalid value or data type.
    #[error("Illegal value in field '{0}'")]
    IllegalFieldValue(&'static str),

    /// Indicates the concatenated pieces byte string is not a multiple of 20.
    /// Since SHA-1 hashes are exactly 20 bytes long, this implies data corruption.
    #[error("Length of the 'pieces' list must be a multiple of 20")]
    InvalidPiecesLength,

    /// Indicates that neither an `announce` nor `announce-list` field was found.
    #[error("No announce URLs found")]
    MissingAnnounce,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // Helper to generate a valid 20-byte dummy hash for testing
    fn dummy_pieces() -> [u8; 20] {
        [0xab; 20]
    }

    #[test]
    fn test_parse_single_file_info() {
        let mut map = BTreeMap::new();
        map.insert(&b"name"[..], Bencode::Bytes(b"ubuntu.iso"));
        map.insert(&b"piece length"[..], Bencode::Int(262_144));
        let pieces = dummy_pieces();
        map.insert(&b"pieces"[..], Bencode::Bytes(&pieces));
        map.insert(&b"length"[..], Bencode::Int(1_024_000));

        let bencode = Bencode::Dict(map);
        let info = Info::try_from(bencode).expect("Failed to parse valid single-file info");

        assert_eq!(info.name, "ubuntu.iso");
        assert_eq!(info.piece_length, NonZeroU64::new(262_144).unwrap());
        assert_eq!(info.pieces.len(), 1); // 1 chunk of 20 bytes
        assert!(!info.private);

        match info.file_mode {
            FileMode::Single { length, md5sum } => {
                assert_eq!(length, 1_024_000);
                assert_eq!(md5sum, None);
            }
            FileMode::Multi { .. } => panic!("Expected FileMode::Single"),
        }
    }

    #[test]
    fn test_parse_multi_file_info() {
        // Build a FileInfo dict
        let mut file_map = BTreeMap::new();
        file_map.insert(&b"length"[..], Bencode::Int(512));
        let path_list = vec![Bencode::Bytes(b"docs"), Bencode::Bytes(b"readme.txt")];
        file_map.insert(&b"path"[..], Bencode::List(path_list));
        let file_bencode = Bencode::Dict(file_map);

        // Build the main Info dict
        let mut map = BTreeMap::new();
        map.insert(&b"name"[..], Bencode::Bytes(b"my_folder"));
        map.insert(&b"piece length"[..], Bencode::Int(262_144));
        let pieces = dummy_pieces();
        map.insert(&b"pieces"[..], Bencode::Bytes(&pieces));
        map.insert(&b"files"[..], Bencode::List(vec![file_bencode]));

        let bencode = Bencode::Dict(map);
        let info = Info::try_from(bencode).expect("Failed to parse valid multi-file info");

        assert_eq!(info.name, "my_folder");

        match info.file_mode {
            FileMode::Multi { files } => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].length, 512);
                assert_eq!(files[0].path, vec!["docs", "readme.txt"]);
            }
            FileMode::Single { .. } => panic!("Expected FileMode::Multi"),
        }
    }

    #[test]
    fn test_invalid_pieces_length() {
        let mut map = BTreeMap::new();
        map.insert(&b"name"[..], Bencode::Bytes(b"test"));
        map.insert(&b"piece length"[..], Bencode::Int(262_144));
        map.insert(&b"length"[..], Bencode::Int(1024));

        // 21 bytes is invalid (must be multiple of 20)
        let invalid_pieces = [0xab; 21];
        map.insert(&b"pieces"[..], Bencode::Bytes(&invalid_pieces));

        let bencode = Bencode::Dict(map);
        let err = Info::try_from(bencode).expect_err("Should have failed on invalid pieces length");

        assert!(matches!(err, Error::InvalidPiecesLength));
    }

    #[test]
    fn test_parse_valid_torrent() {
        let mut info_map = BTreeMap::new();
        info_map.insert(&b"name"[..], Bencode::Bytes(b"test"));
        info_map.insert(&b"piece length"[..], Bencode::Int(262_144));
        info_map.insert(&b"length"[..], Bencode::Int(1024));
        let pieces = dummy_pieces();
        info_map.insert(&b"pieces"[..], Bencode::Bytes(&pieces));

        let mut torrent_map = BTreeMap::new();
        torrent_map.insert(&b"info"[..], Bencode::Dict(info_map));
        torrent_map.insert(
            &b"announce"[..],
            Bencode::Bytes(b"http://tracker.example.com/announce"),
        );
        torrent_map.insert(&b"created by"[..], Bencode::Bytes(b"MyTorrentClient/1.0"));
        torrent_map.insert(&b"creation date"[..], Bencode::Int(1_620_000_000));

        let bencode = Bencode::Dict(torrent_map);
        let torrent = Torrent::try_from(bencode).expect("Failed to parse valid torrent");

        assert_eq!(
            torrent.announce.unwrap().as_str(),
            "http://tracker.example.com/announce"
        );
        assert_eq!(torrent.created_by.unwrap(), "MyTorrentClient/1.0");
        assert_eq!(torrent.creation_date.unwrap(), 1_620_000_000);
        assert_eq!(torrent.info.name, "test");
    }

    #[test]
    fn test_announce_list() {
        let mut info_map = BTreeMap::new();
        info_map.insert(&b"name"[..], Bencode::Bytes(b"test"));
        info_map.insert(&b"piece length"[..], Bencode::Int(262_144));
        info_map.insert(&b"length"[..], Bencode::Int(1024));
        let pieces = dummy_pieces();
        info_map.insert(&b"pieces"[..], Bencode::Bytes(&pieces));

        let mut torrent_map = BTreeMap::new();
        torrent_map.insert(&b"info"[..], Bencode::Dict(info_map));

        // Multi-tier announce list: [["http://tracker1.com"], ["http://tracker2.com", "http://tracker3.com"]]
        let tier1 = Bencode::List(vec![Bencode::Bytes(b"http://tracker1.com")]);
        let tier2 = Bencode::List(vec![
            Bencode::Bytes(b"http://tracker2.com"),
            Bencode::Bytes(b"http://tracker3.com"),
        ]);
        torrent_map.insert(&b"announce-list"[..], Bencode::List(vec![tier1, tier2]));

        let bencode = Bencode::Dict(torrent_map);
        let torrent =
            Torrent::try_from(bencode).expect("Failed to parse valid torrent with announce-list");

        let announce_list = torrent.announce_list.unwrap();
        assert_eq!(announce_list.len(), 2);
        assert_eq!(announce_list[0][0].as_str(), "http://tracker1.com/");
        assert_eq!(announce_list[1][1].as_str(), "http://tracker3.com/");
    }
}
