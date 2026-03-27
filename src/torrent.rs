use thiserror::Error;
use url::Url;

use crate::bencode::Bencode;

#[derive(Debug)]
pub struct Torrent {
    pub info: Info,
    pub info_hash: [u8; 20],
    pub announce: Option<Url>,
    pub announce_list: Option<Vec<Vec<Url>>>,
    pub creation_date: Option<u64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub encoding: Option<String>,
}

#[derive(Debug)]
pub struct Info {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<u8>,
    pub private: bool,
    pub file_mode: FileMode,
}

#[derive(Debug)]
pub enum FileMode {
    Single { length: u64, md5sum: Option<String> },
    Multi { files: Vec<FileInfo> },
}

#[derive(Debug)]
pub struct FileInfo {
    pub length: u64,
    pub md5sum: Option<String>,
    pub path: Vec<String>,
}

impl<'a> TryFrom<&'a Bencode<'a>> for FileInfo {
    type Error = crate::error::Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        todo!()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
}
