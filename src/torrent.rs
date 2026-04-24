pub mod builder;
pub mod factory;

use std::{borrow::Cow, collections::BTreeMap, num::NonZeroU64, path::PathBuf};

use thiserror::Error;
use url::Url;

use crate::{
    bencode::Bencode,
    torrent::{
        builder::TorrentBuilder,
        factory::{TorrentFactory, state::Empty},
    },
};

/// An internal extension trait for `BTreeMap` to simplify extracting optional
/// and required fields from Bencoded dictionaries.
trait DictExt<'a> {
    /// Retrieves a reference to the value associated with the given byte slice key.
    fn opt(&self, key: &[u8]) -> Option<&Bencode<'a>>;

    /// Retrieves a reference to the value, returning an error if the key is missing.
    fn require(&self, key: &[u8]) -> Result<&Bencode<'a>, Error>;

    /// Retrieves a string value, returning `None` if the key is missing.
    /// Returns an error if the key exists but is not a valid string.
    fn opt_str(&self, key: &[u8]) -> Result<Option<&str>, Error>;

    /// Retrieves a string value, returning an error if the key is missing or not a string.
    fn require_str(&self, key: &[u8]) -> Result<&str, Error>;
}

impl<'a> DictExt<'a> for BTreeMap<&'a [u8], Bencode<'a>> {
    /// Removes the need in `b"...".as_slice()` in normal `BTreeMap::get` calls.
    fn opt(&self, key: &[u8]) -> Option<&Bencode<'a>> {
        self.get(key)
    }

    fn opt_str(&self, key: &[u8]) -> Result<Option<&str>, Error> {
        self.opt(key)
            .map(Bencode::as_str)
            .transpose()
            .map_err(Into::into)
    }

    fn require(&self, key: &[u8]) -> Result<&Bencode<'a>, Error> {
        self.opt(key).ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }

    fn require_str(&self, key: &[u8]) -> Result<&str, Error> {
        self.opt_str(key)?.ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }
}

/// Represents the root data structure of a parsed `.torrent` file.
///
/// This struct contains all the top-level metadata required by a BitTorrent client
/// to connect to trackers and understand the contents of the torrent.
#[derive(Debug)]
pub struct Torrent<'a> {
    /// A dictionary that describes the file(s) of the torrent.
    pub info: Info<'a>,
    /// The primary announce URL of the tracker.
    pub announce: Option<Url>,
    /// An optional list of backup trackers (Tiered trackers).
    pub announce_list: Option<Vec<Vec<Url>>>,
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
    #[must_use]
    pub fn builder(info: InfoBuf) -> TorrentBuilder {
        TorrentBuilder::new(info)
    }

    #[must_use] 
    pub fn factory() -> TorrentFactory<Empty> {
        TorrentFactory::new()
    }

    #[must_use]
    pub fn trackers(&self) -> Vec<Vec<&Url>> {
        match (&self.announce, &self.announce_list) {
            (Some(url), None) => vec![vec![url]],
            (_, Some(tiers)) => tiers.iter().map(|tier| tier.iter().collect()).collect(),
            (None, None) => vec![],
        }
    }

    /// Converts the `Torrent` struct back into a `Bencode` representation.
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn into_owned(self) -> TorrentBuf {
        TorrentBuf {
            info: self.info.into_owned(),
            announce: self.announce,
            announce_list: self.announce_list,
            creation_date: self.creation_date,
            comment: self.comment.map(|c| Cow::Owned(c.into_owned())),
            created_by: self.created_by.map(|c| Cow::Owned(c.into_owned())),
            encoding: self.encoding.map(|c| Cow::Owned(c.into_owned())),
        }
    }
}

impl<'a> TryFrom<&'a Bencode<'a>> for Torrent<'a> {
    type Error = Error;

    /// Attempts to parse a `Torrent` from a generic `Bencode` structure.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if:
    /// - The bencode is not a dictionary.
    /// - The required `info` field is missing or invalid.
    /// - Announce URLs are missing or malformed.
    /// - Data types for specific fields do not match the specification.
    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

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

        if !info.private && announce.is_none() && announce_list.is_none() {
            return Err(Error::MissingAnnounce);
        }

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
            creation_date,
            comment,
            created_by,
            encoding,
        })
    }
}

pub type TorrentBuf = Torrent<'static>;

/// Represents the `info` dictionary within a torrent file.
///
/// This structure holds the critical data describing the payload (the files to download),
/// including file names, piece sizes, and the cryptographic hashes used to verify data integrity.
#[derive(Debug)]
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
    /// Dictates whether this torrent represents a single file or a directory of multiple files.
    pub file_mode: FileMode<'a>,
}

impl Info<'_> {
    /// Converts the `Info` struct back into a `Bencode` representation.
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn into_owned(self) -> InfoBuf {
        InfoBuf {
            name: Cow::Owned(self.name.into_owned()),
            piece_length: self.piece_length,
            pieces: Cow::Owned(self.pieces.into_owned()),
            private: self.private,
            file_mode: self.file_mode.into_owned(),
        }
    }
}

impl<'a> TryFrom<&'a Bencode<'a>> for Info<'a> {
    type Error = Error;

    /// Attempts to parse an `Info` struct from a `Bencode` dictionary.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if required fields are missing, if `pieces` is not
    /// perfectly divisible by 20 bytes, or if data types are incorrect.
    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

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

        let file_mode = if let Some(b) = map.opt(b"files") {
            let files = b
                .as_list()?
                .iter()
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
            file_mode,
        })
    }
}

pub type InfoBuf = Info<'static>;

/// Defines the structure of the payload contained within the torrent.
///
/// BitTorrent supports both single-file payloads and multi-file directory payloads.
#[derive(Debug)]
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
    #[must_use]
    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single { .. })
    }

    #[must_use]
    pub fn is_multi(&self) -> bool {
        !self.is_single()
    }

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

pub type FileModeBuf = FileMode<'static>;

/// Metadata for a single file within a multi-file torrent.
#[derive(Debug)]
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

impl<'a> TryFrom<&'a Bencode<'a>> for FileInfo<'a> {
    type Error = Error;

    /// Attempts to parse a `FileInfo` struct from a `Bencode` dictionary.
    ///
    /// # Errors
    ///
    /// Returns an error if the `length` or `path` fields are missing or invalid.
    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

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
        let info = Info::try_from(&bencode).expect("Failed to parse valid single-file info");

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
        let info = Info::try_from(&bencode).expect("Failed to parse valid multi-file info");

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
        let err =
            Info::try_from(&bencode).expect_err("Should have failed on invalid pieces length");

        assert!(matches!(err, Error::InvalidPiecesLength));
    }

    #[test]
    fn test_torrent_missing_announce() {
        // Build a valid Info dict
        let mut info_map = BTreeMap::new();
        info_map.insert(&b"name"[..], Bencode::Bytes(b"test"));
        info_map.insert(&b"piece length"[..], Bencode::Int(262_144));
        info_map.insert(&b"length"[..], Bencode::Int(1024));
        let pieces = dummy_pieces();
        info_map.insert(&b"pieces"[..], Bencode::Bytes(&pieces));

        let mut torrent_map = BTreeMap::new();
        torrent_map.insert(&b"info"[..], Bencode::Dict(info_map));
        // Intentionally leaving out 'announce' and 'announce-list'

        let bencode = Bencode::Dict(torrent_map);
        let err =
            Torrent::try_from(&bencode).expect_err("Should have failed due to missing announce");

        assert!(matches!(err, Error::MissingAnnounce));
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
        let torrent = Torrent::try_from(&bencode).expect("Failed to parse valid torrent");

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
            Torrent::try_from(&bencode).expect("Failed to parse valid torrent with announce-list");

        let announce_list = torrent.announce_list.unwrap();
        assert_eq!(announce_list.len(), 2);
        assert_eq!(announce_list[0][0].as_str(), "http://tracker1.com/");
        assert_eq!(announce_list[1][1].as_str(), "http://tracker3.com/");
    }
}
