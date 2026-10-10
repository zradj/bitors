//! Building new torrents from files on disk.
//!
//! The main type of this module is [`TorrentBuilder`], a typestate builder that collects paths and
//! metadata, hashes the files, and produces a [`TorrentBuf`]. See its documentation for the
//! available options and their defaults.
//!
//! A torrent can be built in three forms:
//!
//! - [`build_v1`](TorrentBuilder::build_v1) produces a v1-only torrent.
//! - [`build_v2`](TorrentBuilder::build_v2) produces a v2-only torrent.
//! - [`build_hybrid`](TorrentBuilder::build_hybrid), also available as
//!   [`build`](TorrentBuilder::build), produces a hybrid torrent that works with both v1 and v2
//!   clients.
//!
//! Hashing is the expensive part of building a torrent. Files are memory-mapped and split into
//! pieces that are hashed in parallel with `rayon`; for v2 and hybrid torrents the per-file
//! Merkle trees are computed as well.
//!
//! The module also contains the [`state`] markers used by the builder's typestate, [`FilterFn`]
//! (the type of path filters), and [`enum@Error`], which describes everything that can go wrong
//! while building.
//!
//! # Examples
//!
//! ```no_run
//! use bitors::Torrent;
//!
//! # fn main() -> Result<(), bitors::torrent::builder::Error> {
//! let torrent = Torrent::builder()
//!     .comment("My first torrent")
//!     .add_path("my_folder")
//!     .build()?;
//!
//! println!("{}", torrent.magnet_link());
//! # Ok(())
//! # }
//! ```

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::File,
    marker::PhantomData,
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use path_clean::clean;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use thiserror::Error;
use url::Url;

use crate::torrent::{
    FileInfo, FileInfoAttr, FileInfoAttrFlags, FileLeaf, FileMode, FileTree, FileTreeNode, Info,
    InfoHybrid, InfoV1, InfoV1Buf, InfoV2, InfoV2Buf, PieceLayers, PieceLayersBuf, Torrent,
    TorrentBuf, TorrentMeta, TrackerTier,
    builder::{
        field_builders::{CommonFields, common_fields, hybrid_fields, v1_fields, v2_fields},
        hashing::V2FileHashes,
        state::HasPaths,
        utils::{
            piece_length_usize, remove_common_prefix, resolve_file_paths, resolve_name,
            torrent_from_parts,
        },
    },
};

/// The block size as defined by BEP 52 - 16 KiB.
const V2_BLOCK_SIZE: usize = 16 * 1024;
/// The [`u64`] equivalent of [`V2_BLOCK_SIZE`].
const V2_BLOCK_SIZE_U64: u64 = V2_BLOCK_SIZE as u64;

/// States of [`TorrentBuilder`].
pub mod state {
    /// Represents a [`TorrentBuilder`](super::TorrentBuilder) that has no paths supplied to it.
    #[derive(Debug)]
    pub struct Empty;

    /// Represents a [`TorrentBuilder`](super::TorrentBuilder) that was supplied with at least one path.
    #[derive(Debug)]
    pub struct HasPaths;
}

/// Structs and functions that deal with file hashing for the torrent.
mod hashing {
    use std::{num::NonZeroU64, sync::OnceLock};

    use crate::torrent::builder::utils::{FileEntry, FileManager};
    use rayon::prelude::*;

    use super::{
        Digest, Error, ParallelIterator, ParallelSlice, Sha1, Sha256, V2_BLOCK_SIZE,
        V2_BLOCK_SIZE_U64,
    };

    /// An array of all zeros, used to simulate a padding file.
    static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];
    /// The hashes of empty Merkle trees of depths up to 32.
    static EMPTY_TREE_HASHES: OnceLock<[[u8; 32]; 33]> = OnceLock::new();

    /// A vector of 20-byte piece hashes.
    #[derive(Debug)]
    pub(super) struct V1PieceHashes(pub(super) Vec<[u8; 20]>);

    /// Represents the value of the `pieces root` field for non-empty files
    /// as well as the corresponding entry in the `piece layers` field if applicable.
    #[derive(Debug)]
    pub(super) enum V2FileHashes {
        /// The file is empty and thus has no hashes.
        Empty,
        /// The file's length is smaller than or equal to the piece length, so it
        /// does not have a corresponding entry in the `piece layers` field.
        SinglePiece {
            /// The value of the `pieces root` field for this file.
            root: [u8; 32],
        },
        /// The file's length was larger than the piece length.
        MultiPiece {
            /// The value of the `pieces root` field for this file.
            root: [u8; 32],
            /// The corresponding entry in the `piece layers` field.
            layer: Vec<[u8; 32]>,
        },
    }

    /// Information on a "chunk", or a part of a piece, including the file's index
    /// in the [`FileManager`], the offset within the file, the length of the chunk,
    /// and whether the chunk is part of a padding file.
    ///
    /// A use is registered in [`FileManager`] for each chunk.
    #[derive(Debug, Clone)]
    struct V1ChunkPlan {
        /// The index of the file in the [`FileManager`].
        file_index: usize,
        /// The offset of the chunk within the file.
        offset: usize,
        /// The length of the chunk in bytes.
        length: usize,
        /// Indicates whether the chunk is part of a padding file. In this case,
        /// no I/O operation is performed and a stream of zeros is read directly
        /// from memory.
        padding: bool,
    }

    /// Computes the v1 piece hashes given the files. In v1, all files are treated
    /// as a single byte stream, which is then divided into pieces. This means
    /// that pieces can span file boundaries.
    ///
    /// Hash calculation is parallelized using `rayon` and [`FileManager`].
    pub(super) fn v1_piece_hashes(
        files: &[FileEntry],
        piece_length: usize,
        file_manager: &FileManager,
    ) -> Result<V1PieceHashes, Error> {
        let total_length: u64 = files.iter().map(|f| f.length).sum();
        if total_length == 0 {
            return Ok(V1PieceHashes(Vec::new()));
        }

        let num_pieces: usize = total_length
            .div_ceil(piece_length as u64)
            .try_into()
            .map_err(|_| Error::TorrentTooLargeForPlatform(piece_length))?;
        let mut piece_plans: Vec<Vec<V1ChunkPlan>> = vec![Vec::new(); num_pieces];

        let mut current_piece = 0;
        let mut current_piece_offset = 0;

        for (i, file) in files.iter().enumerate() {
            let mut file_remaining =
                usize::try_from(file.length).map_err(|_| Error::FileTooLarge(file.length))?;
            let mut file_offset: usize = 0;

            while file_remaining > 0 {
                let space_in_piece = piece_length - current_piece_offset;
                let take = file_remaining.min(space_in_piece);

                piece_plans[current_piece].push(V1ChunkPlan {
                    file_index: i,
                    offset: file_offset,
                    length: take,
                    padding: file.padding,
                });
                if !file.padding {
                    file_manager.register_use(i);
                }

                file_remaining -= take;
                file_offset += take;
                current_piece_offset += take;

                if current_piece_offset == piece_length {
                    current_piece += 1;
                    current_piece_offset = 0;
                }
            }
        }

        let hashes = piece_plans
            .into_par_iter()
            .map_init(Sha1::new, |sha1, plan| {
                for chunk in plan {
                    if chunk.padding {
                        let mut remaining = chunk.length;
                        while remaining > 0 {
                            let take = remaining.min(64 * 1024);
                            sha1.update(&ZEROS[..take]);
                            remaining -= take;
                        }
                    } else {
                        let mmap = file_manager.acquire(chunk.file_index)?;

                        let start = chunk.offset;
                        let end = start + chunk.length;

                        sha1.update(&mmap[start..end]);
                    }
                }
                Ok::<[u8; 20], Error>(sha1.finalize_reset().into())
            })
            .collect::<Result<Vec<[u8; 20]>, _>>()?;

        Ok(V1PieceHashes(hashes))
    }

    /// Computes the v2 hashes for the given file, which include the value of
    /// the `pieces root` field and the value of the corresponding entry in the
    /// `piece layers` field, if applicable. This is parallelized using `rayon`.
    pub(super) fn v2_file_hashes(
        piece_length: usize,
        file_manager: &FileManager,
        file_length: u64,
        file_idx: usize,
    ) -> Result<V2FileHashes, Error> {
        debug_assert!(piece_length.is_power_of_two());
        debug_assert!(piece_length >= V2_BLOCK_SIZE);

        if file_length == 0 {
            return Ok(V2FileHashes::Empty);
        }

        let padded_length: usize = file_length
            .max(V2_BLOCK_SIZE_U64)
            .next_power_of_two()
            .try_into()
            .map_err(|_| Error::FileTooLarge(file_length))?;

        let chunk_size = piece_length.min(padded_length);
        let target_depth = (chunk_size / V2_BLOCK_SIZE).ilog2();

        if target_depth >= 32 {
            return Err(Error::PieceLengthTooLarge(
                NonZeroU64::new(piece_length as u64).unwrap(),
            ));
        }

        let mmap = file_manager.acquire(file_idx)?;

        let real_piece_roots: Vec<[u8; 32]> = mmap
            .par_chunks(chunk_size)
            .map_init(Sha256::new, |hasher, chunk| {
                compute_piece_root(chunk, target_depth, hasher)
            })
            .collect();

        let num_padded_pieces = padded_length / chunk_size;
        let mut layer = real_piece_roots.clone();

        if layer.len() < num_padded_pieces {
            let pad_hash = empty_tree_hash(target_depth);
            layer.resize(num_padded_pieces, pad_hash);
        }

        while layer.len() > 1 {
            layer = layer
                .chunks_exact(2)
                .map(|chunk| {
                    let mut hasher = Sha256::new();
                    hasher.update(chunk[0]);
                    hasher.update(chunk[1]);
                    hasher.finalize().into()
                })
                .collect();
        }

        let root_hash = layer[0];

        if file_length > piece_length as u64 {
            Ok(V2FileHashes::MultiPiece {
                root: root_hash,
                layer: real_piece_roots,
            })
        } else {
            Ok(V2FileHashes::SinglePiece { root: root_hash })
        }
    }

    /// Computes the tree using stack until the `target_depth`.
    fn compute_piece_root(chunk: &[u8], target_depth: u32, hasher: &mut Sha256) -> [u8; 32] {
        let blocks_per_chunk = 1 << target_depth;

        let mut stack = [([0u8; 32], 0); 32];
        let mut stack_ptr = 0;

        for i in 0..blocks_per_chunk {
            let start = i * V2_BLOCK_SIZE;

            let leaf = if start < chunk.len() {
                let end = (start + V2_BLOCK_SIZE).min(chunk.len());
                hasher.update(&chunk[start..end]);
                hasher.finalize_reset().into()
            } else {
                [0u8; 32]
            };

            let mut current = leaf;
            let mut height = 0;

            while stack_ptr > 0 && stack[stack_ptr - 1].1 == height {
                stack_ptr -= 1;
                let prev = stack[stack_ptr].0;

                hasher.update(prev);
                hasher.update(current);
                current = hasher.finalize_reset().into();

                height += 1;
            }

            stack[stack_ptr] = (current, height);
            stack_ptr += 1;
        }

        debug_assert_eq!(stack_ptr, 1, "Stack did not cleanly reduce to 1 root");
        stack[0].0
    }

    /// Returns the hash of an empty tree of given depth. The hashes
    /// are computed only once after the first call.
    fn empty_tree_hash(depth: u32) -> [u8; 32] {
        debug_assert!(depth <= 32);

        let hashes = EMPTY_TREE_HASHES.get_or_init(|| {
            let mut table = [[0u8; 32]; 33];
            let mut current = [0u8; 32];

            table[0] = current;

            for item in table.iter_mut().skip(1) {
                let mut hasher = Sha256::new();
                hasher.update(current);
                hasher.update(current);
                current = hasher.finalize().into();
                *item = current;
            }

            table
        });

        hashes[depth as usize]
    }
}

/// Utility structs and functions that specifically deal with field construction for the torrent.
mod field_builders {
    use rayon::iter::IndexedParallelIterator;
    use url::Url;

    use crate::torrent::{
        FileInfoBuf, FileTreeBuf, TrackerTier,
        builder::{
            hashing::{v1_piece_hashes, v2_file_hashes},
            utils::{FileEntry, FileManager},
        },
    };

    use super::{
        BTreeMap, Cow, Error, FileInfo, FileInfoAttr, FileInfoAttrFlags, FileLeaf, FileMode,
        FileTree, FileTreeNode, InfoV1, InfoV1Buf, InfoV2, InfoV2Buf, IntoParallelRefIterator,
        NonZeroU64, ParallelIterator, Path, PathBuf, PieceLayers, PieceLayersBuf, SystemTime,
        UNIX_EPOCH, V2FileHashes,
    };

    /// Unprocessed fields common for both versions of BitTorrent. For more information on
    /// each of them, see the corresponding fields in [`Torrent`] and
    /// [`Info`]. For the default values, see [`TorrentBuilder`].
    ///
    /// [`Torrent`]: crate::Torrent
    /// [`Info`]: crate::torrent::Info
    /// [`TorrentBuilder`]: super::TorrentBuilder
    #[derive(Debug)]
    pub(super) struct CommonFields {
        /// The piece length chosen by the user, or [`None`] to pick one automatically based on
        /// the total size of the files.
        pub(super) piece_length: Option<NonZeroU64>,
        /// Whether the torrent is private.
        pub(super) private: bool,
        /// The value of the `source` field, if set.
        pub(super) source: Option<String>,
        /// The tracker tiers collected so far. The last tier is the one that new trackers are
        /// added to. May be empty or contain empty tiers; these are dropped during resolution.
        pub(super) tracker_tiers: Vec<TrackerTier>,
        /// The web seeds collected so far.
        pub(super) web_seeds: Vec<Url>,
        /// The creation date in seconds since the Unix epoch, or [`None`] to use the current
        /// time.
        pub(super) creation_date: Option<u64>,
        /// The value of the `created by` field, if set.
        pub(super) created_by: Option<String>,
        /// The value of the `comment` field, if set.
        pub(super) comment: Option<String>,
    }

    /// Resolved fields common for both versions of BitTorrent. For more information on
    /// each of them, see the corresponding fields in [`Torrent`] and
    /// [`Info`]. For the default values, see [`TorrentBuilder`].
    ///
    /// [`Torrent`]: crate::Torrent
    /// [`Info`]: crate::torrent::Info
    /// [`TorrentBuilder`]: super::TorrentBuilder
    #[derive(Debug)]
    pub(super) struct CommonFieldsResolved {
        /// The piece length, either supplied by the user or computed from the total size of
        /// the files.
        pub(super) piece_length: NonZeroU64,
        /// Whether the torrent is private.
        pub(super) private: bool,
        /// The value of the `source` field, if set.
        pub(super) source: Option<Cow<'static, str>>,
        /// The non-empty tracker tiers, or [`None`] if there are no trackers.
        pub(super) tracker_tiers: Option<Vec<TrackerTier>>,
        /// The web seeds, or [`None`] if there are none.
        pub(super) web_seeds: Option<Vec<Url>>,
        /// The creation date in seconds since the Unix epoch. Always [`Some`] after resolution.
        pub(super) creation_date: Option<u64>,
        /// The value of the `created by` field, if set.
        pub(super) created_by: Option<Cow<'static, str>>,
        /// The value of the `comment` field, if set.
        pub(super) comment: Option<Cow<'static, str>>,
        /// The torrent's encoding. Always `"UTF-8"`.
        pub(super) encoding: Option<Cow<'static, str>>,
    }

    /// Hashes the provided files and constructs an [`InfoV1`].
    pub(super) fn v1_fields(
        files: &[FileEntry],
        piece_length: usize,
        single_file: bool,
    ) -> Result<InfoV1Buf, Error> {
        let file_manager = FileManager::new(files);

        let file_infos = v1_file_infos(files)?;

        let piece_hashes = v1_piece_hashes(files, piece_length, &file_manager)?;

        let file_mode = match (file_infos.len(), single_file) {
            (0, _) => unreachable!("TorrentFactory<HasFiles> does not allow an empty file vector"),
            (1, true) => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
        };

        Ok(InfoV1 {
            pieces: Cow::Owned(piece_hashes.0),
            file_mode,
        })
    }

    /// Hashes the provided files and constructs an [`InfoV2`] and [`PieceLayers`].
    pub(super) fn v2_fields(
        files: &[FileEntry],
        piece_length: usize,
    ) -> Result<(InfoV2Buf, PieceLayersBuf), Error> {
        let file_manager = FileManager::new(files);
        for i in 0..files.len() {
            file_manager.register_use(i);
        }

        let hashes_list = files
            .par_iter()
            .enumerate()
            .map(|(i, file)| v2_file_hashes(piece_length, &file_manager, file.length, i))
            .collect::<Result<Vec<_>, _>>()?;
        let (file_tree, piece_layers) = v2_file_tree_and_piece_layers(files, hashes_list);

        Ok((InfoV2 { file_tree }, piece_layers))
    }

    /// Hashes the provided files and constructs an [`InfoV1`], an [`InfoV2`], and [`PieceLayers`].
    pub(super) fn hybrid_fields(
        files: &[FileEntry],
        piece_length: usize,
        single_file: bool,
    ) -> Result<(InfoV1Buf, InfoV2Buf, PieceLayersBuf), Error> {
        let mut files_pad = Vec::with_capacity(files.len().saturating_mul(2).saturating_sub(1));
        let mut v1_to_v2_ids = Vec::with_capacity(files.len());

        if let Some((last, rest)) = files.split_last() {
            let pl = piece_length as u64;
            for file in rest {
                v1_to_v2_ids.push(files_pad.len());
                files_pad.push(file.clone());

                let rem = file.length % pl;
                if rem != 0 {
                    let pad_len = file.length - rem;
                    files_pad.push(FileEntry {
                        disk_path: PathBuf::new(),
                        meta_path: Path::new(".pad").join(pad_len.to_string()),
                        length: pad_len,
                        padding: true,
                    });
                }
            }
            v1_to_v2_ids.push(files_pad.len());
            files_pad.push(last.clone());
        }

        let file_manager = FileManager::new(&files_pad);
        for &pad_idx in &v1_to_v2_ids {
            if files_pad[pad_idx].length > 0 {
                file_manager.register_use(pad_idx);
            }
        }

        let (v2_hashes_list_res, v1_piece_hashes_res) = rayon::join(
            || {
                files
                    .par_iter()
                    .enumerate()
                    .map(|(i, file)| {
                        let pad_idx = v1_to_v2_ids[i];
                        v2_file_hashes(piece_length, &file_manager, file.length, pad_idx)
                    })
                    .collect::<Result<Vec<_>, _>>()
            },
            || v1_piece_hashes(&files_pad, piece_length, &file_manager),
        );
        let v2_hashes_list = v2_hashes_list_res?;
        let v1_piece_hashes = v1_piece_hashes_res?;

        let file_infos = v1_file_infos(&files_pad)?;
        let file_mode = match (file_infos.len(), single_file) {
            (0, _) => return Err(Error::NoFiles),
            (1, true) => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
        };

        let (file_tree, piece_layers) = v2_file_tree_and_piece_layers(files, v2_hashes_list);

        Ok((
            InfoV1 {
                pieces: Cow::Owned(v1_piece_hashes.0),
                file_mode,
            },
            InfoV2 { file_tree },
            piece_layers,
        ))
    }

    /// Resolves the common fields, inserting the defaults for some non-optional ones.
    ///
    /// See [`TorrentBuilder`](super::TorrentBuilder) for more information on the defaults.
    pub(super) fn common_fields(
        common_fields: CommonFields,
        files: &[(PathBuf, u64)],
    ) -> CommonFieldsResolved {
        let piece_length = common_fields.piece_length.unwrap_or_else(|| {
            let total_length: u64 = files.iter().map(|(_, len)| len).sum();
            let target = (total_length / 1000).max(1);
            NonZeroU64::new((1 << target.ilog2()).clamp(16 * 1024, 16 * 1024 * 1024)).unwrap()
        });

        let creation_date = common_fields.creation_date.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });

        let tracker_tiers = common_fields
            .tracker_tiers
            .into_iter()
            .filter(|tier| !tier.is_empty())
            .collect::<Vec<_>>();

        let tracker_tiers = if tracker_tiers.is_empty() {
            None
        } else {
            Some(tracker_tiers)
        };

        let web_seeds = if common_fields.web_seeds.is_empty() {
            None
        } else {
            Some(common_fields.web_seeds)
        };

        CommonFieldsResolved {
            piece_length,
            private: common_fields.private,
            source: common_fields.source.map(Cow::Owned),
            tracker_tiers,
            web_seeds,
            creation_date: Some(creation_date),
            created_by: common_fields.created_by.map(Cow::Owned),
            comment: common_fields.comment.map(Cow::Owned),
            encoding: Some(Cow::Borrowed("UTF-8")),
        }
    }

    /// Constructs [`FileInfo`] instances for the given files.
    pub(super) fn v1_file_infos(files: &[FileEntry]) -> Result<Vec<FileInfoBuf>, Error> {
        let file_path_comps = files
            .iter()
            .map(|file| -> Result<Vec<String>, Error> {
                file.meta_path
                    .components()
                    .map(|c| {
                        Ok(c.as_os_str()
                            .to_str()
                            .ok_or(Error::NonUtf8Name)?
                            .to_string())
                    })
                    .collect()
            })
            .collect::<Result<Vec<_>, _>>()?;

        let res = files
            .iter()
            .zip(file_path_comps)
            .map(|(file, comps)| -> Result<FileInfo, Error> {
                let attr = if file.padding {
                    Some(FileInfoAttr::new(FileInfoAttrFlags::PADDING))
                } else {
                    None
                };

                Ok(FileInfo {
                    attr,
                    length: file.length,
                    md5sum: None,
                    path: comps.into_iter().map(Cow::Owned).collect(),
                    extra: BTreeMap::new(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(res)
    }

    /// Constructs the [`FileTree`] and [`PieceLayers`] given the files and their corresponding hashes.
    pub(super) fn v2_file_tree_and_piece_layers(
        files: &[FileEntry],
        hashes_list: Vec<V2FileHashes>,
    ) -> (FileTreeBuf, PieceLayersBuf) {
        let mut file_tree = FileTree::default();
        let mut piece_layers = PieceLayers::default();

        for (file, file_hashes) in files.iter().zip(hashes_list) {
            let mut current = &mut file_tree;

            let parent = file.meta_path.parent().unwrap_or_else(|| Path::new(""));
            for component in parent.components() {
                let component_str = component
                    .as_os_str()
                    .to_str()
                    .expect("UTF-8 correctness has already been checked")
                    .to_string();

                let node = current
                    .0
                    .entry(Cow::Owned(component_str))
                    .or_insert_with(|| FileTreeNode::Directory(FileTree::default()));

                current = match node {
                    FileTreeNode::Directory(dir) => dir,
                    FileTreeNode::File(_) => unreachable!(),
                };
            }

            let filename = file
                .meta_path
                .file_name()
                .expect("meta_path must have a file name component")
                .to_str()
                .expect("UTF-8 correctness has already been checked")
                .to_string();

            let (pieces_root, layer_opt) = match file_hashes {
                V2FileHashes::Empty => (None, None),
                V2FileHashes::SinglePiece { root } => (Some(Cow::Owned(root)), None),
                V2FileHashes::MultiPiece { root, layer } => {
                    (Some(Cow::Owned(root)), Some((root, layer)))
                }
            };

            current.0.insert(
                Cow::Owned(filename),
                FileTreeNode::File(FileLeaf {
                    length: file.length,
                    pieces_root,
                    extra: BTreeMap::new(),
                }),
            );

            if let Some((key, value)) = layer_opt {
                piece_layers
                    .0
                    .insert(key, Cow::Owned(value.into_flattened()));
            }
        }

        (file_tree, piece_layers)
    }
}

/// Utility structs and functions for torrent construction.
mod utils {
    use std::{
        ops::Deref,
        sync::{Arc, Mutex},
    };

    use memmap2::{Mmap, MmapOptions};
    use walkdir::WalkDir;

    use crate::torrent::builder::{FilterFn, field_builders::CommonFieldsResolved};

    use super::{
        BTreeMap, Cow, Error, File, Info, InfoHybrid, InfoV1Buf, InfoV2Buf, NonZeroU64, Path,
        PathBuf, PieceLayersBuf, Torrent, TorrentBuf, TorrentMeta, clean,
    };

    /// Attempts to convert a [`NonZeroU64`] `piece_length` to [`usize`].
    pub(super) fn piece_length_usize(piece_length: NonZeroU64) -> Result<usize, Error> {
        piece_length
            .get()
            .try_into()
            .map_err(|_| Error::PieceLengthTooLarge(piece_length))
    }

    /// Takes the raw paths and filters provided by the user and resolves file paths, returning a vector
    /// of tuples of the form `(<file_path>, <file_length>)`.
    ///
    /// The function requests metadata for each non-filtered path. If a path corresponds to a file,
    /// it is added as is. If a path corresponds to a directory, it is traversed using
    /// [`WalkDir`] to search for files. If a path corresponds to neither file nor directory,
    /// an [`enum@Error`] is returned. The function also propagates any [`WalkDir`] errors via [`enum@Error`].
    pub(super) fn resolve_file_paths(
        paths: Vec<PathBuf>,
        filters: &[FilterFn],
        follow_symlinks: bool,
    ) -> Result<Vec<(PathBuf, u64)>, Error> {
        let mut files = Vec::with_capacity(paths.len());

        for path in paths {
            match path.metadata()? {
                m if m.is_file() => {
                    if filters.iter().all(|filter| filter(&path)) {
                        files.push((path, m.len()));
                    }
                }
                m if m.is_dir() => {
                    let new_files = WalkDir::new(&path)
                        .follow_links(follow_symlinks)
                        .into_iter()
                        .filter_entry(|entry| {
                            let path = entry.path();
                            filters.iter().all(|filter| filter(path))
                        })
                        .filter_map(|entry| {
                            let entry = match entry {
                                Ok(e) => e,
                                Err(e) => return Some(Err(Error::from(e))),
                            };
                            if !entry.file_type().is_file() {
                                return None;
                            }
                            let result = entry
                                .metadata()
                                .map_err(Error::from)
                                .map(|m| (entry.into_path(), m.len()));
                            Some(result)
                        })
                        .collect::<Result<Vec<_>, _>>()?;

                    files.extend(new_files);
                }
                _ => return Err(Error::UnsupportedFileType(path)),
            }
        }

        if files.is_empty() {
            Err(Error::NoFiles)
        } else {
            Ok(files)
        }
    }

    /// Resolves the torrent's name. See [`TorrentBuilder`](crate::torrent::builder::TorrentBuilder)
    /// for more information.
    pub(super) fn resolve_name(
        name: Option<String>,
        files: &[FileEntry],
        single_file: bool,
        common_prefix: &Path,
    ) -> Result<Cow<'static, str>, Error> {
        Ok(match (name, files, single_file) {
            (Some(name), ..) => Cow::Owned(name),
            (None, [file], true) => Cow::Owned(
                file.disk_path
                    .components()
                    .next_back()
                    .and_then(|c| c.as_os_str().to_str())
                    .ok_or(Error::NonUtf8Name)?
                    .to_string(),
            ),
            (None, ..) => {
                if !clean(common_prefix).starts_with("..")
                    && let Ok(absolute_prefix) = common_prefix.canonicalize()
                    && let Some(last) = absolute_prefix.components().next_back()
                {
                    Cow::Owned(
                        last.as_os_str()
                            .to_str()
                            .ok_or(Error::NonUtf8Name)?
                            .to_string(),
                    )
                } else {
                    Cow::Borrowed("New Torrent")
                }
            }
        })
    }

    /// Determines the common prefix among the provided paths and constructs [`FileEntry`] elements
    /// without it. The output is of the form `(<prefix>, <file_entries>)`.
    pub(super) fn remove_common_prefix(paths: &[(PathBuf, u64)]) -> (PathBuf, Vec<FileEntry>) {
        debug_assert!(!paths.is_empty());

        let mut prefix = paths[0]
            .0
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        'prefix_search: for (s, _) in &paths[1..] {
            while !s.starts_with(&prefix) {
                if !prefix.pop() {
                    break 'prefix_search;
                }
            }
        }

        let paths_no_prefix = paths
            .iter()
            .map(|(p, _)| p.strip_prefix(&prefix).unwrap_or(p));

        let file_entries = paths
            .iter()
            .zip(paths_no_prefix)
            .map(|((disk_path, length), meta_path)| FileEntry {
                disk_path: disk_path.clone(),
                meta_path: meta_path.to_path_buf(),
                length: *length,
                padding: false,
            })
            .collect();

        if prefix.as_os_str().is_empty() {
            (prefix, file_entries)
        } else {
            (clean(prefix), file_entries)
        }
    }

    /// Constructs the final [`Torrent`] from different parts.
    pub(super) fn torrent_from_parts(
        name: Cow<'static, str>,
        common_fields: CommonFieldsResolved,
        v1: Option<InfoV1Buf>,
        v2: Option<InfoV2Buf>,
        piece_layers: Option<PieceLayersBuf>,
    ) -> TorrentBuf {
        let meta = match (v1, v2, piece_layers) {
            (Some(v1), Some(v2), Some(piece_layers)) => TorrentMeta::Hybrid {
                info: Info {
                    name,
                    piece_length: common_fields.piece_length,
                    private: common_fields.private.then_some(true),
                    source: common_fields.source,
                    kind: InfoHybrid { v1, v2 },
                    extra: BTreeMap::new(),
                    raw: None,
                },
                piece_layers,
            },
            (Some(v1), None, None) => TorrentMeta::V1 {
                info: Info {
                    name,
                    piece_length: common_fields.piece_length,
                    private: common_fields.private.then_some(true),
                    source: common_fields.source,
                    kind: v1,
                    extra: BTreeMap::new(),
                    raw: None,
                },
            },
            (None, Some(v2), Some(piece_layers)) => TorrentMeta::V2 {
                info: Info {
                    name,
                    piece_length: common_fields.piece_length,
                    private: common_fields.private.then_some(true),
                    source: common_fields.source,
                    kind: v2,
                    extra: BTreeMap::new(),
                    raw: None,
                },
                piece_layers,
            },
            _ => unreachable!("Invariants violated"),
        };

        Torrent {
            tracker_tiers: common_fields.tracker_tiers,
            web_seeds: common_fields.web_seeds,
            creation_date: common_fields.creation_date,
            comment: common_fields.comment,
            created_by: common_fields.created_by,
            encoding: common_fields.encoding,
            meta,
        }
    }

    /// A file entry in [`TorrentBuilder`](crate::torrent::builder::TorrentBuilder).
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(super) struct FileEntry {
        /// The file path provided by the user. Used to read the file contents.
        pub(super) disk_path: PathBuf,
        /// The file path inside the torrent. Essentially the same as [`FileEntry::disk_path`]
        /// but with the common prefix removed.
        pub(super) meta_path: PathBuf,
        /// The length of the file in bytes.
        pub(super) length: u64,
        /// Indicates whether the file is a padding file.
        ///
        /// Padding files do not exist in the real file system and are just simulated byte streams
        /// of all zeros.
        pub(super) padding: bool,
    }

    /// A RAII file manager for multithreaded hashing operations.
    ///
    /// For each thread that intends to use a file, a use should be "registered" via
    /// [`FileManager::register_use`] to increase the internal counter. The files
    /// are memory mapped lazily and provided to threads using [`MmapGuard`]. After the thread
    /// drops the [`MmapGuard`], the use counter decreases by 1. When the counter reaches 0, the memory
    /// map is dropped.
    #[derive(Debug)]
    pub(super) struct FileManager<'a> {
        /// The files that will be used for hashing.
        files: &'a [FileEntry],
        /// The vector of file states.
        states: Vec<Mutex<FileState>>,
    }

    /// The file counter and the associated memory map if the file has already been acquired.
    #[derive(Debug, Default)]
    pub(super) struct FileState {
        /// The number of registered uses for the file.
        uses: usize,
        /// The file's memory map if it has already been acquired or [`None`] if it has not.
        mmap: Option<Arc<Mmap>>,
    }

    /// A RAII guard that stores the [`Arc`] to the memory map for the duration of the read operation.
    ///
    /// When the guard is dropped, the corresponding [`FileState::uses`] counter reduces by 1. If it
    /// reaches 0, the memory map is dropped.
    pub(super) struct MmapGuard<'a> {
        /// The file's memory map.
        mmap: Arc<Mmap>,
        /// The file state associated with the file being read.
        state: &'a Mutex<FileState>,
    }

    impl Deref for MmapGuard<'_> {
        type Target = [u8];
        fn deref(&self) -> &Self::Target {
            &self.mmap
        }
    }

    impl Drop for MmapGuard<'_> {
        /// Decreases the associated [`FileState::uses`] counter by 1. If the counter reaches 0,
        /// [`FileState::mmap`] gets set to [`None`], which drops the underlying memory map.
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            state.uses -= 1;
            if state.uses == 0 {
                state.mmap = None;
            }
        }
    }

    impl<'a> FileManager<'a> {
        /// Creates a new [`FileManager`] from the given files.
        pub fn new(files: &'a [FileEntry]) -> Self {
            let states = std::iter::repeat_with(|| Mutex::new(FileState::default()))
                .take(files.len())
                .collect();
            Self { files, states }
        }

        /// Registers a use for the file.
        ///
        /// Once the file's memory map is loaded into RAM, it will not dropped until its use
        /// counter is decreased to 0.
        pub fn register_use(&self, file_index: usize) {
            let mut state = self.states[file_index].lock().unwrap();
            state.uses += 1;
        }

        /// Acquires the file's memory map.
        ///
        /// The memory maps are created lazily when this method is called for the first time
        /// for a given file.
        pub fn acquire(&self, file_index: usize) -> Result<MmapGuard<'_>, Error> {
            let mut state = self.states[file_index].lock().unwrap();

            if state.mmap.is_none() {
                let file_entry = &self.files[file_index];
                let len = usize::try_from(file_entry.length)
                    .map_err(|_| Error::FileTooLarge(file_entry.length))?;
                let f = File::open(&file_entry.disk_path)?;
                let mmap = unsafe { MmapOptions::new().len(len).map(&f)? };
                state.mmap = Some(Arc::new(mmap));
            }

            let mmap = state.mmap.as_ref().unwrap().clone();

            Ok(MmapGuard {
                mmap,
                state: &self.states[file_index],
            })
        }
    }
}

/// A path filter function.
///
/// The function receives a path found during traversal and returns `true` to keep it or `false`
/// to exclude it. Filters are added with [`TorrentBuilder::add_filter`]; a path is kept only if
/// all filters accept it.
pub type FilterFn = Box<dyn Fn(&Path) -> bool + Send + Sync>;

/// A typestate builder for constructing [`Torrent`]s from files on disk.
///
/// `TorrentBuilder` collects one or more paths, hashes the resulting files, and assembles a
/// [`TorrentBuf`] in v1-only, v2-only, or hybrid form via [`build_v1`](TorrentBuilder::build_v1),
/// [`build_v2`](TorrentBuilder::build_v2), or [`build_hybrid`](TorrentBuilder::build_hybrid)
/// (aliased as [`build`](TorrentBuilder::build)). Any metadata field left unconfigured falls
/// back to a sensible default when one of these is called; see [Defaults](#defaults) below.
///
/// # Typestate
///
/// The builder is generic over a `State` marker ([`state::Empty`] or [`state::HasPaths`]) that
/// tracks whether at least one path has been supplied:
///
/// - **[`TorrentBuilder<state::Empty>`]** is the builder's starting state, produced by
///   [`TorrentBuilder::new`] (or its [`Default`] impl). All metadata methods are available, but
///   [`build`](TorrentBuilder::build) and its variants are not, since there are no files yet.
/// - **[`TorrentBuilder<state::HasPaths>`]** is reached once a path has been supplied via
///   [`add_path`](TorrentBuilder::add_path) or [`add_paths`](TorrentBuilder::add_paths). Only
///   in this state can the torrent actually be built.
///
/// This makes building a torrent with no files a compile-time error rather than a runtime one.
///
/// # Examples
///
/// ```no_run
/// # use bitors::{Torrent, torrent::builder::Error};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let torrent = Torrent::builder()
///     .name("my_torrent")
///     .private(true)
///     .add_tracker("https://tracker.example.com/announce".parse()?)
///     .add_path("my_folder")
///     .build()?;
/// # Ok(())
/// # }
/// ```
///
/// # Defaults
///
/// | Field | Default when unset | Set via |
/// |---|---|---|
/// | `name` | The lone file's name if there is exactly one file; otherwise the last path component of the files' common ancestor directory, if it can be canonicalized; otherwise the literal string `"New Torrent"`. | [`name`](TorrentBuilder::name) |
/// | `piece_length` | Chosen automatically for roughly 1000 pieces: `total_length / 1000`, rounded down to the nearest power of two, clamped to `[16 KiB, 16 MiB]`. | [`piece_length`](TorrentBuilder::piece_length) |
/// | `private` | `false` | [`private`](TorrentBuilder::private) |
/// | `source` | `None` | [`source`](TorrentBuilder::source) |
/// | tracker tiers | `None` (no trackers) — an empty tier list, or one containing only empty tiers, is normalized to `None`. | [`add_tracker`](TorrentBuilder::add_tracker), [`add_trackers`](TorrentBuilder::add_trackers), [`next_tracker_tier`](TorrentBuilder::next_tracker_tier) |
/// | web seeds | `None` (no web seeds) | [`add_web_seed`](TorrentBuilder::add_web_seed), [`add_web_seeds`](TorrentBuilder::add_web_seeds) |
/// | `creation_date` | The current Unix timestamp (seconds since the epoch) at build time. | [`creation_date`](TorrentBuilder::creation_date) |
/// | `created_by` | `None` — not auto-populated with crate name or version. | [`created_by`](TorrentBuilder::created_by) |
/// | `comment` | `None` | [`comment`](TorrentBuilder::comment) |
/// | `encoding` | Always `"UTF-8"`; not configurable through the builder. | — |
/// | `follow_symlinks` | `false` — symlinks encountered while traversing directories are not followed. | [`follow_symlinks`](TorrentBuilder::follow_symlinks) |
/// | path filters | None — no files are excluded from traversal. | [`add_filter`](TorrentBuilder::add_filter) |
///
/// # Piece length constraints
///
/// [`build_v2`](TorrentBuilder::build_v2) and [`build_hybrid`](TorrentBuilder::build_hybrid)
/// (and therefore [`build`](TorrentBuilder::build), which calls `build_hybrid`) require the
/// piece length — explicit or defaulted — to be a power of two of at least 16 KiB (16384
/// bytes), per BitTorrent v2. A violation returns [`Error::InvalidPieceLengthV2`].
/// [`build_v1`](TorrentBuilder::build_v1) has no such restriction.
pub struct TorrentBuilder<State> {
    /// The files and directories supplied by the user. Directories are traversed recursively
    /// when the torrent is built.
    paths: Vec<PathBuf>,
    /// The filters applied to the paths found during traversal. A path is excluded if any
    /// filter rejects it.
    filters: Vec<FilterFn>,
    /// The torrent name chosen by the user, or [`None`] to derive one from the files.
    name: Option<String>,
    /// The fields shared by all torrent versions that were configured through the builder.
    common_fields: CommonFields,
    /// Whether symlinks encountered during traversal are followed.
    follow_symlinks: bool,
    /// Marks whether any paths have been supplied (see the [`state`] module).
    _state: PhantomData<State>,
}

// ── Methods available in both states ────────────────────────────────────────

impl<T> TorrentBuilder<T> {
    /// Sets the custom name for the torrent.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the piece length for the torrent.
    ///
    /// Note that in v2-only and hybrid torrents, the piece length must be
    /// a power of two and at least 16 KiB (16384). Torrent building will fail
    /// if these conditions are not satisfied.
    #[must_use]
    pub fn piece_length(mut self, piece_length: NonZeroU64) -> Self {
        self.common_fields.piece_length = Some(piece_length);
        self
    }

    /// Changes whether the torrent is private.
    #[must_use]
    pub fn private(mut self, private: bool) -> Self {
        self.common_fields.private = private;
        self
    }

    /// Sets the value of the `source` field in the torrent.
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.common_fields.source = Some(source.into());
        self
    }

    /// Sets the creation date of the torrent.
    #[must_use]
    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.common_fields.creation_date = Some(creation_date);
        self
    }

    /// Sets the value of the `created by` field in the torrent.
    #[must_use]
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        self.common_fields.created_by = Some(created_by.into());
        self
    }

    /// Adds the comment to the torrent.
    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.common_fields.comment = Some(comment.into());
        self
    }

    /// Adds a tracker URL to the current tracker tier.
    #[must_use]
    pub fn add_tracker(mut self, tracker: Url) -> Self {
        self.last_tracker_tier_mut().0.push(tracker);
        self
    }

    /// Adds several tracker URLs to the current tracker tier.
    #[must_use]
    pub fn add_trackers<I: IntoIterator<Item = Url>>(mut self, trackers: I) -> Self {
        self.last_tracker_tier_mut().0.extend(trackers);
        self
    }

    /// Starts a new tracker tier.
    ///
    /// No-op if the current tracker tier is empty.
    #[must_use]
    pub fn next_tracker_tier(mut self) -> Self {
        if !self.last_tracker_tier_mut().is_empty() {
            self.common_fields
                .tracker_tiers
                .push(TrackerTier::default());
        }
        self
    }

    /// Adds a web seed to the torrent.
    #[must_use]
    pub fn add_web_seed(mut self, seed: Url) -> Self {
        self.common_fields.web_seeds.push(seed);
        self
    }

    /// Adds several web seeds to the torrent.
    #[must_use]
    pub fn add_web_seeds<I: IntoIterator<Item = Url>>(mut self, seeds: I) -> Self {
        self.common_fields.web_seeds.extend(seeds);
        self
    }

    /// Adds a path filter function to this builder.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use bitors::{Torrent, torrent::builder::Error};
    /// # fn main() -> Result<(), Error> {
    /// let torrent = Torrent::builder()
    ///     .add_path("my_folder")
    ///     .add_filter(|path| path.file_name().is_some_and(|name| name != ".gitignore"))
    ///     .build()?;
    ///
    /// // Do something else...
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn add_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        self.filters.push(Box::new(filter));
        self
    }

    /// Adds a path to this builder for reading.
    ///
    /// The path can either represent a file or a directory.
    /// If it represents a directory, it will be traversed recursively for files.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use bitors::{Torrent, torrent::builder::Error};
    /// # fn main() -> Result<(), Error> {
    /// let torrent = Torrent::builder()
    ///     .add_path("my_file.txt")
    ///     .add_path("my_folder") // recursively traversed
    ///     .build()?;
    /// // Do something else...
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn add_path(mut self, path: impl Into<PathBuf>) -> TorrentBuilder<HasPaths> {
        self.paths.push(path.into());
        self.into_state()
    }

    /// Tells the builder whether the builder should follow the symlinks it encounters
    /// during path traversals.
    ///
    /// The default value is `false`.
    #[must_use]
    pub fn follow_symlinks(mut self, follow_symlinks: bool) -> Self {
        self.follow_symlinks = follow_symlinks;
        self
    }

    /// Gets a mutable reference to the last tracker tier. If the tracker tier list is empty,
    /// a tracker tier is created.
    fn last_tracker_tier_mut(&mut self) -> &mut TrackerTier {
        if self.common_fields.tracker_tiers.is_empty() {
            self.common_fields
                .tracker_tiers
                .push(TrackerTier::default());
        }
        self.common_fields.tracker_tiers.last_mut().unwrap()
    }

    /// Converts the builder from one state to another while preserving the values
    /// of all fields.
    fn into_state<S>(self) -> TorrentBuilder<S> {
        TorrentBuilder {
            paths: self.paths,
            filters: self.filters,
            name: self.name,
            common_fields: self.common_fields,
            follow_symlinks: self.follow_symlinks,
            _state: PhantomData,
        }
    }
}

// ── Empty state ──────────────────────────────────────────────────────────────

impl Default for TorrentBuilder<state::Empty> {
    /// Creates an empty builder. Equivalent to [`TorrentBuilder::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentBuilder<state::Empty> {
    /// Creates an empty builder.
    ///
    /// At least one path must be supplied to the builder via [`TorrentBuilder::add_path`] or
    /// [`TorrentBuilder::add_paths`] to enable [torrent building](TorrentBuilder::build).
    ///
    /// This is equivalent to [`Torrent::builder`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            paths: vec![],
            filters: vec![],
            name: None,
            common_fields: CommonFields {
                piece_length: None,
                private: false,
                source: None,
                tracker_tiers: vec![],
                web_seeds: vec![],
                creation_date: None,
                created_by: None,
                comment: None,
            },
            follow_symlinks: false,
            _state: PhantomData,
        }
    }

    /// Adds several paths to the builder for reading.
    ///
    /// The paths can either represent files or directories.
    /// Each one representing a directory will be traversed recursively for files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoPaths`] if an empty iterator was supplied.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use bitors::{Torrent, torrent::builder::Error};
    /// # fn main() -> Result<(), Error> {
    /// let torrent = Torrent::builder()
    ///     .add_paths(["my_file.txt", "my_folder"])?
    ///     .build()?;
    /// // Do something else...
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(
        mut self,
        paths: I,
    ) -> Result<TorrentBuilder<HasPaths>, Error> {
        let mut paths = paths.into_iter().map(Into::into);
        let first_path = paths.next().ok_or(Error::NoPaths)?;

        self.paths.push(first_path);
        self.paths.extend(paths);

        Ok(self.into_state())
    }
}

// ── HasPaths state ───────────────────────────────────────────────────────────

impl TorrentBuilder<state::HasPaths> {
    /// Creates a builder initialized with a path.
    ///
    /// This is equivalent to calling [`TorrentBuilder::add_path`] on an empty [`TorrentBuilder`].
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        TorrentBuilder::new().add_path(path)
    }

    /// Creates a builder initialized with several paths.
    ///
    /// This is equivalent to calling [`TorrentBuilder::add_paths`] on an empty [`TorrentBuilder`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoPaths`] if an empty iterator was supplied.
    pub fn from_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(paths: I) -> Result<Self, Error> {
        TorrentBuilder::new().add_paths(paths)
    }

    /// Adds several paths to the builder for reading.
    ///
    /// The paths can either represent files or directories.
    /// Each one representing a directory will be traversed recursively for files.
    ///
    /// Unlike its counterpart in `TorrentBuilder<Empty>`, this one is infallible and a no-op if
    /// an empty iterator was supplied.
    #[must_use]
    pub fn add_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(mut self, paths: I) -> Self {
        self.paths.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Builds a hybrid torrent.
    ///
    /// # Errors
    ///
    /// Returns an [`enum@Error`] in the following cases:
    /// - No files were found after traversing all of the supplied paths;
    /// - An I/O error occurred;
    /// - A path traversal error occurred;
    /// - One of the file paths was not a valid UTF-8 (this is required by BitTorrent specs);
    /// - One of the file paths represented neither a file nor a directory and was not a symlink;
    /// - The piece length or one of the provided files was too large (32-bit systems only);
    ///
    /// Also returns an [`Error::InvalidPieceLengthV2`] if the piece length was invalid according to BitTorrent v2.
    /// See [`TorrentBuilder::build_v2`] for more information.
    pub fn build(self) -> Result<TorrentBuf, Error> {
        self.build_hybrid()
    }

    /// Builds a v1-only torrent.
    ///
    /// # Errors
    ///
    /// See [`TorrentBuilder::build`] (except [`Error::InvalidPieceLengthV2`]).
    pub fn build_v1(self) -> Result<TorrentBuf, Error> {
        let mut files = resolve_file_paths(self.paths, &self.filters, self.follow_symlinks)?;
        let single_file = files.len() == 1;
        let common_fields = common_fields(self.common_fields, &files);

        files.sort();

        let (common_prefix, files) = remove_common_prefix(&files);
        let v1 = v1_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            single_file,
        )?;

        let name = resolve_name(self.name, &files, single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            None,
            None,
        ))
    }

    /// Builds a v2-only torrent.
    ///
    /// # Errors
    ///
    /// See [`TorrentBuilder::build`].
    pub fn build_v2(self) -> Result<TorrentBuf, Error> {
        let mut files = resolve_file_paths(self.paths, &self.filters, self.follow_symlinks)?;
        let single_file = files.len() == 1;
        let common_fields = common_fields(self.common_fields, &files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        files.sort();

        let (common_prefix, files) = remove_common_prefix(&files);

        let (v2, v2_ext) = v2_fields(&files, piece_length_usize(common_fields.piece_length)?)?;

        let name = resolve_name(self.name, &files, single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            None,
            Some(v2),
            Some(v2_ext),
        ))
    }

    /// Builds a hybrid torrent.
    ///
    /// This is equivalent to [`TorrentBuilder::build`].
    ///
    /// # Errors
    ///
    /// See [`TorrentBuilder::build`].
    pub fn build_hybrid(self) -> Result<TorrentBuf, Error> {
        let mut files = resolve_file_paths(self.paths, &self.filters, self.follow_symlinks)?;
        let single_file = files.len() == 1;
        let common_fields = common_fields(self.common_fields, &files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        files.sort();

        let (common_prefix, files) = remove_common_prefix(&files);

        let (v1, v2, v2_ext) = hybrid_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            single_file,
        )?;

        let name = resolve_name(self.name, &files, single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            Some(v2),
            Some(v2_ext),
        ))
    }
}

/// Errors that can arise while building torrents.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A path traversal error occurred.
    #[error("Error while walking a directory: {0}")]
    WalkDir(#[from] walkdir::Error),

    /// An empty iterator was provided to an empty torrent builder.
    #[error("An empty iterator provided to `add_paths`")]
    NoPaths,

    /// No files were found after traversing all of the provided paths.
    #[error("No files were found after traversal")]
    NoFiles,

    /// A file or a directory path was not valid UTF-8 as required by BitTorrent specs.
    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,

    /// A path represented neither a file, a directory, nor a symlink.
    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),

    /// The length of the file was too large for the platform address space (32-bit systems only).
    #[error("file length {0} is too large for the platform address space")]
    FileTooLarge(u64),

    /// The files were too large for the given piece length on this platform (32-bit systems only).
    #[error(
        "Files cannot be processed with the given piece length in this platform address space: {0}"
    )]
    TorrentTooLargeForPlatform(usize),

    /// The provided piece length was invalid for a v2-only or hybrid torrent.
    ///
    /// Per BitTorrent v2, the piece length must be a power of two and at least 16 KiB (16384 bytes).
    #[error("Piece length must be a power of two and at least 16 KiB in BitTorrent v2: {0}")]
    InvalidPieceLengthV2(NonZeroU64),

    /// The provided piece length was too large for this platform, i.e. exceeded [`usize::MAX`].
    ///
    /// This can happen only on 32-bit systems for legitimate torrents.
    #[error("The provided piece length is too large: {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
