use std::collections::BTreeMap;

use thiserror::Error;
use url::Url;

use crate::bencode::Bencode;

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

    fn opt_str(&self, key: &[u8]) -> Result<Option<&str>, Error> {
        self.opt(key)
            .map(Bencode::as_str)
            .transpose()
            .map_err(Into::into)
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
    pub comment: Option<&'a str>,
    /// Name and version of the program used to create the .torrent.
    pub created_by: Option<&'a str>,
    /// The string encoding format used to generate the pieces part of the info dictionary 
    /// in the .torrent metainfo file (e.g., "UTF-8").
    pub encoding: Option<&'a str>,
}

impl<'a> Torrent<'a> {
    /// Converts the `Torrent` struct back into a `Bencode` representation.
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
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

        let info = map.require(b"info")?.try_into()?;

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

        // TODO: add support for DHT
        if announce.is_none() && announce_list.is_none() {
            return Err(Error::MissingAnnounce);
        }

        let creation_date = map
            .opt(b"creation date")
            .map(|b| -> Result<u64, Error> {
                u64::try_from(b.as_int()?).map_err(|_| Error::IllegalFieldValue("creation date"))
            })
            .transpose()?;

        let comment = map.opt_str(b"comment")?;

        let created_by = map.opt_str(b"created by")?;

        let encoding = map.opt_str(b"encoding")?;

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

/// Represents the `info` dictionary within a torrent file.
///
/// This structure holds the critical data describing the payload (the files to download),
/// including file names, piece sizes, and the cryptographic hashes used to verify data integrity.
#[derive(Debug)]
pub struct Info<'a> {
    /// In the single file case, the name of the file. 
    /// In the multiple file case, the name of the directory in which to store all the files.
    pub name: &'a str,
    /// The number of bytes in each piece the files are split into.
    pub piece_length: u64,
    /// An array of 20-byte SHA1 hashes, one for each piece in the torrent.
    pub pieces: &'a [[u8; 20]],
    /// If true, the client must not obtain peer data from the DHT or PEX. 
    /// It must only rely on the specified tracker(s).
    pub private: bool,
    /// Dictates whether this torrent represents a single file or a directory of multiple files.
    pub file_mode: FileMode<'a>,
}

impl<'a> Info<'a> {
    /// Converts the `Info` struct back into a `Bencode` representation.
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
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

        let piece_length = u64::try_from(map.require(b"piece length")?.as_int()?)
            .map_err(|_| Error::IllegalFieldValue("piece length"))?;

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

        let file_mode = match map.opt(b"files") {
            Some(b) => {
                let files = b
                    .as_list()?
                    .iter()
                    .map(FileInfo::try_from)
                    .collect::<Result<Vec<FileInfo>, _>>()?;

                FileMode::Multi { files }
            }
            None => {
                let length = u64::try_from(map.require(b"length")?.as_int()?)
                    .map_err(|_| Error::IllegalFieldValue("length"))?;

                let md5sum = map.opt_str(b"md5sum")?;

                FileMode::Single { length, md5sum }
            }
        };

        Ok(Self {
            name,
            piece_length,
            pieces,
            private,
            file_mode,
        })
    }
}

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
        md5sum: Option<&'a str>,
    },
    /// Represents a torrent containing a directory of multiple files.
    Multi {
        /// A list detailing each individual file inside the torrent directory.
        files: Vec<FileInfo<'a>>,
    },
}

/// Metadata for a single file within a multi-file torrent.
#[derive(Debug)]
pub struct FileInfo<'a> {
    /// The length of the file in bytes.
    pub length: u64,
    /// An optional 32-character hexadecimal string corresponding to the MD5 sum of the file.
    pub md5sum: Option<&'a str>,
    /// A list containing one or more string elements that together represent the path and filename.
    /// Each element corresponds to a directory name or (for the last element) the filename.
    pub path: Vec<&'a str>,
}

impl<'a> FileInfo<'a> {
    /// Converts the `FileInfo` struct back into a `Bencode` representation.
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
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

        let length = u64::try_from(map.require(b"length")?.as_int()?)
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = map.opt_str(b"md5sum")?;

        let path = map
            .require(b"path")?
            .as_list()?
            .iter()
            .map(Bencode::as_str)
            .collect::<Result<Vec<&str>, _>>()?;

        Ok(Self {
            length,
            md5sum,
            path,
        })
    }
}

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
