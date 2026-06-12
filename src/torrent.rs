pub mod builder;

use std::{
    borrow::Cow,
    collections::BTreeMap,
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use bitflags::bitflags;
use sha1::{
    Digest, Sha1,
    digest::{Output, Update},
};
use sha2::Sha256;
use thiserror::Error;
use url::Url;

use crate::{
    bencode::Bencode,
    magnet::MagnetLink,
    torrent::builder::{TorrentBuilder, state::Empty},
};

trait DictExt<'a> {
    fn opt(&mut self, key: &[u8]) -> Option<Bencode<'a>>;

    fn require(&mut self, key: &[u8]) -> Result<Bencode<'a>, Error>;

    fn opt_str(&mut self, key: &[u8]) -> Result<Option<&'a str>, Error>;

    fn require_str(&mut self, key: &[u8]) -> Result<&'a str, Error>;
}

impl<'a> DictExt<'a> for BTreeMap<&'a [u8], Bencode<'a>> {
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

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Torrent<'a> {
    pub info: Info<'a>,

    pub announce: Option<Url>,

    pub announce_list: Option<Vec<Vec<Url>>>,

    pub url_list: Option<Vec<Url>>,

    pub creation_date: Option<u64>,

    pub comment: Option<Cow<'a, str>>,

    pub created_by: Option<Cow<'a, str>>,

    pub encoding: Option<Cow<'a, str>>,
    pub v2_ext: Option<TorrentV2Ext<'a>>,
}

impl Torrent<'_> {
    #[must_use]
    pub fn builder() -> TorrentBuilder<Empty> {
        TorrentBuilder::new()
    }

    #[must_use]
    pub fn is_v1(&self) -> bool {
        self.info.v1.is_some()
    }

    #[must_use]
    pub fn is_v2(&self) -> bool {
        self.info.v2.is_some() && self.v2_ext.is_some()
    }

    #[must_use]
    pub fn is_hybrid(&self) -> bool {
        self.is_v1() && self.is_v2()
    }

    #[must_use]
    pub fn trackers(&self) -> Vec<Vec<&Url>> {
        match (&self.announce, &self.announce_list) {
            (Some(url), None) => vec![vec![url]],
            (_, Some(tiers)) => tiers.iter().map(|tier| tier.iter().collect()).collect(),
            (None, None) => vec![],
        }
    }

    #[must_use]
    pub fn info_hash_v1(&self) -> Option<[u8; 20]> {
        self.info.info_hash_v1()
    }

    #[must_use]
    pub fn info_hash_v2(&self) -> Option<[u8; 32]> {
        self.info.info_hash_v2()
    }

    #[must_use]
    pub fn total_size(&self) -> u64 {
        match &self.info.v1 {
            Some(InfoV1 { file_mode, .. }) => match file_mode {
                FileMode::Single { length, .. } => *length,
                FileMode::Multi { files } => files.iter().map(|f| f.length).sum(),
            },
            #[expect(clippy::missing_panics_doc, reason = "infallible")]
            None => self.info.v2.as_ref().unwrap().file_tree.total_size(),
        }
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        match &self.info.v1 {
            Some(InfoV1 { file_mode, .. }) => match file_mode {
                FileMode::Single { .. } => 1,
                FileMode::Multi { files } => files.len(),
            },
            #[expect(clippy::missing_panics_doc, reason = "infallible")]
            None => self.info.v2.as_ref().unwrap().file_tree.file_count(),
        }
    }

    /// Generates a [`MagnetLink`] for this torrent.
    ///
    /// A magnet link contains the torrent's info hash(es). It can also optionally contain the name,
    /// the total size, and the trackers.
    ///
    /// This is equivalent to [`MagnetLink::from`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use bitors::Torrent;
    ///
    /// let torrent = Torrent::builder().add_path("my_file").unwrap().build().unwrap();
    /// println!("{}", torrent.magnet_link());
    /// // Hybrid torrent: magnet:?xt=urn:btih:<v1 hash>&xt=urn:btmh:<v2 hash>...
    /// ```
    #[must_use]
    pub fn magnet_link(&self) -> MagnetLink {
        MagnetLink::from(self)
    }

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
            url_list: self.url_list,
            creation_date: self.creation_date,
            comment: self.comment.map(|c| Cow::Owned(c.into_owned())),
            created_by: self.created_by.map(|c| Cow::Owned(c.into_owned())),
            encoding: self.encoding.map(|c| Cow::Owned(c.into_owned())),
            v2_ext: self.v2_ext.map(TorrentV2Ext::into_owned),
        }
    }

    fn hybrid_mismatch(&self) -> bool {
        if !self.is_hybrid() {
            return false;
        }

        let file_tree = &self.info.v2.as_ref().unwrap().file_tree;

        match &self.info.v1.as_ref().unwrap().file_mode {
            FileMode::Single { .. } => {
                file_tree.file_paths().as_slice() != [Path::new(self.info.name.as_ref())]
            }
            FileMode::Multi { files } => {
                let mut v1_files: Vec<PathBuf> = files
                    .iter()
                    .filter(|f| {
                        !f.attr
                            .as_ref()
                            .is_some_and(|a| a.flags.contains(FileInfoAttrFlags::PADDING))
                    })
                    .map(FileInfo::full_path)
                    .collect();
                v1_files.sort();

                file_tree.file_paths() != v1_files
            }
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for Torrent<'a> {
    type Error = Error;

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

        let v2_ext = match map.opt(b"piece layers") {
            Some(b) => {
                let mut piece_layers = BTreeMap::new();

                for (k, v) in b.into_dict()? {
                    let key = k
                        .try_into()
                        .map_err(|_| Error::IllegalFieldValue("piece layers (key)"))?;
                    let value = v.as_bytes()?;
                    piece_layers.insert(Cow::Borrowed(key), Cow::Borrowed(value));
                }

                Some(TorrentV2Ext { piece_layers })
            }
            None => None,
        };

        let res = Self {
            info,
            announce,
            announce_list,
            url_list,
            creation_date,
            comment,
            created_by,
            encoding,
            v2_ext,
        };

        if !res.is_v1() && !res.is_v2() {
            Err(Error::UnrecognizedFormat)
        } else if res.hybrid_mismatch() {
            Err(Error::HybridMismatch)
        } else {
            Ok(res)
        }
    }
}

pub type TorrentBuf = Torrent<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TorrentV2Ext<'a> {
    pub piece_layers: BTreeMap<Cow<'a, [u8; 32]>, Cow<'a, [u8]>>,
}

impl TorrentV2Ext<'_> {
    #[must_use]
    pub fn into_owned(self) -> TorrentV2ExtBuf {
        let mut piece_layers = BTreeMap::new();
        for (k, v) in self.piece_layers {
            piece_layers.insert(Cow::Owned(k.into_owned()), Cow::Owned(v.into_owned()));
        }

        TorrentV2Ext { piece_layers }
    }
}

pub type TorrentV2ExtBuf = TorrentV2Ext<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Info<'a> {
    pub name: Cow<'a, str>,

    pub piece_length: NonZeroU64,

    pub private: bool,

    pub source: Option<Cow<'a, str>>,

    pub v1: Option<InfoV1<'a>>,
    pub v2: Option<InfoV2<'a>>,
}

impl Info<'_> {
    #[must_use]
    pub fn info_hash_v1(&self) -> Option<[u8; 20]> {
        self.v1.as_ref()?;
        Some(self.info_hash(Sha1::new()).into())
    }

    #[must_use]
    pub fn info_hash_v2(&self) -> Option<[u8; 32]> {
        self.v2.as_ref()?;
        Some(self.info_hash(Sha256::new()).into())
    }

    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn into_owned(self) -> InfoBuf {
        InfoBuf {
            name: Cow::Owned(self.name.into_owned()),
            piece_length: self.piece_length,
            private: self.private,
            source: self.source.map(|c| Cow::Owned(c.into_owned())),
            v1: self.v1.map(InfoV1::into_owned),
            v2: self.v2.map(InfoV2::into_owned),
        }
    }

    fn info_hash<D: Digest + Update>(&self, hash_func: D) -> Output<D> {
        let mut hasher = digest_io::IoWrapper(hash_func);
        let _ = self.to_bencode().encode_to_writer(&mut hasher);
        hasher.0.finalize()
    }
}

impl<'a> TryFrom<Bencode<'a>> for Info<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut map = bencode.into_dict()?;

        // --- Extract common fields ---
        let name = map.require_str(b"name")?;
        let piece_length = map
            .require(b"piece length")?
            .as_int()?
            .try_into()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::IllegalFieldValue("piece length"))?;

        let private = match map.opt(b"private") {
            Some(b) => match b.as_int()? {
                0 => false,
                1 => true,
                _ => return Err(Error::IllegalFieldValue("private")),
            },
            None => false,
        };
        let source = map.opt_str(b"source")?.map(Cow::Borrowed);

        // --- Extract v1 fields ---
        let pieces = match map.opt(b"pieces") {
            Some(b) => {
                let pieces = b.as_bytes()?;
                let (pieces, []) = pieces.as_chunks() else {
                    return Err(Error::InvalidPiecesLength);
                };
                Some(Cow::Borrowed(pieces))
            }
            None => None,
        };

        let file_mode = if let Some(b) = map.opt(b"files") {
            let files = b
                .into_list()?
                .into_iter()
                .map(FileInfo::try_from)
                .collect::<Result<Vec<FileInfo>, _>>()?;

            Some(FileMode::Multi { files })
        } else if let Some(b) = map.opt(b"length") {
            let length = b
                .as_int()?
                .try_into()
                .map_err(|_| Error::IllegalFieldValue("length"))?;

            let md5sum = map.opt_str(b"md5sum")?.map(Cow::Borrowed);

            Some(FileMode::Single { length, md5sum })
        } else {
            None
        };

        let v1 = match (pieces, file_mode) {
            (None, None) => None,
            (Some(pieces), Some(file_mode)) => Some(InfoV1 { pieces, file_mode }),
            _ => return Err(Error::MalformedV1),
        };

        // --- Extract v2 fields ---
        let meta_version = match map.opt(b"meta version") {
            Some(b) => {
                let meta_version = b
                    .as_int()?
                    .try_into()
                    .map_err(|_| Error::IllegalFieldValue("meta version"))?;

                if meta_version != 2 {
                    return Err(Error::IllegalFieldValue("meta version"));
                }

                Some(meta_version)
            }
            None => None,
        };

        let file_tree = map.opt(b"file tree").map(FileTree::try_from).transpose()?;

        let v2 = match (meta_version, file_tree) {
            (None, None) => None,
            (Some(meta_version), Some(file_tree)) => Some(InfoV2 {
                meta_version,
                file_tree,
            }),
            _ => return Err(Error::MalformedV2),
        };

        if v2.is_some() && !piece_length.is_power_of_two() {
            Err(Error::IllegalFieldValue("piece length"))
        } else {
            Ok(Self {
                name: Cow::Borrowed(name),
                piece_length,
                private,
                source,
                v1,
                v2,
            })
        }
    }
}

pub type InfoBuf = Info<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV1<'a> {
    pub pieces: Cow<'a, [[u8; 20]]>,
    pub file_mode: FileMode<'a>,
}

impl InfoV1<'_> {
    #[must_use]
    pub fn into_owned(self) -> InfoV1Buf {
        InfoV1Buf {
            pieces: Cow::Owned(self.pieces.into_owned()),
            file_mode: self.file_mode.into_owned(),
        }
    }
}

pub type InfoV1Buf = InfoV1<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV2<'a> {
    pub meta_version: u8,
    pub file_tree: FileTree<'a>,
}

impl InfoV2<'_> {
    #[must_use]
    pub fn into_owned(self) -> InfoV2Buf {
        InfoV2Buf {
            meta_version: self.meta_version,
            file_tree: self.file_tree.into_owned(),
        }
    }
}

pub type InfoV2Buf = InfoV2<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileMode<'a> {
    Single {
        length: u64,

        md5sum: Option<Cow<'a, str>>,
    },

    Multi {
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

pub type FileModeBuf = FileMode<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfoAttr {
    pub flags: FileInfoAttrFlags,
    pub encoded: [u8; 4],
    pub len: usize,
}

impl FileInfoAttr {
    #[must_use]
    pub fn new(flags: FileInfoAttrFlags) -> Self {
        let mut encoded = [0u8; 4];
        let mut len = 0;

        for flag in flags.iter() {
            encoded[len] = match flag {
                FileInfoAttrFlags::SYMLINK => b'l',
                FileInfoAttrFlags::EXEC => b'x',
                FileInfoAttrFlags::HIDDEN => b'h',
                FileInfoAttrFlags::PADDING => b'p',
                _ => unreachable!(),
            };
            len += 1;
        }

        Self {
            flags,
            encoded,
            len,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.encoded[..self.len]
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileInfoAttr {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut flags = FileInfoAttrFlags::empty();

        for &c in bencode.as_bytes()? {
            match c {
                b'l' => flags.insert(FileInfoAttrFlags::SYMLINK),
                b'x' => flags.insert(FileInfoAttrFlags::EXEC),
                b'h' => flags.insert(FileInfoAttrFlags::HIDDEN),
                b'p' => flags.insert(FileInfoAttrFlags::PADDING),
                _ => return Err(Error::IllegalFieldValue("attr")),
            }
        }

        Ok(Self::new(flags))
    }
}

bitflags! {
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub struct FileInfoAttrFlags: u8 {
        const SYMLINK = 0b0001;
        const EXEC = 0b0010;
        const HIDDEN = 0b0100;
        const PADDING = 0b1000;
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfo<'a> {
    pub attr: Option<FileInfoAttr>,

    pub length: u64,

    pub md5sum: Option<Cow<'a, str>>,

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

    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn into_owned(self) -> FileInfoBuf {
        FileInfoBuf {
            attr: self.attr,
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

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut map = bencode.into_dict()?;

        let attr = map.opt(b"attr").map(FileInfoAttr::try_from).transpose()?;

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
            attr,
            length,
            md5sum,
            path,
        })
    }
}

pub type FileInfoBuf = FileInfo<'static>;

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct FileTree<'a>(pub BTreeMap<Cow<'a, str>, FileTreeNode<'a>>);

impl FileTree<'_> {
    #[must_use]
    pub fn file_paths(&self) -> Vec<PathBuf> {
        let mut res = Vec::with_capacity(self.file_count());
        self.collect_paths(Path::new(""), &mut res);
        res
    }

    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.0
            .values()
            .map(|node| match node {
                FileTreeNode::Directory(file_tree) => file_tree.total_size(),
                FileTreeNode::File(file_leaf) => file_leaf.length,
            })
            .sum()
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.0
            .values()
            .map(|node| match node {
                FileTreeNode::Directory(file_tree) => file_tree.file_count(),
                FileTreeNode::File(_) => 1,
            })
            .sum()
    }

    #[must_use]
    pub fn into_owned(self) -> FileTreeBuf {
        let mut res = BTreeMap::new();

        for (k, v) in self.0 {
            res.insert(Cow::Owned(k.into_owned()), v.into_owned());
        }

        FileTree(res)
    }

    fn collect_paths(&self, prefix: &Path, res: &mut Vec<PathBuf>) {
        for (key, node) in &self.0 {
            let path = prefix.join(key.as_ref());
            match node {
                FileTreeNode::Directory(file_tree) => file_tree.collect_paths(&path, res),
                FileTreeNode::File(_) => res.push(path),
            }
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileTree<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.into_dict()?;
        let mut res = BTreeMap::new();

        for (key, value) in map {
            let key = std::str::from_utf8(key)?;

            let node = if key.is_empty() {
                FileTreeNode::File(FileLeaf::try_from(value)?)
            } else {
                FileTreeNode::Directory(Self::try_from(value)?)
            };

            res.insert(Cow::Borrowed(key), node);
        }

        Ok(Self(res))
    }
}

pub type FileTreeBuf = FileTree<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileTreeNode<'a> {
    Directory(FileTree<'a>),
    File(FileLeaf<'a>),
}

impl FileTreeNode<'_> {
    #[must_use]
    pub fn into_owned(self) -> FileTreeNodeBuf {
        match self {
            Self::Directory(dir) => FileTreeNodeBuf::Directory(dir.into_owned()),
            Self::File(file) => FileTreeNodeBuf::File(file.into_owned()),
        }
    }
}

pub type FileTreeNodeBuf = FileTreeNode<'static>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileLeaf<'a> {
    pub length: u64,
    pub pieces_root: Option<Cow<'a, [u8; 32]>>,
}

impl FileLeaf<'_> {
    #[must_use]
    pub fn into_owned(self) -> FileLeafBuf {
        FileLeafBuf {
            length: self.length,
            pieces_root: self.pieces_root.map(|c| Cow::Owned(c.into_owned())),
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileLeaf<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let map = bencode.into_dict()?;

        let length: u64 = map
            .get(b"length".as_slice())
            .ok_or(Error::MissingField("length".to_string()))?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;
        let pieces_root = map.get(b"pieces root".as_slice());

        match (length, pieces_root) {
            (0, None) => Ok(Self {
                length: 0,
                pieces_root: None,
            }),
            (1.., Some(b)) => {
                let pieces_root = b
                    .as_bytes()?
                    .try_into()
                    .map_err(|_| Error::IllegalFieldValue("pieces root"))?;
                Ok(Self {
                    length,
                    pieces_root: Some(Cow::Borrowed(pieces_root)),
                })
            }
            _ => Err(Error::InvalidFileTree),
        }
    }
}

pub type FileLeafBuf = FileLeaf<'static>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("UTF-8 encoding error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("URL parsing error: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("Missing required field: {0}")]
    MissingField(String),

    #[error("Illegal value in field '{0}'")]
    IllegalFieldValue(&'static str),

    #[error("Length of the 'pieces' list must be a multiple of 20")]
    InvalidPiecesLength,

    #[error("Invalid file tree")]
    InvalidFileTree,

    #[error("No announce URLs found")]
    MissingAnnounce,

    #[error("Malformed BitTorrent v1 torrent")]
    MalformedV1,

    #[error("Malformed BitTorrent v2 torrent")]
    MalformedV2,

    #[error("File information mismatch between v1 and v2 fields in hybrid torrent")]
    HybridMismatch,

    #[error("Unrecognized torrent format")]
    UnrecognizedFormat,
}
