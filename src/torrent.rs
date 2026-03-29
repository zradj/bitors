use std::collections::BTreeMap;

use thiserror::Error;
use url::Url;

use crate::bencode::Bencode;

trait DictExt<'a> {
    fn require(&self, key: &[u8]) -> Result<&Bencode<'a>, Error>;
    fn get_str(&self, key: &[u8]) -> Result<Option<&str>, Error>;
    fn require_str(&self, key: &[u8]) -> Result<&str, Error>;
}

impl<'a> DictExt<'a> for BTreeMap<&'a [u8], Bencode<'a>> {
    fn require(&self, key: &[u8]) -> Result<&Bencode<'a>, Error> {
        self.get(key).ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }

    fn get_str(&self, key: &[u8]) -> Result<Option<&str>, Error> {
        self.get(key)
            .map(|b| -> Result<&str, Error> {
                let bytes = b.as_bytes()?;
                Ok(std::str::from_utf8(bytes)?)
            })
            .transpose()
    }

    fn require_str(&self, key: &[u8]) -> Result<&str, Error> {
        self.get_str(key)?.ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }
}

#[derive(Debug)]
pub struct Torrent<'a> {
    pub info: Info<'a>,
    pub announce: Option<Url>,
    pub announce_list: Option<Vec<Vec<Url>>>,
    pub creation_date: Option<u64>,
    pub comment: Option<&'a str>,
    pub created_by: Option<&'a str>,
    pub encoding: Option<&'a str>,
}

impl<'a> TryFrom<&'a Bencode<'a>> for Torrent<'a> {
    type Error = Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

        let info = map.require(b"info")?.try_into()?;

        let announce = map
            .get_str(b"announce")?
            .map(|s| Url::parse(&s))
            .transpose()?;

        let announce_list = map
            .get(b"announce-list".as_slice())
            .map(|b| -> Result<Vec<Vec<Url>>, Error> {
                b.as_list()?
                    .iter()
                    .map(|b| -> Result<Vec<Url>, Error> {
                        b.as_list()?
                            .iter()
                            .map(|b| -> Result<Url, Error> {
                                let s = std::str::from_utf8(b.as_bytes()?)?;
                                Ok(Url::parse(s)?)
                            })
                            .collect::<Result<Vec<Url>, _>>()
                    })
                    .collect::<Result<Vec<Vec<Url>>, _>>()
            })
            .transpose()?;

        if announce.is_none() && announce_list.is_none() {
            return Err(Error::MissingAnnounce);
        }

        let creation_date = map
            .get(b"creation date".as_slice())
            .map(|b| -> Result<u64, Error> {
                u64::try_from(b.as_int()?).map_err(|_| Error::IllegalFieldValue("creation date"))
            })
            .transpose()?;

        let comment = map.get_str(b"comment")?;

        let created_by = map.get_str(b"created by")?;

        let encoding = map.get_str(b"encoding")?;

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

#[derive(Debug)]
pub struct Info<'a> {
    pub name: &'a str,
    pub piece_length: u64,
    pub pieces: &'a [[u8; 20]],
    pub private: bool,
    pub file_mode: FileMode<'a>,
}

impl<'a> TryFrom<&'a Bencode<'a>> for Info<'a> {
    type Error = Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

        let name = map.require_str(b"name")?;

        let piece_length = u64::try_from(map.require(b"piece length")?.as_int()?)
            .map_err(|_| Error::IllegalFieldValue("piece length"))?;

        let pieces = map.require(b"pieces")?.as_bytes()?;
        let (pieces, []) = pieces.as_chunks() else {
            return Err(Error::InvalidPiecesLength);
        };

        let private = match map.get(b"private".as_slice()) {
            Some(b) => {
                let i = b.as_int()?;
                match i {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::IllegalFieldValue("private")),
                }
            }
            None => false,
        };

        let files = map.get(b"files".as_slice());

        let file_mode = match files {
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

                let md5sum = map.get_str(b"md5sum")?;

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

#[derive(Debug)]
pub enum FileMode<'a> {
    Single { length: u64, md5sum: Option<&'a str> },
    Multi { files: Vec<FileInfo<'a>> },
}

#[derive(Debug)]
pub struct FileInfo<'a> {
    pub length: u64,
    pub md5sum: Option<&'a str>,
    pub path: Vec<&'a str>,
}

impl<'a> TryFrom<&'a Bencode<'a>> for FileInfo<'a> {
    type Error = Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.as_dict()?;

        let length = u64::try_from(map.require(b"length")?.as_int()?)
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = map.get_str(b"md5sum")?;

        let path = map
            .require(b"path")?
            .as_list()?
            .iter()
            .map(|b| -> Result<&str, Self::Error> {
                let bytes = b.as_bytes()?;
                Ok(std::str::from_utf8(bytes)?)
            })
            .collect::<Result<Vec<&str>, _>>()?;

        Ok(Self {
            length,
            md5sum,
            path,
        })
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("URL parsing error: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("Length of the 'pieces' list must be a multiple of 20")]
    InvalidPiecesLength,
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("Illegal value in field '{0}'")]
    IllegalFieldValue(&'static str),
    #[error("No announce URLs found")]
    MissingAnnounce,
}
