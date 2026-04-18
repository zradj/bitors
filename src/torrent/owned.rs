use url::Url;

use crate::{
    bencode::Bencode,
    torrent::{FileInfo, FileMode, Info, Torrent},
};

pub struct OwnedTorrent {
    pub info: OwnedInfo,
    pub announce: Option<Url>,
    pub announce_list: Option<Vec<Vec<Url>>>,
    pub creation_date: Option<u64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub encoding: Option<String>,
}

impl OwnedTorrent {
    pub fn trackers(&self) -> Vec<Vec<&Url>> {
        match (&self.announce, &self.announce_list) {
            (Some(url), None) => vec![vec![url]],
            (_, Some(tiers)) => tiers.iter().map(|tier| tier.iter().collect()).collect(),
            (None, None) => vec![],
        }
    }

    pub fn as_borrowed(&self) -> Torrent<'_> {
        Torrent {
            info: self.info.as_borrowed(),
            announce: self.announce.clone(),
            announce_list: self.announce_list.clone(),
            creation_date: self.creation_date,
            comment: self.comment.as_deref(),
            created_by: self.created_by.as_deref(),
            encoding: self.encoding.as_deref(),
        }
    }
}

impl<'a> From<Torrent<'a>> for OwnedTorrent {
    fn from(torrent: Torrent<'a>) -> Self {
        Self {
            info: torrent.info.into(),
            announce: torrent.announce,
            announce_list: torrent.announce_list,
            creation_date: torrent.creation_date,
            comment: torrent.comment.map(String::from),
            created_by: torrent.created_by.map(String::from),
            encoding: torrent.encoding.map(String::from),
        }
    }
}

impl<'a> TryFrom<&'a Bencode<'a>> for OwnedTorrent {
    type Error = super::Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        Torrent::try_from(bencode).map(Self::from)
    }
}

pub struct OwnedInfo {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub private: bool,
    pub file_mode: OwnedFileMode,
}

impl OwnedInfo {
    pub fn as_borrowed(&self) -> Info<'_> {
        Info {
            name: &self.name,
            piece_length: self.piece_length,
            pieces: &self.pieces,
            private: self.private,
            file_mode: self.file_mode.as_borrowed(),
        }
    }
}

impl<'a> From<Info<'a>> for OwnedInfo {
    fn from(info: Info<'a>) -> Self {
        Self {
            name: info.name.to_string(),
            piece_length: info.piece_length,
            pieces: info.pieces.to_vec(),
            private: info.private,
            file_mode: info.file_mode.into(),
        }
    }
}

impl<'a> TryFrom<&'a Bencode<'a>> for OwnedInfo {
    type Error = super::Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        Info::try_from(bencode).map(Self::from)
    }
}

pub enum OwnedFileMode {
    Single { length: u64, md5sum: Option<String> },
    Multi { files: Vec<OwnedFileInfo> },
}

impl OwnedFileMode {
    pub fn as_borrowed(&self) -> FileMode<'_> {
        match self {
            Self::Single { length, md5sum } => FileMode::Single {
                length: *length,
                md5sum: md5sum.as_deref(),
            },
            Self::Multi { files } => FileMode::Multi {
                files: files.iter().map(OwnedFileInfo::as_borrowed).collect(),
            },
        }
    }
}

impl<'a> From<FileMode<'a>> for OwnedFileMode {
    fn from(file_mode: FileMode<'a>) -> Self {
        match file_mode {
            FileMode::Single { length, md5sum } => Self::Single {
                length,
                md5sum: md5sum.map(String::from),
            },
            FileMode::Multi { files } => Self::Multi {
                files: files.into_iter().map(OwnedFileInfo::from).collect(),
            },
        }
    }
}

pub struct OwnedFileInfo {
    pub length: u64,
    pub md5sum: Option<String>,
    pub path: Vec<String>,
}

impl OwnedFileInfo {
    pub fn as_borrowed(&self) -> FileInfo<'_> {
        FileInfo {
            length: self.length,
            md5sum: self.md5sum.as_deref(),
            path: self.path.iter().map(|s| s.as_str()).collect(),
        }
    }
}

impl<'a> From<FileInfo<'a>> for OwnedFileInfo {
    fn from(file_info: FileInfo<'a>) -> Self {
        Self {
            length: file_info.length,
            md5sum: file_info.md5sum.map(String::from),
            path: file_info.path.into_iter().map(String::from).collect(),
        }
    }
}

impl<'a> TryFrom<&'a Bencode<'a>> for OwnedFileInfo {
    type Error = super::Error;

    fn try_from(bencode: &'a Bencode<'a>) -> Result<Self, Self::Error> {
        FileInfo::try_from(bencode).map(Self::from)
    }
}
