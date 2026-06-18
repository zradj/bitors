pub mod builder;

use bitflags::bitflags;
use rand::seq::SliceRandom;
use sha1::{
    Digest, Sha1,
    digest::{Output, Update},
};
use sha2::Sha256;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    num::NonZeroU64,
    ops::Deref,
    path::{Path, PathBuf},
};
use thiserror::Error;
use url::Url;

use crate::{
    bencode::Bencode,
    magnet::MagnetLink,
    torrent::builder::{TorrentBuilder, state::Empty},
};

pub type TorrentBuf = Torrent<'static>;
pub type TorrentMetaBuf = TorrentMeta<'static>;
pub type PieceLayersBuf = PieceLayers<'static>;
pub type InfoBuf<T> = Info<'static, T>;
pub type InfoV1Buf = InfoV1<'static>;
pub type InfoV2Buf = InfoV2<'static>;
pub type InfoHybridBuf = InfoHybrid<'static>;
pub type FileModeBuf = FileMode<'static>;
pub type FileInfoBuf = FileInfo<'static>;
pub type FileTreeBuf = FileTree<'static>;
pub type FileTreeNodeBuf = FileTreeNode<'static>;
pub type FileLeafBuf = FileLeaf<'static>;

fn extract_info_v1_fields<'a>(
    dict: &mut BTreeMap<&'a [u8], Bencode<'a>>,
) -> Result<Option<InfoV1<'a>>, Error> {
    let pieces = match dict.opt(b"pieces") {
        Some(b) => {
            let pieces = b.as_bytes()?;
            let (pieces, []) = pieces.as_chunks() else {
                return Err(Error::InvalidPiecesLength);
            };
            Some(Cow::Borrowed(pieces))
        }
        None => None,
    };

    let file_mode = if let Some(b) = dict.opt(b"files") {
        let files = b
            .into_list()?
            .into_iter()
            .map(FileInfo::try_from)
            .collect::<Result<Vec<FileInfo>, _>>()?;

        Some(FileMode::Multi { files })
    } else if let Some(b) = dict.opt(b"length") {
        let length = b
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = dict.opt_str(b"md5sum")?.map(Cow::Borrowed);

        Some(FileMode::Single { length, md5sum })
    } else {
        None
    };

    match (pieces, file_mode) {
        (None, None) => Ok(None),
        (Some(pieces), Some(file_mode)) => Ok(Some(InfoV1 { pieces, file_mode })),
        _ => Err(Error::MalformedV1),
    }
}

fn extract_info_v2_fields<'a>(
    dict: &mut BTreeMap<&'a [u8], Bencode<'a>>,
) -> Result<Option<InfoV2<'a>>, Error> {
    let has_meta_version = match dict.opt(b"meta version") {
        Some(b) => {
            let meta_version = b.as_int()?;
            if meta_version != 2 {
                return Err(Error::IllegalFieldValue("meta version"));
            }
            true
        }
        None => false,
    };

    let file_tree = dict.opt(b"file tree").map(FileTree::try_from).transpose()?;

    match (has_meta_version, file_tree) {
        (false, None) => Ok(None),
        (true, Some(file_tree)) => Ok(Some(InfoV2 { file_tree })),
        _ => Err(Error::MalformedV2),
    }
}

pub trait IntoOwned {
    type Owned: 'static + IntoOwned;
    fn into_owned(self) -> Self::Owned;
}

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
    pub tracker: Option<Url>,
    pub tracker_tiers: Option<Vec<TrackerTier>>,
    pub web_seeds: Option<Vec<Url>>,
    pub creation_date: Option<u64>,
    pub comment: Option<Cow<'a, str>>,
    pub created_by: Option<Cow<'a, str>>,
    pub encoding: Option<Cow<'a, str>>,
    pub meta: TorrentMeta<'a>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TorrentMeta<'a> {
    V1 {
        info: Info<'a, InfoV1<'a>>,
    },
    V2 {
        info: Info<'a, InfoV2<'a>>,
        piece_layers: PieceLayers<'a>,
    },
    Hybrid {
        info: Info<'a, InfoHybrid<'a>>,
        piece_layers: PieceLayers<'a>,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Info<'a, T: 'a + IntoOwned> {
    pub name: Cow<'a, str>,
    pub piece_length: NonZeroU64,
    pub private: bool,
    pub source: Option<Cow<'a, str>>,
    pub kind: T,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum ParsedInfo<'a> {
    V1(Info<'a, InfoV1<'a>>),
    V2(Info<'a, InfoV2<'a>>),
    Hybrid(Info<'a, InfoHybrid<'a>>),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV1<'a> {
    pub pieces: Cow<'a, [[u8; 20]]>,
    pub file_mode: FileMode<'a>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV2<'a> {
    pub file_tree: FileTree<'a>,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoHybrid<'a> {
    pub v1: InfoV1<'a>,
    pub v2: InfoV2<'a>,
}

impl<'a> TryFrom<Bencode<'a>> for Torrent<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let parsed_info = ParsedInfo::try_from(dict.require(b"info")?)?;

        let piece_layers = match dict.opt(b"piece layers") {
            Some(b) => {
                let mut piece_layers = BTreeMap::new();

                for (k, v) in b.into_dict()? {
                    let key = k
                        .try_into()
                        .map_err(|_| Error::IllegalFieldValue("piece layers (key)"))?;
                    let value = v.as_bytes()?;
                    piece_layers.insert(Cow::Borrowed(key), Cow::Borrowed(value));
                }

                Some(PieceLayers(piece_layers))
            }
            None => None,
        };

        let meta = match (parsed_info, piece_layers) {
            (ParsedInfo::V1(info), None) => TorrentMeta::V1 { info },
            (ParsedInfo::V2(info), Some(piece_layers)) => TorrentMeta::V2 { info, piece_layers },
            (ParsedInfo::Hybrid(info), Some(piece_layers)) => {
                TorrentMeta::Hybrid { info, piece_layers }
            }
            (ParsedInfo::V1(_), Some(_)) | (ParsedInfo::V2(_) | ParsedInfo::Hybrid(_), None) => {
                return Err(Error::MalformedV2);
            }
        };

        let tracker = dict.opt_str(b"announce")?.map(Url::parse).transpose()?;

        let tracker_tiers = dict
            .opt(b"announce-list")
            .map(|b| {
                b.as_list()?
                    .iter()
                    .map(|b| -> Result<TrackerTier, Error> {
                        let tracker_tier = b
                            .as_list()?
                            .iter()
                            .map(|b| Ok::<Url, Error>(Url::parse(b.as_str()?)?))
                            .collect::<Result<Vec<Url>, _>>()?;
                        Ok(TrackerTier(tracker_tier))
                    })
                    .collect::<Result<Vec<TrackerTier>, _>>()
            })
            .transpose()?;

        let web_seeds = dict
            .opt(b"url-list")
            .map(|b| {
                b.as_list()?
                    .iter()
                    .map(|b| Ok::<Url, Error>(Url::parse(b.as_str()?)?))
                    .collect::<Result<Vec<Url>, _>>()
            })
            .transpose()?;

        let creation_date = dict
            .opt(b"creation date")
            .map(|b| -> Result<u64, Error> {
                b.as_int()?
                    .try_into()
                    .map_err(|_| Error::IllegalFieldValue("creation date"))
            })
            .transpose()?;

        let comment = dict.opt_str(b"comment")?.map(Cow::Borrowed);

        let created_by = dict.opt_str(b"created by")?.map(Cow::Borrowed);

        let encoding = dict.opt_str(b"encoding")?.map(Cow::Borrowed);

        let res = Self {
            tracker,
            tracker_tiers,
            web_seeds,
            creation_date,
            comment,
            created_by,
            encoding,
            meta,
        };

        if res.hybrid_mismatch() {
            Err(Error::HybridMismatch)
        } else {
            Ok(res)
        }
    }
}

impl<'a> TryFrom<Bencode<'a>> for ParsedInfo<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        // --- Extract common fields ---
        let name = Cow::Borrowed(dict.require_str(b"name")?);
        let piece_length = dict
            .require(b"piece length")?
            .as_int()?
            .try_into()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::IllegalFieldValue("piece length"))?;

        let private = match dict.opt(b"private") {
            Some(b) => match b.as_int()? {
                0 => false,
                1 => true,
                _ => return Err(Error::IllegalFieldValue("private")),
            },
            None => false,
        };
        let source = dict.opt_str(b"source")?.map(Cow::Borrowed);

        let v1 = extract_info_v1_fields(&mut dict)?;
        let v2 = extract_info_v2_fields(&mut dict)?;

        match (v1, v2) {
            (Some(v1), Some(v2)) => {
                if !piece_length.is_power_of_two() || piece_length.get() < 16 * 1024 {
                    return Err(Error::IllegalFieldValue("piece length"));
                }

                Ok(ParsedInfo::Hybrid(Info {
                    name,
                    piece_length,
                    private,
                    source,
                    kind: InfoHybrid { v1, v2 },
                }))
            }
            (Some(v1), None) => Ok(ParsedInfo::V1(Info {
                name,
                piece_length,
                private,
                source,
                kind: v1,
            })),
            (None, Some(v2)) => {
                if !piece_length.is_power_of_two() || piece_length.get() < 16 * 1024 {
                    return Err(Error::IllegalFieldValue("piece length"));
                }

                Ok(ParsedInfo::V2(Info {
                    name,
                    piece_length,
                    private,
                    source,
                    kind: v2,
                }))
            }
            (None, None) => Err(Error::UnrecognizedFormat),
        }
    }
}

impl IntoOwned for Torrent<'_> {
    type Owned = TorrentBuf;

    fn into_owned(self) -> Self::Owned {
        TorrentBuf {
            tracker: self.tracker,
            tracker_tiers: self.tracker_tiers,
            web_seeds: self.web_seeds,
            creation_date: self.creation_date,
            comment: self.comment.map(|c| Cow::Owned(c.into_owned())),
            created_by: self.created_by.map(|c| Cow::Owned(c.into_owned())),
            encoding: self.encoding.map(|c| Cow::Owned(c.into_owned())),
            meta: self.meta.into_owned(),
        }
    }
}

impl IntoOwned for TorrentMeta<'_> {
    type Owned = TorrentMetaBuf;

    fn into_owned(self) -> Self::Owned {
        match self {
            Self::V1 { info } => TorrentMetaBuf::V1 {
                info: info.into_owned(),
            },
            Self::V2 { info, piece_layers } => TorrentMetaBuf::V2 {
                info: info.into_owned(),
                piece_layers: piece_layers.into_owned(),
            },
            Self::Hybrid { info, piece_layers } => TorrentMetaBuf::Hybrid {
                info: info.into_owned(),
                piece_layers: piece_layers.into_owned(),
            },
        }
    }
}

impl<T: IntoOwned> IntoOwned for Info<'_, T> {
    type Owned = InfoBuf<T::Owned>;

    fn into_owned(self) -> Self::Owned {
        InfoBuf {
            name: Cow::Owned(self.name.into_owned()),
            piece_length: self.piece_length,
            private: self.private,
            source: self.source.map(|c| Cow::Owned(c.into_owned())),
            kind: self.kind.into_owned(),
        }
    }
}

impl IntoOwned for InfoV1<'_> {
    type Owned = InfoV1Buf;

    fn into_owned(self) -> Self::Owned {
        InfoV1Buf {
            pieces: Cow::Owned(self.pieces.into_owned()),
            file_mode: self.file_mode.into_owned(),
        }
    }
}

impl IntoOwned for InfoV2<'_> {
    type Owned = InfoV2Buf;

    fn into_owned(self) -> Self::Owned {
        InfoV2Buf {
            file_tree: self.file_tree.into_owned(),
        }
    }
}

impl IntoOwned for InfoHybrid<'_> {
    type Owned = InfoHybridBuf;

    fn into_owned(self) -> Self::Owned {
        InfoHybridBuf {
            v1: self.v1.into_owned(),
            v2: self.v2.into_owned(),
        }
    }
}

impl Torrent<'_> {
    #[must_use]
    pub fn builder() -> TorrentBuilder<Empty> {
        TorrentBuilder::new()
    }

    #[must_use]
    pub fn is_v1(&self) -> bool {
        matches!(
            &self.meta,
            TorrentMeta::V1 { .. } | TorrentMeta::Hybrid { .. }
        )
    }

    #[must_use]
    pub fn is_v2(&self) -> bool {
        matches!(
            &self.meta,
            TorrentMeta::V2 { .. } | TorrentMeta::Hybrid { .. }
        )
    }

    #[must_use]
    pub fn is_hybrid(&self) -> bool {
        matches!(self.meta, TorrentMeta::Hybrid { .. })
    }

    #[must_use]
    pub fn trackers(&self) -> Vec<Vec<&Url>> {
        match (&self.tracker, &self.tracker_tiers) {
            (Some(url), None) => vec![vec![url]],
            (_, Some(tiers)) => tiers.iter().map(|tier| tier.iter().collect()).collect(),
            (None, None) => vec![],
        }
    }

    #[must_use]
    pub fn info_hash_v1(&self) -> Option<[u8; 20]> {
        match &self.meta {
            TorrentMeta::V1 { info } => Some(info.info_hash()),
            TorrentMeta::Hybrid { info, .. } => Some(info.info_hash_v1()),
            TorrentMeta::V2 { .. } => None,
        }
    }

    #[must_use]
    pub fn info_hash_v2(&self) -> Option<[u8; 32]> {
        match &self.meta {
            TorrentMeta::V2 { info, .. } => Some(info.info_hash()),
            TorrentMeta::Hybrid { info, .. } => Some(info.info_hash_v2()),
            TorrentMeta::V1 { .. } => None,
        }
    }

    #[must_use]
    pub fn total_size(&self) -> u64 {
        match &self.meta {
            TorrentMeta::V1 { info } => match &info.kind.file_mode {
                FileMode::Single { length, .. } => *length,
                FileMode::Multi { files } => files.iter().map(|f| f.length).sum(),
            },
            TorrentMeta::V2 { info, .. } => info.kind.file_tree.total_size(),
            TorrentMeta::Hybrid { info, .. } => info.kind.v2.file_tree.total_size(),
        }
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        match &self.meta {
            TorrentMeta::V1 { info } => match &info.kind.file_mode {
                FileMode::Single { .. } => 1,
                FileMode::Multi { files } => files.len(),
            },
            TorrentMeta::V2 { info, .. } => info.kind.file_tree.file_count(),
            TorrentMeta::Hybrid { info, .. } => info.kind.v2.file_tree.file_count(),
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

    fn hybrid_mismatch(&self) -> bool {
        match &self.meta {
            TorrentMeta::Hybrid { info, .. } => {
                let file_tree = &info.kind.v2.file_tree;
                match &info.kind.v1.file_mode {
                    FileMode::Single { .. } => {
                        file_tree.file_paths().as_slice() != [Path::new(info.name.as_ref())]
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
            _ => false,
        }
    }
}

impl Torrent<'_> {
    #[must_use]
    pub fn name(&self) -> &Cow<'_, str> {
        match &self.meta {
            TorrentMeta::V1 { info } => &info.name,
            TorrentMeta::V2 { info, .. } => &info.name,
            TorrentMeta::Hybrid { info, .. } => &info.name,
        }
    }

    pub fn set_name(&mut self, name: &str) {
        let name = Cow::Owned(name.to_owned());
        match &mut self.meta {
            TorrentMeta::V1 { info } => info.name = name,
            TorrentMeta::V2 { info, .. } => info.name = name,
            TorrentMeta::Hybrid { info, .. } => info.name = name,
        }
    }

    #[must_use]
    pub fn piece_length(&self) -> NonZeroU64 {
        match &self.meta {
            TorrentMeta::V1 { info } => info.piece_length,
            TorrentMeta::V2 { info, .. } => info.piece_length,
            TorrentMeta::Hybrid { info, .. } => info.piece_length,
        }
    }

    #[must_use]
    pub fn private(&self) -> bool {
        match &self.meta {
            TorrentMeta::V1 { info } => info.private,
            TorrentMeta::V2 { info, .. } => info.private,
            TorrentMeta::Hybrid { info, .. } => info.private,
        }
    }

    pub fn set_private(&mut self, private: bool) {
        match &mut self.meta {
            TorrentMeta::V1 { info } => info.private = private,
            TorrentMeta::V2 { info, .. } => info.private = private,
            TorrentMeta::Hybrid { info, .. } => info.private = private,
        }
    }

    #[must_use]
    pub fn source(&self) -> Option<&Cow<'_, str>> {
        match &self.meta {
            TorrentMeta::V1 { info } => info.source.as_ref(),
            TorrentMeta::V2 { info, .. } => info.source.as_ref(),
            TorrentMeta::Hybrid { info, .. } => info.source.as_ref(),
        }
    }

    pub fn set_source(&mut self, source: &str) {
        let source = Some(Cow::Owned(source.to_owned()));
        match &mut self.meta {
            TorrentMeta::V1 { info } => info.source = source,
            TorrentMeta::V2 { info, .. } => info.source = source,
            TorrentMeta::Hybrid { info, .. } => info.source = source,
        }
    }
}

impl<T: IntoOwned> Info<'_, T> {
    fn info_hash_internal<D: Digest + Update>(
        hash_func: D,
        encoded_info: &Bencode<'_>,
    ) -> Output<D> {
        let mut hasher = digest_io::IoWrapper(hash_func);
        let _ = encoded_info.encode_to_writer(&mut hasher);
        hasher.0.finalize()
    }
}

impl Info<'_, InfoV1<'_>> {
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn info_hash(&self) -> [u8; 20] {
        Self::info_hash_internal(Sha1::new(), &self.to_bencode()).into()
    }
}

impl Info<'_, InfoV2<'_>> {
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn info_hash(&self) -> [u8; 32] {
        Self::info_hash_internal(Sha256::new(), &self.to_bencode()).into()
    }
}

impl Info<'_, InfoHybrid<'_>> {
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        self.into()
    }

    #[must_use]
    pub fn info_hash_v1(&self) -> [u8; 20] {
        Self::info_hash_internal(Sha1::new(), &self.to_bencode()).into()
    }

    #[must_use]
    pub fn info_hash_v2(&self) -> [u8; 32] {
        Self::info_hash_internal(Sha256::new(), &self.to_bencode()).into()
    }
}

impl InfoV2<'_> {
    pub const META_VERSION: u8 = 2;
}

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct TrackerTier(pub Vec<Url>);

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct PieceLayers<'a>(pub BTreeMap<Cow<'a, [u8; 32]>, Cow<'a, [u8]>>);

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

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfoAttr {
    flags: FileInfoAttrFlags,
    encoded: [u8; 4],
    len: usize,
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

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct FileTree<'a>(pub BTreeMap<Cow<'a, str>, FileTreeNode<'a>>);

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileTreeNode<'a> {
    Directory(FileTree<'a>),
    File(FileLeaf<'a>),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileLeaf<'a> {
    pub length: u64,
    pub pieces_root: Option<Cow<'a, [u8; 32]>>,
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

impl<'a> TryFrom<Bencode<'a>> for FileInfo<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let attr = dict.opt(b"attr").map(FileInfoAttr::try_from).transpose()?;

        let length = dict
            .require(b"length")?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = dict.opt_str(b"md5sum")?.map(Cow::Borrowed);

        let path = dict
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

impl<'a> TryFrom<Bencode<'a>> for FileTree<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let dict = bencode.into_dict()?;
        let mut res = BTreeMap::new();

        for (key, value) in dict {
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

impl<'a> TryFrom<Bencode<'a>> for FileLeaf<'a> {
    type Error = Error;

    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let dict = bencode.into_dict()?;

        let length: u64 = dict
            .get(b"length".as_slice())
            .ok_or(Error::MissingField("length".to_string()))?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;
        let pieces_root = dict.get(b"pieces root".as_slice());

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

impl Deref for TrackerTier {
    type Target = [Url];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoOwned for PieceLayers<'_> {
    type Owned = PieceLayersBuf;

    fn into_owned(self) -> PieceLayersBuf {
        PieceLayers(
            self.0
                .into_iter()
                .map(|(k, v)| (Cow::Owned(k.into_owned()), Cow::Owned(v.into_owned())))
                .collect(),
        )
    }
}

impl IntoOwned for FileMode<'_> {
    type Owned = FileModeBuf;

    fn into_owned(self) -> Self::Owned {
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

impl IntoOwned for FileInfo<'_> {
    type Owned = FileInfoBuf;

    fn into_owned(self) -> Self::Owned {
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

impl IntoOwned for FileTree<'_> {
    type Owned = FileTreeBuf;

    fn into_owned(self) -> Self::Owned {
        FileTree(
            self.0
                .into_iter()
                .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
                .collect(),
        )
    }
}

impl IntoOwned for FileTreeNode<'_> {
    type Owned = FileTreeNodeBuf;

    fn into_owned(self) -> Self::Owned {
        match self {
            Self::Directory(dir) => FileTreeNodeBuf::Directory(dir.into_owned()),
            Self::File(file) => FileTreeNodeBuf::File(file.into_owned()),
        }
    }
}

impl IntoOwned for FileLeaf<'_> {
    type Owned = FileLeafBuf;

    fn into_owned(self) -> Self::Owned {
        FileLeafBuf {
            length: self.length,
            pieces_root: self.pieces_root.map(|c| Cow::Owned(c.into_owned())),
        }
    }
}

impl TrackerTier {
    #[must_use]
    pub fn get_shuffled(&self) -> Vec<Url> {
        let mut tier = self.0.clone();
        tier.shuffle(&mut rand::rng());
        tier
    }
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
}

impl FileInfoAttr {
    #[must_use]
    pub fn new(flags: FileInfoAttrFlags) -> Self {
        let mut encoded = [0u8; 4];
        let mut len = 0;

        for (_, flag) in flags.iter_names() {
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
}

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

    #[error("Piece length must be a power of two and at least 16 KiB in BitTorrent v2: {0}")]
    IllegalPieceLengthV2(NonZeroU64),

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
