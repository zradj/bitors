use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read, Repeat, Take},
    marker::PhantomData,
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use memmap2::Mmap;
use path_clean::clean;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use thiserror::Error;
use url::Url;
use walkdir::WalkDir;

use crate::torrent::{
    FileInfo, FileInfoAttr, FileInfoAttrFlags, FileLeaf, FileMode, FileTree, FileTreeNode, Info,
    InfoV1, InfoV1Buf, InfoV2, InfoV2Buf, Torrent, TorrentBuf, TorrentV2Ext, TorrentV2ExtBuf,
    builder::state::HasFiles,
};

pub mod state {

    #[derive(Debug)]
    pub struct Empty;

    #[derive(Debug)]
    pub struct HasFiles;
}

// 16 KiB (per BEP 52)
const V2_BLOCK_SIZE: usize = 16 * 1024;
const V2_BLOCK_SIZE_U64: u64 = V2_BLOCK_SIZE as u64;

fn piece_length_usize(piece_length: NonZeroU64) -> Result<usize, Error> {
    piece_length
        .get()
        .try_into()
        .map_err(|_| Error::PieceLengthTooLarge(piece_length))
}

#[derive(Debug, Clone)]
struct FileEntry {
    disk_path: PathBuf,
    meta_path: PathBuf,
    length: u64,
    padding: bool,
}

#[derive(Debug)]
struct CommonFields {
    piece_length: Option<NonZeroU64>,
    private: bool,
    source: Option<String>,
    announce_list: Vec<Vec<Url>>,
    url_list: Vec<Url>,
    creation_date: Option<u64>,
    created_by: Option<String>,
    comment: Option<String>,
}

#[derive(Debug)]
struct CommonFieldsResolved {
    piece_length: NonZeroU64,
    private: bool,
    source: Option<Cow<'static, str>>,
    announce: Option<Url>,
    announce_list: Option<Vec<Vec<Url>>>,
    url_list: Option<Vec<Url>>,
    creation_date: Option<u64>,
    created_by: Option<Cow<'static, str>>,
    comment: Option<Cow<'static, str>>,
    encoding: Option<Cow<'static, str>>,
}

#[derive(Debug)]
struct V1PieceHashes(Vec<[u8; 20]>);

#[derive(Debug)]
enum V2FileHashes {
    Empty,
    SinglePiece {
        root: [u8; 32],
    },
    MultiPiece {
        root: [u8; 32],
        layer: Vec<[u8; 32]>,
    },
}

#[derive(Debug)]
enum V1Reader {
    Disk(BufReader<File>),
    Padding(Take<Repeat>),
}

impl Read for V1Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Disk(r) => r.read(buf),
            Self::Padding(r) => r.read(buf),
        }
    }
}

#[derive(Debug)]
struct V1ChunkIterator<'a> {
    files: &'a [FileEntry],
    piece_length: usize,
    file_idx: usize,
    reader: Option<V1Reader>,
    piece_idx: usize,
}

impl<'a> V1ChunkIterator<'a> {
    fn new(files: &'a [FileEntry], piece_length: usize) -> Self {
        Self {
            files,
            piece_length,
            file_idx: 0,
            reader: None,
            piece_idx: 0,
        }
    }

    fn open_reader(&self, idx: usize) -> Result<V1Reader, Error> {
        let entry = &self.files[idx];
        if entry.padding {
            Ok(V1Reader::Padding(io::repeat(0).take(entry.length)))
        } else {
            let f = File::open(&entry.disk_path)?;
            Ok(V1Reader::Disk(BufReader::with_capacity(
                self.piece_length,
                f,
            )))
        }
    }
}

impl Iterator for V1ChunkIterator<'_> {
    type Item = Result<(usize, Vec<u8>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.reader.is_none() {
            if self.file_idx >= self.files.len() {
                return None;
            }
            match self.open_reader(self.file_idx) {
                Ok(r) => self.reader = Some(r),
                Err(e) => return Some(Err(e)),
            }
        }

        let mut chunk = vec![0u8; self.piece_length];
        let mut total = 0;

        loop {
            let reader_ref = self.reader.as_mut().unwrap();
            match reader_ref.read(&mut chunk[total..]) {
                Ok(0) => {
                    self.file_idx += 1;
                    if self.file_idx >= self.files.len() {
                        self.reader = None;
                        break;
                    }
                    match self.open_reader(self.file_idx) {
                        Ok(r) => self.reader = Some(r),
                        Err(e) => return Some(Err(e)),
                    }
                }
                Ok(n) => {
                    total += n;
                    if total == self.piece_length {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Some(Err(e.into())),
            }
        }

        if total == 0 {
            return None;
        }

        chunk.truncate(total);
        let current_idx = self.piece_idx;
        self.piece_idx += 1;

        Some(Ok((current_idx, chunk)))
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
    pub fn add_announce_url(mut self, announce_url: Url) -> Self {
        self.last_announce_tier_mut().push(announce_url);
        self
    }

    #[must_use]
    pub fn add_announce_urls<I: IntoIterator<Item = Url>>(mut self, announce_urls: I) -> Self {
        self.last_announce_tier_mut().extend(announce_urls);
        self
    }

    #[must_use]
    pub fn next_announce_tier(mut self) -> Self {
        if !self.last_announce_tier_mut().is_empty() {
            self.common_fields.announce_list.push(vec![]);
        }
        self
    }

    #[must_use]
    pub fn add_url(mut self, url: Url) -> Self {
        self.common_fields.url_list.push(url);
        self
    }

    #[must_use]
    pub fn add_urls<I: IntoIterator<Item = Url>>(mut self, urls: I) -> Self {
        self.common_fields.url_list.extend(urls);
        self
    }

    fn last_announce_tier_mut(&mut self) -> &mut Vec<Url> {
        if self.common_fields.announce_list.is_empty() {
            self.common_fields.announce_list.push(vec![]);
        }
        self.common_fields.announce_list.last_mut().unwrap()
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
                    .map(walkdir::DirEntry::into_path)
                    .map(|p| -> Result<(PathBuf, u64), Error> {
                        let len = p.metadata()?.len();
                        Ok((p, len))
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
                announce_list: vec![],
                url_list: vec![],
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
        let common_fields = Self::build_common_fields(self.common_fields, &self.files);

        self.files.sort();

        let (common_prefix, files) = Self::remove_common_prefix(&self.files);
        let v1 = Self::build_v1_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            self.single_file,
        )?;

        let name = Self::resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(Self::torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            None,
            None,
        ))
    }

    pub fn build_v2(mut self) -> Result<TorrentBuf, Error> {
        let common_fields = Self::build_common_fields(self.common_fields, &self.files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        self.files.sort();

        let (common_prefix, files) = Self::remove_common_prefix(&self.files);

        let (v2, v2_ext) =
            Self::build_v2_fields(&files, piece_length_usize(common_fields.piece_length)?)?;

        let name = Self::resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(Self::torrent_from_parts(
            name,
            common_fields,
            None,
            Some(v2),
            Some(v2_ext),
        ))
    }

    pub fn build_hybrid(mut self) -> Result<TorrentBuf, Error> {
        let common_fields = Self::build_common_fields(self.common_fields, &self.files);

        if !common_fields.piece_length.is_power_of_two()
            || common_fields.piece_length.get() < 16 * 1024
        {
            return Err(Error::InvalidPieceLengthV2(common_fields.piece_length));
        }

        self.files.sort();

        let (common_prefix, files) = Self::remove_common_prefix(&self.files);

        let (v1, v2, v2_ext) = Self::build_hybrid_fields(
            &files,
            piece_length_usize(common_fields.piece_length)?,
            self.single_file,
        )?;

        let name = Self::resolve_name(self.name, &files, self.single_file, &common_prefix)?;

        Ok(Self::torrent_from_parts(
            name,
            common_fields,
            Some(v1),
            Some(v2),
            Some(v2_ext),
        ))
    }

    fn build_v1_fields(
        files: &[FileEntry],
        piece_length: usize,
        single_file: bool,
    ) -> Result<InfoV1Buf, Error> {
        let file_infos = Self::build_v1_file_infos(files)?;

        let piece_hashes = Self::compute_v1_piece_hashes(files, piece_length)?;

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

    fn build_v2_fields(
        files: &[FileEntry],
        piece_length: usize,
    ) -> Result<(InfoV2Buf, TorrentV2ExtBuf), Error> {
        let hashes_list = files
            .par_iter()
            .map(|f| Self::compute_v2_file_hashes(f, piece_length))
            .collect::<Result<Vec<_>, _>>()?;
        let (file_tree, piece_layers_entries) =
            Self::build_v2_file_tree_and_piece_layers(files, hashes_list);

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
        let v2_ext = TorrentV2Ext { piece_layers };

        Ok((
            InfoV2 {
                meta_version: 2,
                file_tree,
            },
            v2_ext,
        ))
    }

    fn build_hybrid_fields(
        files: &[FileEntry],
        piece_length: usize,
        single_file: bool,
    ) -> Result<(InfoV1Buf, InfoV2Buf, TorrentV2ExtBuf), Error> {
        let v2_hashes_list = files
            .par_iter()
            .map(|file| Self::compute_v2_file_hashes(file, piece_length))
            .collect::<Result<Vec<_>, _>>()?;

        let mut files_pad = Vec::with_capacity(files.len());
        for file in files {
            let pad_len = file.length.next_multiple_of(piece_length as u64) - file.length;
            files_pad.push(file.clone());
            if pad_len > 0 {
                files_pad.push(FileEntry {
                    disk_path: PathBuf::new(),
                    meta_path: PathBuf::from(format!(".pad/{pad_len}")),
                    length: pad_len,
                    padding: true,
                });
            }
        }

        let v1_piece_hashes = Self::compute_v1_piece_hashes(&files_pad, piece_length)?;

        let file_infos = Self::build_v1_file_infos(&files_pad)?;

        let file_mode = match (file_infos.len(), single_file) {
            (0, _) => unreachable!("TorrentFactory<HasFiles> does not allow an empty file vector"),
            (1, true) => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
        };

        let (file_tree, piece_layers_entries) =
            Self::build_v2_file_tree_and_piece_layers(files, v2_hashes_list);

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
        let v2_ext = TorrentV2Ext { piece_layers };

        Ok((
            InfoV1 {
                pieces: Cow::Owned(v1_piece_hashes.0),
                file_mode,
            },
            InfoV2 {
                meta_version: 2,
                file_tree,
            },
            v2_ext,
        ))
    }

    fn resolve_name(
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

    fn build_common_fields(
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

        let announce = common_fields
            .announce_list
            .first()
            .and_then(|tier| tier.first().cloned());

        let announce_list = common_fields
            .announce_list
            .into_iter()
            .filter(|tier| !tier.is_empty())
            .collect::<Vec<_>>();

        let announce_list = if announce_list.is_empty() {
            None
        } else {
            Some(announce_list)
        };

        let url_list = if common_fields.url_list.is_empty() {
            None
        } else {
            Some(common_fields.url_list)
        };

        CommonFieldsResolved {
            piece_length,
            private: common_fields.private,
            source: common_fields.source.map(Cow::Owned),
            announce,
            announce_list,
            url_list,
            creation_date: Some(creation_date),
            created_by: common_fields.created_by.map(Cow::Owned),
            comment: common_fields.comment.map(Cow::Owned),
            encoding: Some(Cow::Borrowed("UTF-8")),
        }
    }

    fn build_v1_file_infos(files: &[FileEntry]) -> Result<Vec<FileInfo<'static>>, Error> {
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

    fn build_v2_file_tree_and_piece_layers(
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

            let (pieces_root, maybe_layer) = match file_hashes {
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

            if let Some(layer_entry) = maybe_layer {
                piece_layers.push(layer_entry);
            }
        }

        (file_tree, piece_layers)
    }

    fn compute_v1_piece_hashes(
        files: &[FileEntry],
        piece_length: usize,
    ) -> Result<V1PieceHashes, Error> {
        let chunk_iter = V1ChunkIterator::new(files, piece_length);

        let mut indexed_hashes = chunk_iter
            .par_bridge()
            .map(|res| {
                res.map(|(index, chunk)| {
                    let mut sha1 = Sha1::new();
                    sha1.update(&chunk);
                    (index, sha1.finalize().into())
                })
            })
            .collect::<Result<Vec<(usize, [u8; 20])>, _>>()?;

        indexed_hashes.sort_by_key(|(index, _)| *index);

        let hashes = indexed_hashes.into_iter().map(|(_, h)| h).collect();
        Ok(V1PieceHashes(hashes))
    }

    fn compute_v2_file_hashes(
        file: &FileEntry,
        piece_length: usize,
    ) -> Result<V2FileHashes, Error> {
        debug_assert!(piece_length.is_power_of_two());
        debug_assert!(piece_length >= V2_BLOCK_SIZE);

        if file.length == 0 {
            return Ok(V2FileHashes::Empty);
        }

        let padded_length = file.length.max(V2_BLOCK_SIZE_U64).next_power_of_two();
        let reader = File::open(&file.disk_path)?;

        let mmap = unsafe { Mmap::map(&reader)? };

        let leaves = mmap
            .par_chunks(V2_BLOCK_SIZE)
            .map_init(Sha256::new, |sha256, chunk| {
                sha256.update(chunk);
                sha256.finalize_reset().into()
            })
            .collect();

        Ok(Self::compute_v2_merkle_tree(
            leaves,
            file.length,
            padded_length,
            piece_length,
        ))
    }

    fn compute_v2_merkle_tree(
        mut leaves: Vec<[u8; 32]>,
        file_length: u64,
        padded_length: u64,
        piece_length: usize,
    ) -> V2FileHashes {
        let mut sha256 = Sha256::new();
        let num_leaves = (padded_length / V2_BLOCK_SIZE_U64) as usize;
        let target_depth = (piece_length / V2_BLOCK_SIZE).ilog2();
        let mut piece_layer = None;

        // Zero-hash leaves to balance the tree (per BEP 52)
        leaves.resize(num_leaves, [0u8; 32]);

        let mut layer_width = num_leaves / 2;
        let mut depth = 0;

        let mut prev_layer = leaves;
        while layer_width > 0 {
            if depth == target_depth && file_length > (piece_length as u64) {
                let num_real_pieces = usize::try_from(file_length.div_ceil(piece_length as u64))
                    .expect("32-bit targets cannot open files this big");
                piece_layer = Some(prev_layer[..num_real_pieces].to_vec());
            }

            let mut layer = Vec::with_capacity(layer_width);

            for chunk in prev_layer.chunks_exact(2) {
                sha256.update(chunk[0]);
                sha256.update(chunk[1]);
                layer.push(sha256.finalize_reset().into());
            }

            depth += 1;
            prev_layer = layer;
            layer_width /= 2;
        }

        let [root_hash] = prev_layer.as_slice() else {
            unreachable!("Merkle tree reduction must yield exactly one root");
        };

        match (root_hash, piece_layer) {
            (&root, None) => V2FileHashes::SinglePiece { root },
            (&root, Some(layer)) => V2FileHashes::MultiPiece { root, layer },
        }
    }

    fn remove_common_prefix(paths: &[(PathBuf, u64)]) -> (PathBuf, Vec<FileEntry>) {
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

    fn torrent_from_parts(
        name: Cow<'static, str>,
        common_fields: CommonFieldsResolved,
        v1: Option<InfoV1Buf>,
        v2: Option<InfoV2Buf>,
        v2_ext: Option<TorrentV2ExtBuf>,
    ) -> TorrentBuf {
        let info = Info {
            name,
            piece_length: common_fields.piece_length,
            private: common_fields.private,
            source: common_fields.source,
            v1,
            v2,
        };

        Torrent {
            info,
            announce: common_fields.announce,
            announce_list: common_fields.announce_list,
            url_list: common_fields.url_list,
            creation_date: common_fields.creation_date,
            comment: common_fields.comment,
            created_by: common_fields.created_by,
            encoding: common_fields.encoding,
            v2_ext,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("No files were provided to the factory")]
    NoFiles,

    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,

    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),

    #[error("Piece length must be a power of two and at least 16 KiB in BitTorrent v2: {0}")]
    InvalidPieceLengthV2(NonZeroU64),

    #[error("The provided piece length is too large (does not fit in usize): {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
