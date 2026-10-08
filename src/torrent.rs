//! Torrent metainfo.
//!
//! This module contains [`Torrent`], the in-memory representation of a `.torrent` file, and the
//! types that make it up. They follow the layout of the metainfo file as described by
//! [BEP 3](https://www.bittorrent.org/beps/bep_0003.html) (v1),
//! [BEP 52](https://www.bittorrent.org/beps/bep_0052.html) (v2), and
//! [BEP 47](https://www.bittorrent.org/beps/bep_0047.html) (file attributes).
//!
//! # Structure
//!
//! - [`Torrent`] holds the optional top-level fields (trackers, web seeds, creation date, and so
//!   on) and a [`TorrentMeta`].
//! - [`TorrentMeta`] is an enum with one variant per BitTorrent version (v1-only, v2-only, and
//!   hybrid). It holds the `info` dictionary as an [`Info`] and, for v2 and hybrid torrents, the
//!   `piece layers` as [`PieceLayers`].
//! - [`Info`] holds the fields common to all versions and a `kind` field with the version-specific
//!   ones: [`InfoV1`], [`InfoV2`], or [`InfoHybrid`].
//! - v1 describes files with [`FileMode`] and [`FileInfo`]; v2 describes them with a
//!   [`FileTree`] made of [`FileTreeNode`]s and [`FileLeaf`]s.
//!
//! # Parsing and building
//!
//! A [`Torrent`] is obtained in one of three ways:
//!
//! - [`parse_torrent`](crate::parse_torrent) parses raw data directly.
//! - [`Torrent::try_from`] converts a [`Bencode`] element that was parsed earlier with
//!   [`Parser`](crate::bencode::Parser).
//! - [`Torrent::builder`] returns a [`TorrentBuilder`] that hashes files on disk (see the
//!   [`builder`] module).
//!
//! # Borrowed and owned torrents
//!
//! A parsed [`Torrent<'a>`](Torrent) borrows from the buffer it was parsed from. To keep it after
//! the buffer is gone, call [`Torrent::into_owned`](IntoOwned::into_owned), which returns the
//! owned type [`TorrentBuf`]. Every borrowed type has an owned counterpart with the `Buf` suffix
//! (for example [`InfoBuf`] and [`FileTreeBuf`]), and [`IntoOwned`] is the trait that connects
//! them.
//!
//! # Errors
//!
//! Failures while converting [`Bencode`] into a [`Torrent`] are reported as [`enum@Error`].

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
    bencode::{Bencode, cow_bytes_to_str},
    magnet::MagnetLink,
    torrent::builder::{TorrentBuilder, state::Empty},
};

/// Owned version of [`Torrent`]. Use [`Torrent::into_owned`] to obtain an instance.
pub type TorrentBuf = Torrent<'static>;
/// Owned version of [`TorrentMeta`]. Use [`TorrentMeta::into_owned`] to obtain an instance.
pub type TorrentMetaBuf = TorrentMeta<'static>;
/// Owned version of [`PieceLayers`]. Use [`PieceLayers::into_owned`] to obtain an instance.
pub type PieceLayersBuf = PieceLayers<'static>;
/// Owned version of [`Info`]. Use [`Info::into_owned`] to obtain an instance.
pub type InfoBuf<T> = Info<'static, T>;
/// Owned version of [`InfoV1`]. Use [`InfoV1::into_owned`] to obtain an instance.
pub type InfoV1Buf = InfoV1<'static>;
/// Owned version of [`InfoV2`]. Use [`InfoV2::into_owned`] to obtain an instance.
pub type InfoV2Buf = InfoV2<'static>;
/// Owned version of [`InfoHybrid`]. Use [`InfoHybrid::into_owned`] to obtain an instance.
pub type InfoHybridBuf = InfoHybrid<'static>;
/// Owned version of [`FileMode`]. Use [`FileMode::into_owned`] to obtain an instance.
pub type FileModeBuf = FileMode<'static>;
/// Owned version of [`FileInfo`]. Use [`FileInfo::into_owned`] to obtain an instance.
pub type FileInfoBuf = FileInfo<'static>;
/// Owned version of [`FileTree`]. Use [`FileTree::into_owned`] to obtain an instance.
pub type FileTreeBuf = FileTree<'static>;
/// Owned version of [`FileTreeNode`]. Use [`FileTreeNode::into_owned`] to obtain an instance.
pub type FileTreeNodeBuf = FileTreeNode<'static>;
/// Owned version of [`FileLeaf`]. Use [`FileLeaf::into_owned`] to obtain an instance.
pub type FileLeafBuf = FileLeaf<'static>;

/// A helper function that converts a map of unrecognized fields (e.g. [`Info::extra`])
/// into its owned version.
fn extra_into_owned(
    extra: BTreeMap<Cow<'_, [u8]>, Bencode<'_>>,
) -> BTreeMap<Cow<'static, [u8]>, Bencode<'static>> {
    extra
        .into_iter()
        .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
        .collect()
}

/// A helper function that splits the value of the `pieces` field into 20-byte SHA-1 hashes.
fn split_pieces(pieces: &[u8]) -> Result<&[[u8; 20]], Error> {
    match pieces.as_chunks() {
        (pieces, []) => Ok(pieces),
        _ => Err(Error::InvalidPiecesLength),
    }
}

/// A helper function that optionally extracts the v1 fields from an `info` dictionary.
///
/// This function returns [`Some`] if all fields were present and [`None`] if none were.
/// In the case of an inconsistent state (some fields were present and some were not), an
/// [`enum@Error`] is returned.
fn extract_info_v1_fields<'a>(
    dict: &mut BTreeMap<Cow<'a, [u8]>, Bencode<'a>>,
) -> Result<Option<InfoV1<'a>>, Error> {
    let pieces = match dict.opt(b"pieces") {
        Some(b) => Some(match b.into_bytes()? {
            Cow::Borrowed(pieces) => Cow::Borrowed(split_pieces(pieces)?),
            Cow::Owned(pieces) => Cow::Owned(split_pieces(&pieces)?.to_vec()),
        }),
        None => None,
    };

    let file_mode = match (
        dict.opt(b"files"),
        dict.opt(b"length"),
        dict.opt_str(b"md5sum")?,
    ) {
        (Some(b), None, None) => {
            let files = b
                .into_list()?
                .into_iter()
                .map(FileInfo::try_from)
                .collect::<Result<Vec<FileInfo>, _>>()?;

            Some(FileMode::Multi { files })
        }
        (None, Some(b), md5sum) => {
            let length = b
                .as_int()?
                .try_into()
                .map_err(|_| Error::IllegalFieldValue("length"))?;

            Some(FileMode::Single { length, md5sum })
        }
        (None, None, None) => None,
        _ => return Err(Error::MalformedV1),
    };

    match (pieces, file_mode) {
        (None, None) => Ok(None),
        (Some(pieces), Some(file_mode)) => Ok(Some(InfoV1 { pieces, file_mode })),
        _ => Err(Error::MalformedV1),
    }
}

/// A helper function that optionally extracts the v2 fields from an `info` dictionary.
///
/// This function returns [`Some`] if all fields were present and [`None`] if none were.
/// In the case of an inconsistent state (some fields were present and some were not), an
/// [`enum@Error`] is returned.
fn extract_info_v2_fields<'a>(
    dict: &mut BTreeMap<Cow<'a, [u8]>, Bencode<'a>>,
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

/// A type whose lifetime can be made static after consuming.
pub trait IntoOwned {
    /// The static version of this type.
    type Owned: 'static + IntoOwned;

    /// Consumes this value and returns its "owned" version, i.e. the one with
    /// the static lifetime.
    fn into_owned(self) -> Self::Owned;
}

/// An extension trait for [`BTreeMap`]. Provides helper functions to retrieve a [`Bencode`]
/// element or a [`str`] value from the map. The values are removed from the map after
/// any of the operations.
trait DictExt<'a> {
    /// Optionally retrieve a [`Bencode`] element.
    fn opt(&mut self, key: &[u8]) -> Option<Bencode<'a>>;

    /// Retrieve a required [`Bencode`] element from the map, returning an
    /// [`enum@Error`] if it is not present.
    fn require(&mut self, key: &[u8]) -> Result<Bencode<'a>, Error>;

    /// Optionally retrieve a [`Bencode`] element and convert it to [`str`].
    /// Returns an [`enum@Error`] if the element is not valid UTF-8.
    fn opt_str(&mut self, key: &[u8]) -> Result<Option<Cow<'a, str>>, Error>;

    /// Retrieve a required [`Bencode`] element from the map, converting it to [`str`].
    /// Returns an [`enum@Error`] if the element is not present in the map or if it is not
    /// valid UTF-8.
    fn require_str(&mut self, key: &[u8]) -> Result<Cow<'a, str>, Error>;
}

impl<'a> DictExt<'a> for BTreeMap<Cow<'a, [u8]>, Bencode<'a>> {
    fn opt(&mut self, key: &[u8]) -> Option<Bencode<'a>> {
        self.remove(key)
    }

    fn require(&mut self, key: &[u8]) -> Result<Bencode<'a>, Error> {
        self.opt(key).ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }

    fn opt_str(&mut self, key: &[u8]) -> Result<Option<Cow<'a, str>>, Error> {
        self.opt(key)
            .map(Bencode::into_str)
            .transpose()
            .map_err(Error::from)
    }

    fn require_str(&mut self, key: &[u8]) -> Result<Cow<'a, str>, Error> {
        self.opt_str(key)?.ok_or(Error::MissingField(
            String::from_utf8_lossy(key).into_owned(),
        ))
    }
}

/// Torrent metainfo.
///
/// All of [`Torrent`]'s fields are optional except for [`Torrent::meta`], which represents
/// the `info` dictionary and the `piece layers` field in v2-only and hybrid torrents.
///
/// A torrent can be directly parsed from raw data using [`parse_torrent`]. Alternatively,
/// you can parse it into [`Bencode`] using [`Parser`] and then call
/// [`Torrent::try_from`] to obtain an instance of [`Torrent`].
///
/// To build a torrent, it is recommended to use a [`TorrentBuilder`] that will
/// perform all the necessary hashing in an efficient manner. An empty builder instance
/// can be obtained using [`Torrent::builder`].
///
/// [`parse_torrent`]: crate::parse_torrent
/// [`Parser`]: crate::bencode::Parser
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Torrent<'a> {
    /// A vector of tiers of trackers.
    ///
    /// A tracker is a specialized server that coordinates communication between peers. The
    /// first tier acts as the primary choice for BitTorrent clients. The subsequent tiers
    /// contain reserve trackers in case the primary trackers are unavailable.
    /// Before proceeding to the next tier, all trackers within the current tier have to be tried.
    ///
    /// This field corresponds to the `announce-list` field in a raw torrent file. The `announce`
    /// field is not represented directly; instead, during parsing, the `announce` field is ignored
    /// if the `announce-list` is present. If it is absent, a single tier containing the value of
    /// the `announce` field is created. During serialization, the first tracker of the first tier
    /// is inserted into the `announce` field for compatibility reasons.
    pub tracker_tiers: Option<Vec<TrackerTier>>,
    /// A vector of web seeds.
    ///
    /// A web seed is an HTTP/HTTPS server that allows users to download file pieces directly
    /// from it alongside the standard BitTorrent P2P swarm.
    ///
    /// This field corresponds to the `url-list` field in a raw torrent file.
    pub web_seeds: Option<Vec<Url>>,
    /// The creation date of the torrent in seconds since the Unix epoch.
    pub creation_date: Option<u64>,
    /// A comment in free form.
    pub comment: Option<Cow<'a, str>>,
    /// A string that typically indicates the software that created the torrent file.
    pub created_by: Option<Cow<'a, str>>,
    /// A string that indicates the torrent's encoding. It is almost always set to "UTF-8", and
    /// this is the only encoding supported by this crate.
    pub encoding: Option<Cow<'a, str>>,
    /// Represents the torrent's required `info` dictionary, as well as the `piece layers`
    /// field if the torrent is v2-only or hybrid. See [`TorrentMeta`] for more information.
    pub meta: TorrentMeta<'a>,
}

/// Required torrent metainfo.
///
/// An `info` dictionary must be present in all BitTorrent versions. Additionally, v2-only
/// and hybrid torrents must also contain the `piece layers` field.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TorrentMeta<'a> {
    /// Required torrent metainfo for a v1-only torrent.
    ///
    /// Only an `info` dictionary is required in this case.
    V1 {
        /// An `info` dictionary that contains v1 fields.
        info: Info<'a, InfoV1<'a>>,
    },
    /// Required torrent metainfo for a v2-only torrent.
    ///
    /// Both an `info` dictionary and a `piece layers` field are required in this case.
    V2 {
        /// An `info` dictionary that contains v2 fields.
        info: Info<'a, InfoV2<'a>>,
        /// Represents the `piece layers` field. See [`PieceLayers`] for more information.
        piece_layers: PieceLayers<'a>,
    },
    /// Required torrent metainfo for a hybrid (v1 and v2) torrent.
    ///
    /// Both an `info` dictionary and a `piece layers` field are required in this case.
    Hybrid {
        /// An `info` dictionary that contains v1 *and* v2 fields.
        info: Info<'a, InfoHybrid<'a>>,
        /// Represents the `piece layers` field. See [`PieceLayers`] for more information.
        piece_layers: PieceLayers<'a>,
    },
}

/// Torrent's `info` dictionary.
///
/// All of [`Info`]'s fields except [`Info::kind`] represent the fields common for both
/// versions of BitTorrent. The [`Info::kind`] field contains either [`InfoV1`], [`InfoV2`],
/// or [`InfoHybrid`]. These structs represent the fields specific for each version variation
/// of BitTorrent.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Info<'a, T: 'a + IntoOwned> {
    /// Torrent's name.
    ///
    /// In v1, it represents the name of the top directory if the torrent contains multiple files,
    /// or the file name if the torrent consists of only one file. In v2, this field is purely
    /// advisory.
    pub name: Cow<'a, str>,
    /// The length of a piece in bytes.
    ///
    /// Pieces are segments of torrent's data sent over the network.
    ///
    /// In v2, the length of a piece must be a power of two and at least 16 KiB (16384 bytes).
    pub piece_length: NonZeroU64,
    /// Indicates whether the torrent is private.
    ///
    /// In private torrents, files are downloaded from an invite-only community.
    /// Private torrents do not use DHT or PEX.
    ///
    /// [`None`] means that the `private` field is absent, which is equivalent to `Some(false)`.
    /// The two are kept apart because they produce different info hashes.
    pub private: Option<bool>,
    /// A string in free form that is usually used to easily modify the info hash.
    ///
    /// It is also sometimes used by private torrents to trace the distribution of the torrent.
    pub source: Option<Cow<'a, str>>,
    /// Contains version-specific fields.
    ///
    /// The type of this field should be either [`InfoV1`], [`InfoV2`], or [`InfoHybrid`].
    pub kind: T,
    /// The fields of the `info` dictionary that are not recognized by this crate
    /// (e.g. `name.utf-8` or `publisher`).
    ///
    /// They are preserved so that serializing the dictionary reproduces it exactly, which
    /// is required for a correct info hash. A recognized field takes precedence over an
    /// entry with the same key in this map.
    pub extra: BTreeMap<Cow<'a, [u8]>, Bencode<'a>>,
}

/// A helper enum that represents a parsed `info` dictionary.
///
/// The internal variant of [`Info`] is inserted into [`TorrentMeta`] during further parsing.
#[derive(Debug, PartialEq, Eq, Clone)]
enum ParsedInfo<'a> {
    /// Parsed v1 dictionary.
    V1(Info<'a, InfoV1<'a>>),
    /// Parsed v2 dictionary.
    V2(Info<'a, InfoV2<'a>>),
    /// Parsed hybrid dictionary.
    Hybrid(Info<'a, InfoHybrid<'a>>),
}

/// V1-specific `info` dictionary fields.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV1<'a> {
    /// SHA-1 hashes of all pieces appended together.
    ///
    /// Each hash is 20 bytes long. In v1, all files are merged into one byte stream, which is
    /// then divided into pieces of length [`Info::piece_length`] each (except probably for the last one,
    /// which may be smaller) and hashed. This means the pieces can span across file boundaries. The hashes
    /// should be verified for each piece during downloading.
    pub pieces: Cow<'a, [[u8; 20]]>,
    /// Contains file information for this torrent.
    pub file_mode: FileMode<'a>,
}

/// V2-specific `info` dictionary fields.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoV2<'a> {
    /// Torrent's file tree.
    ///
    /// This is a [prefix tree (trie)](https://en.wikipedia.org/wiki/Trie) that contains
    /// the file structure of the torrent. See
    /// [BEP 52](https://www.bittorrent.org/beps/bep_0052.html#file-tree-layout) for
    /// more information.
    pub file_tree: FileTree<'a>,
}

/// `info` dictionary fields for a hybrid torrent. It contains both v1- and v2-specific fields.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct InfoHybrid<'a> {
    /// V1-specific fields.
    pub v1: InfoV1<'a>,
    /// V2-specific fields.
    pub v2: InfoV2<'a>,
}

impl<'a> TryFrom<Bencode<'a>> for Torrent<'a> {
    type Error = Error;

    /// Attempts to convert a [`Bencode`] element into [`Torrent`].
    ///
    /// The function extracts all of [`Torrent`]'s fields and constructs a [`TorrentMeta`].
    /// It also enforces that the files in the v1 and v2 fields match in the case of a hybrid torrent.
    ///
    /// The `announce` field is not represented directly; see [`Torrent::tracker_tiers`] for more
    /// information.
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] in the following cases:
    /// - Invalid bencode was provided;
    /// - The `info` dictionary could not be converted properly:
    ///   - Mandatory common fields were missing;
    ///   - Malformed v1 or v2 dictionary (some mandatory fields were present but others were not);
    ///   - The piece length was less than 1;
    ///   - The length of the `pieces` field in a v1 or hybrid torrent was not a multiple of 20;
    ///   - The piece length was not a power of two or was smaller than 16 KiB in a v2 or hybrid torrent;
    ///   - The `meta version` field was not set to `2` in a v2 or hybrid torrent;
    ///   - Fields representing lengths of files contained negative values;
    /// - The `piece layers` field was absent in a v2 or hybrid torrent;
    /// - Invalid values in some fields:
    ///   - Negative creation date;
    ///   - Malformed URLs;
    /// - File information mismatch between the v1 and v2 metainfo in a hybrid torrent.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let parsed_info = ParsedInfo::try_from(dict.require(b"info")?)?;

        let piece_layers = match dict.opt(b"piece layers") {
            Some(b) => {
                let mut piece_layers = BTreeMap::new();

                for (k, v) in b.into_dict()? {
                    let key = k
                        .as_ref()
                        .try_into()
                        .map_err(|_| Error::IllegalFieldValue("piece layers (key)"))?;
                    piece_layers.insert(key, v.into_bytes()?);
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
            _ => {
                return Err(Error::MalformedV2);
            }
        };

        let tracker = dict
            .opt_str(b"announce")?
            .map(|s| Url::parse(&s))
            .transpose()?;

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

        let tracker_tiers = match (tracker, tracker_tiers) {
            (_, Some(tracker_tiers)) => Some(tracker_tiers),
            (Some(tracker), None) => Some(vec![TrackerTier(vec![tracker])]),
            (None, None) => None,
        };

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

        let comment = dict.opt_str(b"comment")?;

        let created_by = dict.opt_str(b"created by")?;

        let encoding = dict.opt_str(b"encoding")?;

        let res = Self {
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

    /// Attempts to convert a [`Bencode`] element into [`ParsedInfo`].
    ///
    /// This represents an intermediate step before compiling a [`TorrentMeta`]. It exists
    /// because the correct structure of a v2 and hybrid torrent should be enforced structurally,
    /// which means that the `piece layers` field must be present in such a torrent alongside
    /// the correct metainfo in the `info` dictionary. However, the `piece layers` field
    /// must be located on the top level of a torrent file outside of the `info` dictionary. For this
    /// reason, the `info` dictionary information is compiled first into [`ParsedInfo`] as an intermediate
    /// step and then merged into [`TorrentMeta`] in [`Torrent::try_from`] with `piece layers`.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let name = dict.require_str(b"name")?;
        let piece_length = dict
            .require(b"piece length")?
            .as_int()?
            .try_into()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::IllegalFieldValue("piece length"))?;

        let private = match dict.opt(b"private") {
            Some(b) => match b.as_int()? {
                0 => Some(false),
                1 => Some(true),
                _ => return Err(Error::IllegalFieldValue("private")),
            },
            None => None,
        };
        let source = dict.opt_str(b"source")?;

        let v1 = extract_info_v1_fields(&mut dict)?;
        let v2 = extract_info_v2_fields(&mut dict)?;

        // All recognized fields have been removed by now
        let extra = dict;

        match (v1, v2) {
            (Some(v1), Some(v2)) => {
                if !piece_length.is_power_of_two() || piece_length.get() < 16 * 1024 {
                    return Err(Error::IllegalPieceLengthV2(piece_length));
                }

                Ok(ParsedInfo::Hybrid(Info {
                    name,
                    piece_length,
                    private,
                    source,
                    kind: InfoHybrid { v1, v2 },
                    extra,
                }))
            }
            (Some(v1), None) => Ok(ParsedInfo::V1(Info {
                name,
                piece_length,
                private,
                source,
                kind: v1,
                extra,
            })),
            (None, Some(v2)) => {
                if !piece_length.is_power_of_two() || piece_length.get() < 16 * 1024 {
                    return Err(Error::IllegalPieceLengthV2(piece_length));
                }

                Ok(ParsedInfo::V2(Info {
                    name,
                    piece_length,
                    private,
                    source,
                    kind: v2,
                    extra,
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
            extra: extra_into_owned(self.extra),
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
    /// Creates an [`TorrentBuilder`] instance with no provided paths.
    ///
    /// This is equivalent to [`TorrentBuilder::new`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use bitors::{Torrent, torrent::builder::Error};
    /// # fn main() -> Result<(), Error> {
    /// let torrent = Torrent::builder()
    ///     .add_path("my_folder")
    ///     .build()?;
    /// // Do something else...
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn builder() -> TorrentBuilder<Empty> {
        TorrentBuilder::new()
    }

    /// Checks if the torrent is v1. This includes both v1-only and hybrid torrents.
    #[must_use]
    pub fn is_v1(&self) -> bool {
        matches!(
            self.meta,
            TorrentMeta::V1 { .. } | TorrentMeta::Hybrid { .. }
        )
    }

    /// Checks if the torrent is v2. This includes both v2-only and hybrid torrents.
    #[must_use]
    pub fn is_v2(&self) -> bool {
        matches!(
            self.meta,
            TorrentMeta::V2 { .. } | TorrentMeta::Hybrid { .. }
        )
    }

    /// Checks if the torrent is hybrid.
    #[must_use]
    pub fn is_hybrid(&self) -> bool {
        matches!(self.meta, TorrentMeta::Hybrid { .. })
    }

    /// Returns a vector of the torrent's tracker tiers or an empty vector if none are present.
    #[must_use]
    pub fn tracker_tiers(&self) -> Vec<&TrackerTier> {
        match &self.tracker_tiers {
            Some(tracker_tiers) => tracker_tiers.iter().collect(),
            None => vec![],
        }
    }

    /// Computes the SHA-1 info hash of a v1-only or hybrid torrent.
    /// Returns [`None`] if the torrent is v2-only.
    #[must_use]
    pub fn info_hash_v1(&self) -> Option<[u8; 20]> {
        match &self.meta {
            TorrentMeta::V1 { info } => Some(info.info_hash()),
            TorrentMeta::Hybrid { info, .. } => Some(info.info_hash_v1()),
            TorrentMeta::V2 { .. } => None,
        }
    }

    /// Computes the SHA-256 info hash of a v2-only or hybrid torrent.
    /// Returns [`None`] if the torrent is v1-only.
    #[must_use]
    pub fn info_hash_v2(&self) -> Option<[u8; 32]> {
        match &self.meta {
            TorrentMeta::V2 { info, .. } => Some(info.info_hash()),
            TorrentMeta::Hybrid { info, .. } => Some(info.info_hash_v2()),
            TorrentMeta::V1 { .. } => None,
        }
    }

    /// Computes the total size of the torrent's files in bytes.
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

    /// Counts the torrent's files.
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
    /// let torrent = Torrent::builder().add_path("my_file").build().unwrap();
    /// println!("{}", torrent.magnet_link());
    /// // Hybrid torrent: magnet:?xt=urn:btih:<v1 hash>&xt=urn:btmh:<v2 hash>...
    /// ```
    #[must_use]
    pub fn magnet_link(&self) -> MagnetLink {
        MagnetLink::from(self)
    }

    /// A convenience method that converts this [`Torrent`] to a [`Bencode`] element.
    ///
    /// This is equivalent to [`Bencode::from`].
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        Bencode::from(self)
    }

    /// Checks whether there is a file information mismatch between the v1 and v2 fields in a hybrid torrent.
    /// Padding files are ignored. Returns `false` (no mismatch) if the torrent is not hybrid.
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
                                // Disregard padding files
                                !f.attr
                                    .as_ref()
                                    .is_some_and(|a| a.flags.contains(FileInfoAttrFlags::PADDING))
                            })
                            .map(FileInfo::to_path_buf)
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
    /// Returns a reference to this torrent's name.
    #[must_use]
    pub fn name(&self) -> &Cow<'_, str> {
        match &self.meta {
            TorrentMeta::V1 { info } => &info.name,
            TorrentMeta::V2 { info, .. } => &info.name,
            TorrentMeta::Hybrid { info, .. } => &info.name,
        }
    }

    /// Sets this torrent's name.
    ///
    /// Note that this **will** change the info hash.
    pub fn set_name(&mut self, name: &str) {
        let name = Cow::Owned(name.to_owned());
        match &mut self.meta {
            TorrentMeta::V1 { info } => info.name = name,
            TorrentMeta::V2 { info, .. } => info.name = name,
            TorrentMeta::Hybrid { info, .. } => info.name = name,
        }
    }

    /// Returns this torrent's piece length.
    ///
    /// Note that there is no setter for `piece_length`. This is because modifying
    /// its value will invalidate all of the hashes in the torrent, thus making it
    /// useless. Use [`TorrentBuilder`] to rehash the files.
    #[must_use]
    pub fn piece_length(&self) -> NonZeroU64 {
        match &self.meta {
            TorrentMeta::V1 { info } => info.piece_length,
            TorrentMeta::V2 { info, .. } => info.piece_length,
            TorrentMeta::Hybrid { info, .. } => info.piece_length,
        }
    }

    /// Checks whether the torrent is private.
    #[must_use]
    pub fn private(&self) -> bool {
        let private_opt = match &self.meta {
            TorrentMeta::V1 { info } => info.private,
            TorrentMeta::V2 { info, .. } => info.private,
            TorrentMeta::Hybrid { info, .. } => info.private,
        };

        private_opt.unwrap_or(false)
    }

    /// Changes this torrent's `private` field.
    ///
    /// The field is always written out afterwards, as `1` or `0`. To remove it instead,
    /// set [`Info::private`] to [`None`].
    ///
    /// Note that this **will** change the info hash.
    pub fn set_private(&mut self, private: bool) {
        match &mut self.meta {
            TorrentMeta::V1 { info } => info.private = Some(private),
            TorrentMeta::V2 { info, .. } => info.private = Some(private),
            TorrentMeta::Hybrid { info, .. } => info.private = Some(private),
        }
    }

    /// Returns a reference to the value of this torrent's `source` field.
    #[must_use]
    pub fn source(&self) -> Option<&Cow<'_, str>> {
        match &self.meta {
            TorrentMeta::V1 { info } => info.source.as_ref(),
            TorrentMeta::V2 { info, .. } => info.source.as_ref(),
            TorrentMeta::Hybrid { info, .. } => info.source.as_ref(),
        }
    }

    /// Changes this torrent's `source` field.
    ///
    /// Note that this **will** change the info hash.
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
    /// An internal helper function that computes the info hash given the
    /// hash function and the bencoded `info` dictionary.
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
    /// A convenience method that converts this v1-only [`Info`] (`Info<'_, InfoV1<'_>>`)
    /// to a [`Bencode`] element.
    ///
    /// This is equivalent to [`Bencode::from`].
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        Bencode::from(self)
    }

    /// Computes the SHA-1 info hash of this `info` dictionary (BitTorrent v1).
    #[must_use]
    pub fn info_hash(&self) -> [u8; 20] {
        Self::info_hash_internal(Sha1::new(), &self.to_bencode()).into()
    }
}

impl Info<'_, InfoV2<'_>> {
    /// A convenience method that converts this v2-only [`Info`] (`Info<'_, InfoV2<'_>>`)
    /// to a [`Bencode`] element.
    ///
    /// This is equivalent to [`Bencode::from`].
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        Bencode::from(self)
    }

    /// Computes the SHA-256 info hash of this `info` dictionary (BitTorrent v2).
    #[must_use]
    pub fn info_hash(&self) -> [u8; 32] {
        Self::info_hash_internal(Sha256::new(), &self.to_bencode()).into()
    }
}

impl Info<'_, InfoHybrid<'_>> {
    /// A convenience method that converts this hybrid [`Info`] (`Info<'_, InfoHybrid<'_>>`)
    /// to a [`Bencode`] element.
    ///
    /// This is equivalent to [`Bencode::from`].
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        Bencode::from(self)
    }

    /// Computes the SHA-1 info hash of this `info` dictionary (BitTorrent v1).
    #[must_use]
    pub fn info_hash_v1(&self) -> [u8; 20] {
        Self::info_hash_internal(Sha1::new(), &self.to_bencode()).into()
    }

    /// Computes the SHA-256 info hash of this `info` dictionary (BitTorrent v2).
    #[must_use]
    pub fn info_hash_v2(&self) -> [u8; 32] {
        Self::info_hash_internal(Sha256::new(), &self.to_bencode()).into()
    }
}

impl InfoV2<'_> {
    /// The `meta version` field.
    pub const META_VERSION: u8 = 2;
}

/// A tier of trackers.
///
/// A tracker is a specialized server that coordinates communication between peers.
///
/// Trackers in a tier should be randomly shuffled before use.
///
/// [`TrackerTier`] can be dereferenced as `&[Url]`.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct TrackerTier(
    /// The underlying vector of [`Url`]s.
    pub Vec<Url>,
);

/// Represents the `piece layers` field in a v2-only or hybrid torrent.
///
/// This field is used by clients to verify file hashes during downloading.
///
/// The thorough description of this field can be found in
/// [BEP 52](https://www.bittorrent.org/beps/bep_0052.html#metainfo-files).
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct PieceLayers<'a>(
    /// The underlying dictionary, sorted by keys.
    pub BTreeMap<[u8; 32], Cow<'a, [u8]>>,
);

/// Represents one of the two file states of an `info` dictionary with v1 fields: Single (one file) or
/// Multi (multiple files).
///
/// 1. **Single:** The torrent contains only one file. The `info` dictionary contains
///    the fields `length` and optionally `md5sum`.
/// 2. **Multi:** The torrent contains several files. The `info` dictionary contains the
///    field `files`, which is a list of dictionaries that contain information about each
///    downloadable file. These dictionaries are represented as [`FileInfo`] in this crate.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileMode<'a> {
    /// The torrent contains only one file.
    Single {
        /// The length of the file in bytes.
        length: u64,
        /// An optional 128-bit MD5 hash of the file.
        md5sum: Option<Cow<'a, str>>,
    },
    /// The torrent contains several files.
    Multi {
        /// A list of file information dictionaries.
        files: Vec<FileInfo<'a>>,
    },
}

/// An `attr` field in a file information dictionary, as defined by [BEP 47](https://www.bittorrent.org/beps/bep_0047.html).
///
/// This field represents the file's attributes. See [`FileInfoAttrFlags`] for more information.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfoAttr {
    /// Bit flags representing the attributes.
    flags: FileInfoAttrFlags,
    /// Encoded bit flags. This value should be precomputed due to the borrow checker restrictions.
    encoded: [u8; 4],
    /// The length of the encoded flags.
    len: usize,
}

bitflags! {
    /// Bit flags representing the file's attributes.
    ///
    /// As per [BEP 47](https://www.bittorrent.org/beps/bep_0047.html), there are four possible attributes:
    ///
    /// | Attribute    | Symbol in `attr` |
    /// |:------------:|:----------------:|
    /// | Symlink      | `l`              |
    /// | Executable   | `x`              |
    /// | Hidden file  | `h`              |
    /// | Padding file | `p`              |
    ///
    /// See [BEP 47](https://www.bittorrent.org/beps/bep_0047.html) for more information on symlinks
    /// and padding files.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub struct FileInfoAttrFlags: u8 {
        /// The file is a symlink (`l`).
        const SYMLINK = 0b0001;
        /// The file is executable (`x`).
        const EXEC = 0b0010;
        /// The file is hidden (`h`).
        const HIDDEN = 0b0100;
        /// The file is a padding file (`p`), which only exists to align the following file to
        /// a piece boundary. Padding files contain zeros and are not written to disk.
        const PADDING = 0b1000;
    }
}

/// A file information dictionary in a torrent with v1 fields and multiple files.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileInfo<'a> {
    /// The file's attributes, as defined by [BEP 47](https://www.bittorrent.org/beps/bep_0047.html).
    pub attr: Option<FileInfoAttr>,
    /// The length of the file in bytes.
    pub length: u64,
    /// An optional 128-bit MD5 hash of the file.
    pub md5sum: Option<Cow<'a, str>>,
    /// The file's path. Represented as a vector of path components.
    pub path: Vec<Cow<'a, str>>,
    /// The fields of this dictionary that are not recognized by this crate
    /// (e.g. `path.utf-8`).
    ///
    /// They are preserved so that the `info` dictionary can be reproduced exactly, which
    /// is required for a correct info hash. A recognized field takes precedence over an
    /// entry with the same key in this map.
    pub extra: BTreeMap<Cow<'a, [u8]>, Bencode<'a>>,
}

/// A `file tree` field representation.
///
/// This is a prefix tree (trie) whose nodes are path components. Each file leaf
/// contains meta information about a file, including the `pieces root` hash.
/// See [the section about file trees in BEP 52](https://www.bittorrent.org/beps/bep_0052.html#file-tree-layout)
/// for more information.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct FileTree<'a>(pub BTreeMap<Cow<'a, str>, FileTreeNode<'a>>);

/// Represents a node within a [`FileTree`].
///
/// Each node can represent either a directory or a file. In the latter case,
/// it is also a leaf.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FileTreeNode<'a> {
    /// A directory node.
    Directory(FileTree<'a>),
    /// A file leaf.
    File(FileLeaf<'a>),
}

/// A file leaf in a [`FileTree`].
///
/// A file leaf contains the length of the file and the `pieces root`,
/// which is the root hash of the associated Merkle tree.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct FileLeaf<'a> {
    /// The length of the file in bytes.
    pub length: u64,
    /// The root hash of the associated Merkle tree.
    ///
    /// All leaves representing files with length larger than zero must have this
    /// field. See [BEP 52](https://www.bittorrent.org/beps/bep_0052.html#:~:text=pieces%20root,-For)
    /// for a thorough explanation on how its value is computed.
    pub pieces_root: Option<Cow<'a, [u8; 32]>>,
    /// The fields of this dictionary that are not recognized by this crate
    /// (e.g. `mtime`).
    ///
    /// They are preserved so that the `info` dictionary can be reproduced exactly, which
    /// is required for a correct info hash. A recognized field takes precedence over an
    /// entry with the same key in this map.
    pub extra: BTreeMap<Cow<'a, [u8]>, Bencode<'a>>,
}

impl<'a> TryFrom<Bencode<'a>> for FileInfoAttr {
    type Error = Error;

    /// Attempts to convert a [`Bencode`] element into [`FileInfoAttr`].
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] if the [`Bencode`] element was not a string or
    /// if it contained an unknown attribute. The list of valid attributes can be found
    /// in [`FileInfoAttrFlags`].
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

    /// Attempts to convert a [`Bencode`] element into [`FileInfo`].
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] if the [`Bencode`] element was not a dictionary
    /// or if any of the fields contained illegal values (such as a negative file length).
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let attr = dict.opt(b"attr").map(FileInfoAttr::try_from).transpose()?;

        let length = dict
            .require(b"length")?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;

        let md5sum = dict.opt_str(b"md5sum")?;

        let path = dict
            .require(b"path")?
            .into_list()?
            .into_iter()
            .map(Bencode::into_str)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            attr,
            length,
            md5sum,
            path,
            // All recognized fields have been removed by now
            extra: dict,
        })
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileTree<'a> {
    type Error = Error;

    /// Attempts to convert a [`Bencode`] element into [`FileTree`].
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] if the [`Bencode`] element was not a dictionary,
    /// a path component was empty or not valid UTF-8, a file node contained anything
    /// besides its leaf, or if a leaf could not be converted (see [`FileLeaf::try_from`]
    /// for more details).
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let dict = bencode.into_dict()?;
        let mut res = BTreeMap::new();

        for (key, value) in dict {
            let key = cow_bytes_to_str(key)?;
            if key.is_empty() {
                return Err(Error::InvalidFileTree);
            }

            // A file is a node whose only key is an empty string: `<name>: {"": <leaf>}`
            let mut node_dict = value.into_dict()?;
            let node = match node_dict.opt(b"") {
                Some(leaf) if node_dict.is_empty() => FileTreeNode::File(FileLeaf::try_from(leaf)?),
                Some(_) => return Err(Error::InvalidFileTree),
                None => FileTreeNode::Directory(Self::try_from(Bencode::Dict(node_dict))?),
            };

            res.insert(key, node);
        }

        Ok(Self(res))
    }
}

impl<'a> TryFrom<Bencode<'a>> for FileLeaf<'a> {
    type Error = Error;

    /// Attempts to convert a [`Bencode`] element into [`FileLeaf`].
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] if the [`Bencode`] element was not a dictionary or
    /// if any of the fields contained illegal values (such as a negative file length
    /// or `pieces root` whose length was not equal to 32 bytes). It also returns an
    /// [`enum@Error`] if there was no `pieces root` on a file with length larger than zero, or,
    /// conversely, if there **was** one on an empty file.
    fn try_from(bencode: Bencode<'a>) -> Result<Self, Self::Error> {
        let mut dict = bencode.into_dict()?;

        let length: u64 = dict
            .require(b"length")?
            .as_int()?
            .try_into()
            .map_err(|_| Error::IllegalFieldValue("length"))?;
        let pieces_root = dict.opt(b"pieces root");

        let pieces_root = match (length, pieces_root) {
            (0, None) => None,
            (1.., Some(b)) => {
                let illegal = |_| Error::IllegalFieldValue("pieces root");
                Some(match b.into_bytes()? {
                    Cow::Borrowed(b) => Cow::Borrowed(b.try_into().map_err(illegal)?),
                    Cow::Owned(b) => Cow::Owned(b.as_slice().try_into().map_err(illegal)?),
                })
            }
            _ => return Err(Error::InvalidFileTree),
        };

        Ok(Self {
            length,
            pieces_root,
            // All recognized fields have been removed by now
            extra: dict,
        })
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
                .map(|(k, v)| (k, Cow::Owned(v.into_owned())))
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
            extra: extra_into_owned(self.extra),
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
            extra: extra_into_owned(self.extra),
        }
    }
}

impl TrackerTier {
    /// Randomly shuffles the tier in-place.
    ///
    /// Tiers should be shuffled before use.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use bitors::{error::Error, parse_torrent};
    /// # fn main() -> Result<(), Error> {
    /// # let data = [0u8; 10];
    /// let torrent = parse_torrent(&data)?; // Assume we already read the torrent into `data`
    /// let mut first_tier = torrent.tracker_tiers.unwrap()[0].clone();
    /// first_tier.shuffle();
    ///
    /// for tracker in &*first_tier {
    ///     // Do something here...
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn shuffle(&mut self) {
        self.0.shuffle(&mut rand::rng());
    }
}

impl FileMode<'_> {
    /// Checks whether the torrent's v1 fields are in single file mode.
    #[must_use]
    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single { .. })
    }

    /// Checks whether the torrent's v1 fields are in multiple file mode.
    #[must_use]
    pub fn is_multi(&self) -> bool {
        !self.is_single()
    }
}

impl FileInfoAttr {
    /// Creates a new [`FileInfoAttr`] instance given the flags.
    ///
    /// Internally, this function preemptively encodes the flags as bytes.
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

    /// Returns the content of the `attr` field as bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.encoded[..self.len]
    }
}

impl FileInfo<'_> {
    /// Generates a [`PathBuf`] from [`FileInfo::path`].
    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        self.path.iter().map(Cow::as_ref).collect()
    }

    /// A convenience method that converts this [`FileInfo`] to a [`Bencode`] element.
    ///
    /// This is equivalent to [`Bencode::from`].
    #[must_use]
    pub fn to_bencode(&self) -> Bencode<'_> {
        Bencode::from(self)
    }
}

impl FileTree<'_> {
    /// Extracts all files from this [`FileTree`] and returns a vector of their paths.
    #[must_use]
    pub fn file_paths(&self) -> Vec<PathBuf> {
        let mut res = Vec::with_capacity(self.file_count());
        self.collect_paths(Path::new(""), &mut res);
        res
    }

    /// Computes the total size of all files in this torrent in bytes.
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

    /// Counts the number of files in this torrent.
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

    /// A helper method that performs path extractions from the [`FileTree`] into `res`.
    ///
    /// The method works recursively by tracking the cumulative prefix at each level of the prefix
    /// tree.
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

/// Errors that can occur when parsing a torrent.
#[derive(Debug, Error)]
pub enum Error {
    /// The raw data was not valid bencode.
    #[error("Bencode parsing error: {0}")]
    Bencode(#[from] crate::bencode::Error),

    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A byte string could not be converted to a UTF-8 string.
    #[error("UTF-8 encoding error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// A malformed URL was encountered.
    #[error("URL parsing error: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// A mandatory field was missing from the torrent.
    #[error("Missing required field: {0}")]
    MissingField(String),

    /// A field contained an illegal value (e.g., a negative file length).
    #[error("Illegal value in field '{0}'")]
    IllegalFieldValue(&'static str),

    /// The piece length was not a power of two or was less than 16 KiB (16384 bytes)
    /// in a v2-only or hybrid torrent.
    #[error("Piece length must be a power of two and at least 16 KiB in BitTorrent v2: {0}")]
    IllegalPieceLengthV2(NonZeroU64),

    /// The length of the `pieces` list was not a multiple of 20.
    ///
    /// As this field contains a list of SHA-1 hashes, which are 20 bytes long each,
    /// its length must be a multiple of 20.
    #[error("Length of the 'pieces' list must be a multiple of 20")]
    InvalidPiecesLength,

    /// The value of the `file tree` field was malformed.
    #[error("Invalid file tree")]
    InvalidFileTree,

    /// The BitTorrent v1 fields were either incomplete or inconsistent.
    #[error("Malformed BitTorrent v1 torrent")]
    MalformedV1,

    /// The BitTorrent v2 fields were either incomplete or inconsistent.
    #[error("Malformed BitTorrent v2 torrent")]
    MalformedV2,

    /// The v1 and v2 fields provided different file lists.
    ///
    /// Note that padding files in v1 are ignored during this check.
    #[error("File information mismatch between v1 and v2 fields in hybrid torrent")]
    HybridMismatch,

    /// The format of the torrent could not be recognized.
    #[error("Unrecognized torrent format")]
    UnrecognizedFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_torrent;

    /// Builds a [`Bencode::Dict`] from a list of key-value pairs.
    fn dict(entries: Vec<(&'static str, Bencode<'static>)>) -> Bencode<'static> {
        Bencode::Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().into(), v))
                .collect(),
        )
    }

    /// Wraps an encoded `info` dictionary into an encoded torrent.
    fn torrent_bytes(info: &Bencode<'_>, piece_layers: Option<Bencode<'static>>) -> Vec<u8> {
        let mut torrent = vec![("info", info.clone().into_owned())];
        if let Some(piece_layers) = piece_layers {
            torrent.push(("piece layers", piece_layers));
        }
        dict(torrent).encode()
    }

    fn v1_info() -> Bencode<'static> {
        dict(vec![
            (
                "files",
                Bencode::List(vec![
                    dict(vec![
                        ("length", 1.into()),
                        ("path", Bencode::List(vec!["a".into()])),
                        ("path.utf-8", Bencode::List(vec!["a".into()])),
                    ]),
                    dict(vec![
                        ("ed2k", vec![0xab; 16].into()),
                        ("length", 2.into()),
                        ("path", Bencode::List(vec!["b".into()])),
                    ]),
                ]),
            ),
            ("name", "test".into()),
            ("name.utf-8", "test".into()),
            ("piece length", 16384.into()),
            ("pieces", vec![0x11; 20].into()),
            ("private", 0.into()),
            ("z", 1.into()),
        ])
    }

    fn v2_info() -> Bencode<'static> {
        dict(vec![
            (
                "file tree",
                dict(vec![(
                    "dir",
                    dict(vec![(
                        "a",
                        dict(vec![(
                            "",
                            dict(vec![
                                ("length", 1.into()),
                                ("mtime", 1_700_000_000.into()),
                                ("pieces root", vec![0x22; 32].into()),
                            ]),
                        )]),
                    )]),
                )]),
            ),
            ("meta version", 2.into()),
            ("name", "test".into()),
            ("piece length", 16384.into()),
            ("z", dict(vec![("nested", Bencode::List(vec![1.into()]))])),
        ])
    }

    #[test]
    fn test_v1_info_hash_preserves_unknown_fields() {
        let info = v1_info();
        let info_bytes = info.encode();
        let data = torrent_bytes(&info, None);
        let torrent = parse_torrent(&data).unwrap();

        let TorrentMeta::V1 { info: parsed } = &torrent.meta else {
            panic!("expected a v1 torrent");
        };
        assert_eq!(parsed.private, Some(false));
        assert!(!torrent.private());
        assert_eq!(parsed.extra.len(), 2);
        let FileMode::Multi { files } = &parsed.kind.file_mode else {
            panic!("expected multiple files");
        };
        assert!(files[0].extra.contains_key(b"path.utf-8".as_slice()));
        assert!(files[1].extra.contains_key(b"ed2k".as_slice()));

        assert_eq!(parsed.to_bencode().encode(), info_bytes);
        assert_eq!(
            torrent.info_hash_v1().unwrap(),
            <[u8; 20]>::from(Sha1::digest(&info_bytes))
        );
    }

    #[test]
    fn test_v2_info_hash_preserves_unknown_fields() {
        let info = v2_info();
        let info_bytes = info.encode();
        let data = torrent_bytes(&info, Some(dict(vec![])));
        let torrent = parse_torrent(&data).unwrap();

        let TorrentMeta::V2 { info: parsed, .. } = &torrent.meta else {
            panic!("expected a v2 torrent");
        };
        assert_eq!(parsed.private, None);

        assert_eq!(parsed.to_bencode().encode(), info_bytes);
        assert_eq!(
            torrent.info_hash_v2().unwrap(),
            <[u8; 32]>::from(Sha256::digest(&info_bytes))
        );
    }

    #[test]
    fn test_built_torrent_round_trip() {
        let dir = std::env::temp_dir().join(format!("bitors-round-trip-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), vec![1u8; 40_000]).unwrap();
        std::fs::write(dir.join("sub").join("b"), vec![2u8; 70_000]).unwrap();

        let built = Torrent::builder()
            .add_path(&dir)
            .private(true)
            .build_hybrid();
        std::fs::remove_dir_all(&dir).unwrap();
        let built = built.unwrap();

        let data = built.to_bencode().encode();
        let parsed = parse_torrent(&data).unwrap();

        assert_eq!(parsed.info_hash_v1(), built.info_hash_v1());
        assert_eq!(parsed.info_hash_v2(), built.info_hash_v2());
        assert_eq!(parsed.to_bencode().encode(), data);
    }

    #[test]
    fn test_owned_torrent_keeps_info_hash() {
        let data = torrent_bytes(&v1_info(), None);
        let torrent = parse_torrent(&data).unwrap();
        let hash = torrent.info_hash_v1();

        let owned = torrent.into_owned();
        drop(data);

        assert_eq!(owned.info_hash_v1(), hash);
    }

    #[test]
    fn test_recognized_field_takes_precedence_over_extra() {
        let data = torrent_bytes(&v1_info(), None);
        let mut torrent = parse_torrent(&data).unwrap();

        let TorrentMeta::V1 { info } = &mut torrent.meta else {
            panic!("expected a v1 torrent");
        };
        info.extra.insert(b"name".into(), "other".into());

        let encoded = info.to_bencode();
        assert_eq!(
            encoded.as_dict().unwrap()[b"name".as_slice()]
                .as_str()
                .unwrap(),
            "test"
        );
    }
}
