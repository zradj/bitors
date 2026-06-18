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
use walkdir::WalkDir;

use crate::torrent::{
    FileInfo, FileInfoAttr, FileInfoAttrFlags, FileLeaf, FileMode, FileTree, FileTreeNode, Info,
    InfoHybrid, InfoV1, InfoV1Buf, InfoV2, InfoV2Buf, PieceLayers, PieceLayersBuf, Torrent,
    TorrentBuf, TorrentMeta, TrackerTier,
    builder::{
        field_builders::{common_fields, hybrid_fields, v1_fields, v2_fields},
        hashing::V2FileHashes,
        state::HasFiles,
        utils::{
            CommonFields, piece_length_usize, remove_common_prefix, resolve_name,
            torrent_from_parts,
        },
    },
};

// 16 KiB (per BEP 52)
const V2_BLOCK_SIZE: usize = 16 * 1024;
const V2_BLOCK_SIZE_U64: u64 = V2_BLOCK_SIZE as u64;

pub mod state {

    #[derive(Debug)]
    pub struct Empty;

    #[derive(Debug)]
    pub struct HasFiles;
}

mod hashing {

    use crate::torrent::builder::utils::{FileEntry, FileManager};
    use rayon::prelude::*;

    use super::{
        Digest, Error, ParallelIterator, ParallelSlice, Sha1, Sha256, V2_BLOCK_SIZE,
        V2_BLOCK_SIZE_U64,
    };

    static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

    #[derive(Debug)]
    pub(super) struct V1PieceHashes(pub(super) Vec<[u8; 20]>);

    #[derive(Debug)]
    pub(super) enum V2FileHashes {
        Empty,
        SinglePiece {
            root: [u8; 32],
        },
        MultiPiece {
            root: [u8; 32],
            layer: Vec<[u8; 32]>,
        },
    }

    #[derive(Debug, Clone)]
    struct V1ChunkPlan {
        file_index: usize,
        offset: usize,
        length: usize,
        padding: bool,
    }

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
            .map_err(|_| Error::PieceLengthTooSmall(piece_length))?;
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
                file_manager.register_use(i);

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

    pub(super) fn v2_file_hashes(
        file: &FileEntry,
        piece_length: usize,
        file_manager: &FileManager,
        fm_idx: usize,
    ) -> Result<V2FileHashes, Error> {
        debug_assert!(piece_length.is_power_of_two());
        debug_assert!(piece_length >= V2_BLOCK_SIZE);

        if file.length == 0 {
            return Ok(V2FileHashes::Empty);
        }

        let padded_length: usize = file
            .length
            .max(V2_BLOCK_SIZE_U64)
            .next_power_of_two()
            .try_into()
            .map_err(|_| Error::FileTooLarge(file.length))?;

        let chunk_size = piece_length.min(padded_length);
        let target_depth = (chunk_size / V2_BLOCK_SIZE).ilog2();

        let mmap = file_manager.acquire(fm_idx)?;

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

        if file.length > piece_length as u64 {
            Ok(V2FileHashes::MultiPiece {
                root: root_hash,
                layer: real_piece_roots,
            })
        } else {
            Ok(V2FileHashes::SinglePiece { root: root_hash })
        }
    }

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

    fn empty_tree_hash(depth: u32) -> [u8; 32] {
        let mut hash = [0u8; 32];
        for _ in 0..depth {
            let mut hasher = Sha256::new();
            hasher.update(hash);
            hasher.update(hash);
            hash = hasher.finalize().into();
        }
        hash
    }
}

mod field_builders {
    use rayon::iter::IndexedParallelIterator;

    use crate::torrent::builder::{
        hashing::{v1_piece_hashes, v2_file_hashes},
        utils::{CommonFields, CommonFieldsResolved, FileEntry, FileManager},
    };

    use super::{
        BTreeMap, Cow, Error, FileInfo, FileInfoAttr, FileInfoAttrFlags, FileLeaf, FileMode,
        FileTree, FileTreeNode, InfoV1, InfoV1Buf, InfoV2, InfoV2Buf, IntoParallelRefIterator,
        NonZeroU64, ParallelIterator, Path, PathBuf, PieceLayers, PieceLayersBuf, SystemTime,
        UNIX_EPOCH, V2FileHashes,
    };

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
            .map(|(i, file)| v2_file_hashes(file, piece_length, &file_manager, i))
            .collect::<Result<Vec<_>, _>>()?;
        let (file_tree, piece_layers_entries) = v2_file_tree_and_piece_layers(files, hashes_list);

        let piece_layers = {
            let mut res = BTreeMap::new();

            for (hash, layer) in piece_layers_entries {
                res.insert(Cow::Owned(hash), Cow::Owned(layer.into_flattened()));
            }

            res
        };
        let piece_layers = PieceLayers(piece_layers);

        Ok((InfoV2 { file_tree }, piece_layers))
    }

    pub(super) fn hybrid_fields(
        files: &[FileEntry],
        piece_length: usize,
        single_file: bool,
    ) -> Result<(InfoV1Buf, InfoV2Buf, PieceLayersBuf), Error> {
        let mut files_pad = Vec::with_capacity(files.len());
        let mut v1_to_v2_ids = Vec::with_capacity(files.len());
        for file in files {
            v1_to_v2_ids.push(files_pad.len());
            files_pad.push(file.clone());

            let pad_len = file.length.next_multiple_of(piece_length as u64) - file.length;
            if pad_len > 0 {
                files_pad.push(FileEntry {
                    disk_path: PathBuf::new(),
                    meta_path: PathBuf::from(format!(".pad/{pad_len}")),
                    length: pad_len,
                    padding: true,
                });
            }
        }
        let single_file = single_file && (files_pad.len() == files.len());

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
                        v2_file_hashes(file, piece_length, &file_manager, pad_idx)
                    })
                    .collect::<Result<Vec<_>, _>>()
            },
            || v1_piece_hashes(&files_pad, piece_length, &file_manager),
        );
        let v2_hashes_list = v2_hashes_list_res?;
        let v1_piece_hashes = v1_piece_hashes_res?;

        let file_infos = v1_file_infos(&files_pad)?;
        let file_mode = match (file_infos.len(), single_file) {
            (0, _) => unreachable!("TorrentFactory<HasFiles> does not allow an empty file vector"),
            (1, true) => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
        };

        let (file_tree, piece_layers_entries) =
            v2_file_tree_and_piece_layers(files, v2_hashes_list);
        let piece_layers = {
            let mut res = BTreeMap::new();

            for (hash, layer) in piece_layers_entries {
                res.insert(
                    Cow::Owned(hash),
                    Cow::Owned(layer.as_slice().as_flattened().to_vec()),
                );
            }

            res
        };
        let piece_layers = PieceLayers(piece_layers);

        Ok((
            InfoV1 {
                pieces: Cow::Owned(v1_piece_hashes.0),
                file_mode,
            },
            InfoV2 { file_tree },
            piece_layers,
        ))
    }

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
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

        let tracker = common_fields
            .tracker_tiers
            .first()
            .and_then(|tier| tier.first().cloned());

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
            tracker,
            tracker_tiers,
            web_seeds,
            creation_date: Some(creation_date),
            created_by: common_fields.created_by.map(Cow::Owned),
            comment: common_fields.comment.map(Cow::Owned),
            encoding: Some(Cow::Borrowed("UTF-8")),
        }
    }

    pub(super) fn v1_file_infos(files: &[FileEntry]) -> Result<Vec<FileInfo<'static>>, Error> {
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
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(res)
    }

    pub(super) fn v2_file_tree_and_piece_layers(
        files: &[FileEntry],
        hashes_list: Vec<V2FileHashes>,
    ) -> (FileTree<'static>, Vec<([u8; 32], Vec<[u8; 32]>)>) {
        let mut file_tree = FileTree::default();
        let mut piece_layers = vec![];

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
                }),
            );

            if let Some(layer_entry) = layer_opt {
                piece_layers.push(layer_entry);
            }
        }

        (file_tree, piece_layers)
    }
}

mod utils {
    use std::{
        ops::Deref,
        sync::{Arc, Mutex},
    };

    use memmap2::{Mmap, MmapOptions};

    use crate::torrent::TrackerTier;

    use super::{
        Cow, Error, File, Info, InfoHybrid, InfoV1Buf, InfoV2Buf, NonZeroU64, Path, PathBuf,
        PieceLayersBuf, Torrent, TorrentBuf, TorrentMeta, Url, clean,
    };

    pub(super) fn piece_length_usize(piece_length: NonZeroU64) -> Result<usize, Error> {
        piece_length
            .get()
            .try_into()
            .map_err(|_| Error::PieceLengthTooLarge(piece_length))
    }

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
                    private: common_fields.private,
                    source: common_fields.source,
                    kind: InfoHybrid { v1, v2 },
                },
                piece_layers,
            },
            (Some(v1), None, None) => TorrentMeta::V1 {
                info: Info {
                    name,
                    piece_length: common_fields.piece_length,
                    private: common_fields.private,
                    source: common_fields.source,
                    kind: v1,
                },
            },
            (None, Some(v2), Some(piece_layers)) => TorrentMeta::V2 {
                info: Info {
                    name,
                    piece_length: common_fields.piece_length,
                    private: common_fields.private,
                    source: common_fields.source,
                    kind: v2,
                },
                piece_layers,
            },
            _ => unreachable!("Invariants violated"),
        };

        Torrent {
            tracker: common_fields.tracker,
            tracker_tiers: common_fields.tracker_tiers,
            web_seeds: common_fields.web_seeds,
            creation_date: common_fields.creation_date,
            comment: common_fields.comment,
            created_by: common_fields.created_by,
            encoding: common_fields.encoding,
            meta,
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(super) struct FileEntry {
        pub(super) disk_path: PathBuf,
        pub(super) meta_path: PathBuf,
        pub(super) length: u64,
        pub(super) padding: bool,
    }

    #[derive(Debug)]
    pub(super) struct CommonFields {
        pub(super) piece_length: Option<NonZeroU64>,
        pub(super) private: bool,
        pub(super) source: Option<String>,
        pub(super) tracker_tiers: Vec<TrackerTier>,
        pub(super) web_seeds: Vec<Url>,
        pub(super) creation_date: Option<u64>,
        pub(super) created_by: Option<String>,
        pub(super) comment: Option<String>,
    }

    #[derive(Debug)]
    pub(super) struct CommonFieldsResolved {
        pub(super) piece_length: NonZeroU64,
        pub(super) private: bool,
        pub(super) source: Option<Cow<'static, str>>,
        pub(super) tracker: Option<Url>,
        pub(super) tracker_tiers: Option<Vec<TrackerTier>>,
        pub(super) web_seeds: Option<Vec<Url>>,
        pub(super) creation_date: Option<u64>,
        pub(super) created_by: Option<Cow<'static, str>>,
        pub(super) comment: Option<Cow<'static, str>>,
        pub(super) encoding: Option<Cow<'static, str>>,
    }

    #[derive(Debug)]
    pub(super) struct FileManager<'a> {
        files: &'a [FileEntry],
        states: Vec<Mutex<FileState>>,
    }

    #[derive(Debug, Default)]
    pub(super) struct FileState {
        uses: usize,
        mmap: Option<Arc<Mmap>>,
    }

    pub(super) struct MmapGuard<'a> {
        mmap: Arc<Mmap>,
        state: &'a Mutex<FileState>,
    }

    impl Deref for MmapGuard<'_> {
        type Target = [u8];
        fn deref(&self) -> &Self::Target {
            &self.mmap
        }
    }

    impl Drop for MmapGuard<'_> {
        fn drop(&mut self) {
            let mut state = self.state.lock().unwrap();
            state.uses -= 1;
            if state.uses == 0 {
                state.mmap = None;
            }
        }
    }

    impl<'a> FileManager<'a> {
        pub fn new(files: &'a [FileEntry]) -> Self {
            let mut states = Vec::with_capacity(files.len());
            for _ in 0..files.len() {
                states.push(Mutex::new(FileState::default()));
            }
            Self { files, states }
        }

        pub fn register_use(&self, file_index: usize) {
            let mut state = self.states[file_index].lock().unwrap();
            state.uses += 1;
        }

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

#[derive(Debug)]
pub struct TorrentBuilder<State> {
    files: Vec<(PathBuf, u64)>,
    name: Option<String>,
    common_fields: CommonFields,
    single_file: bool,
    _state: PhantomData<State>,
}

// ── Methods available in both states ────────────────────────────────────────

impl<T> TorrentBuilder<T> {
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn piece_length(mut self, piece_length: NonZeroU64) -> Self {
        self.common_fields.piece_length = Some(piece_length);
        self
    }

    #[must_use]
    pub fn private(mut self) -> Self {
        self.common_fields.private = true;
        self
    }

    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.common_fields.source = Some(source.into());
        self
    }

    #[must_use]
    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.common_fields.creation_date = Some(creation_date);
        self
    }

    #[must_use]
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        self.common_fields.created_by = Some(created_by.into());
        self
    }

    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.common_fields.comment = Some(comment.into());
        self
    }

    #[must_use]
    pub fn add_tracker(mut self, tracker: Url) -> Self {
        self.last_tracker_tier_mut().0.push(tracker);
        self
    }

    #[must_use]
    pub fn add_trackers<I: IntoIterator<Item = Url>>(mut self, trackers: I) -> Self {
        self.last_tracker_tier_mut().0.extend(trackers);
        self
    }

    #[must_use]
    pub fn next_tracker_tier(mut self) -> Self {
        if !self.last_tracker_tier_mut().is_empty() {
            self.common_fields.tracker_tiers.push(TrackerTier::default());
        }
        self
    }

    #[must_use]
    pub fn add_web_seed(mut self, seed: Url) -> Self {
        self.common_fields.web_seeds.push(seed);
        self
    }

    #[must_use]
    pub fn add_web_seeds<I: IntoIterator<Item = Url>>(mut self, seeds: I) -> Self {
        self.common_fields.web_seeds.extend(seeds);
        self
    }

    fn last_tracker_tier_mut(&mut self) -> &mut TrackerTier {
        if self.common_fields.tracker_tiers.is_empty() {
            self.common_fields.tracker_tiers.push(TrackerTier::default());
        }
        self.common_fields.tracker_tiers.last_mut().unwrap()
    }

    fn add_path_internal(&mut self, path: impl Into<PathBuf>) -> Result<(), Error> {
        let path = path.into();

        match path.metadata()? {
            m if m.is_file() => {
                self.files.push((path, m.len()));
                self.single_file = self.files.len() == 1;
            }
            m if m.is_dir() => {
                let files = WalkDir::new(&path)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                    .map(|e| -> Result<(PathBuf, u64), Error> {
                        let len = e.metadata()?.len();
                        Ok((e.into_path(), len))
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                if files.is_empty() {
                    return Err(Error::NoFiles);
                }

                self.files.extend(files);
                self.single_file = false;
            }
            _ => return Err(Error::UnsupportedFileType(path)),
        }

        Ok(())
    }

    fn into_state<S>(self) -> TorrentBuilder<S> {
        TorrentBuilder {
            files: self.files,
            name: self.name,
            common_fields: self.common_fields,
            single_file: self.single_file,
            _state: PhantomData,
        }
    }
}

// ── Empty state ──────────────────────────────────────────────────────────────

impl Default for TorrentBuilder<state::Empty> {
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentBuilder<state::Empty> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: vec![],
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
            single_file: false,
            _state: PhantomData,
        }
    }

    pub fn add_path(mut self, path: impl Into<PathBuf>) -> Result<TorrentBuilder<HasFiles>, Error> {
        self.add_path_internal(path)?;

        Ok(self.into_state())
    }

    pub fn add_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(
        mut self,
        paths: I,
    ) -> Result<TorrentBuilder<HasFiles>, Error> {
        for path in paths {
            match self.add_path_internal(path) {
                Err(Error::NoFiles) | Ok(()) => (),
                Err(e) => return Err(e),
            }
        }

        if self.files.is_empty() {
            Err(Error::NoFiles)
        } else {
            Ok(self.into_state())
        }
    }
}

// ── HasFiles state ───────────────────────────────────────────────────────────

impl TorrentBuilder<state::HasFiles> {
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, Error> {
        TorrentBuilder::new().add_path(path)
    }

    pub fn from_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(paths: I) -> Result<Self, Error> {
        TorrentBuilder::new().add_paths(paths)
    }

    pub fn add_path(mut self, path: impl Into<PathBuf>) -> Result<Self, Error> {
        match self.add_path_internal(path) {
            Err(Error::NoFiles) | Ok(()) => Ok(self),
            Err(e) => Err(e),
        }
    }

    pub fn add_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(
        mut self,
        paths: I,
    ) -> Result<Self, Error> {
        for path in paths {
            match self.add_path_internal(path) {
                Err(Error::NoFiles) | Ok(()) => (),
                Err(e) => return Err(e),
            }
        }

        Ok(self)
    }

    pub fn build(self) -> Result<TorrentBuf, Error> {
        self.build_hybrid()
    }

    pub fn build_v1(mut self) -> Result<TorrentBuf, Error> {
        let common_fields = common_fields(self.common_fields, &self.files);

        self.files.sort();

        let (common_prefix, files) = remove_common_prefix(&self.files);
        let v1 = v1_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            self.single_file,
        )?;

        let name = resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            None,
            None,
        ))
    }

    pub fn build_v2(mut self) -> Result<TorrentBuf, Error> {
        let common_fields = common_fields(self.common_fields, &self.files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        self.files.sort();

        let (common_prefix, files) = remove_common_prefix(&self.files);

        let (v2, v2_ext) = v2_fields(&files, piece_length_usize(common_fields.piece_length)?)?;

        let name = resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            None,
            Some(v2),
            Some(v2_ext),
        ))
    }

    pub fn build_hybrid(mut self) -> Result<TorrentBuf, Error> {
        let common_fields = common_fields(self.common_fields, &self.files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        self.files.sort();

        let (common_prefix, files) = remove_common_prefix(&self.files);

        let (v1, v2, v2_ext) = hybrid_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            self.single_file,
        )?;

        let name = resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            Some(v2),
            Some(v2_ext),
        ))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Error while walking a directory: {0}")]
    WalkDir(#[from] walkdir::Error),

    #[error("No files were provided to the factory")]
    NoFiles,

    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,

    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),

    #[error("file length {0} is too large for the platform address space")]
    FileTooLarge(u64),

    #[error(
        "Files cannot be processed with the given piece length in this platform address space: {0}"
    )]
    PieceLengthTooSmall(usize),

    #[error("Piece length must be a power of two and at least 16 KiB in BitTorrent v2: {0}")]
    InvalidPieceLengthV2(NonZeroU64),

    #[error("The provided piece length is too large (does not fit in usize): {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
