//! Torrent file creation via a type-safe builder.
//!
//! The central type is [`TorrentFactory`], a builder that enforces — at compile time — that
//! at least one source file has been provided before [`build`](TorrentFactory::build) may be
//! called.  This is achieved through the *typestate* pattern: the factory's type parameter
//! is either [`state::Empty`] or [`state::HasFiles`], and the `build` method is only
//! implemented for the latter.
//!
//! # Workflow
//!
//! 1. Start with [`TorrentFactory::new`] (or [`TorrentFactory::default`]).
//! 2. Optionally configure metadata (name, piece length, announce URLs, …).
//! 3. Supply at least one source via [`add_path`](TorrentFactory::add_path) or
//!    [`add_paths`](TorrentFactory::add_paths), or use the convenience constructors
//!    [`TorrentFactory::from_path`] and [`TorrentFactory::from_paths`].
//! 4. Call [`build`](TorrentFactory::build) to produce the serialisable [`TorrentBuf`].
//!
//! # Examples
//!
//! ## Single-file torrent
//!
//! ```no_run
//! use std::num::NonZeroU64;
//! use url::Url;
//! use bitors::torrent::factory::{TorrentFactory, Error};
//!
//! fn main() -> Result<(), Error> {
//!     let torrent = TorrentFactory::new()
//!         .name("my-release")
//!         .piece_length(NonZeroU64::new(512 * 1024).unwrap())
//!         .add_announce_url(Url::parse("udp://tracker.example.com:6969/announce").unwrap())
//!         .add_path("path/to/file.iso")?
//!         .build()?;
//!
//!     // Do other stuff
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Directory torrent
//!
//! ```no_run
//! use bitors::torrent::factory::{TorrentFactory, Error};
//!
//! fn main() -> Result<(), Error> {
//!     let torrent = TorrentFactory::from_path("path/to/my-album/")?
//!         .private()
//!         .build()?;
//!
//!     // Do other stuff
//!
//!     Ok(())
//! }
//! ```

use std::{
    borrow::Cow,
    fs::File,
    io::{BufReader, Read},
    marker::PhantomData,
    num::NonZeroU64,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use path_clean::clean;
use sha1::{Digest, Sha1};
use thiserror::Error;
use url::Url;
use walkdir::WalkDir;

use crate::torrent::{FileInfo, FileMode, Info, Torrent, TorrentBuf, factory::state::HasFiles};

/// Typestate markers that encode whether the factory has been given source files yet.
///
/// These types are only ever used as the type parameter `State` of [`TorrentFactory`]; they
/// carry no data and exist solely to shift validation from runtime to compile time.
pub mod state {
    /// The factory has not yet been given any source files.
    ///
    /// In this state only the metadata-setting builder methods are available; calling
    /// [`build`](super::TorrentFactory::build) would be a compile error.
    #[derive(Debug)]
    pub struct Empty;

    /// The factory has been given at least one source file and is ready to build.
    #[derive(Debug)]
    pub struct HasFiles;
}

/// Converts a [`NonZeroU64`] piece length to `usize`, returning an error on overflow.
///
/// On 32-bit targets a piece length larger than `usize::MAX` cannot be used as a buffer
/// size.  This helper makes that failure explicit rather than silently truncating.
fn piece_length_usize(piece_length: NonZeroU64) -> Result<usize, Error> {
    piece_length
        .get()
        .try_into()
        .map_err(|_| Error::PieceLengthTooLarge(piece_length))
}

/// A builder for creating `.torrent` files.
///
/// `TorrentFactory` uses the *typestate* pattern: the type parameter `State` is either
/// [`state::Empty`] or [`state::HasFiles`].  Methods that require at least one source file
/// (currently only [`build`](TorrentFactory::build)) are restricted to
/// `TorrentFactory<state::HasFiles>`, making it impossible to call them before files have
/// been provided.
///
/// All builder methods consume `self` and return a new `TorrentFactory`, allowing method
/// calls to be chained fluently.  Methods that are valid in *either* state are implemented
/// on `TorrentFactory<T>` (generic over `T`); methods only valid after files have been added
/// are implemented on `TorrentFactory<state::HasFiles>`.
///
/// # Default values
///
/// | Field           | Default                                                                  |
/// |-----------------|--------------------------------------------------------------------------|
/// | `piece_length`  | 512 KiB (`512 * 1024` bytes)                                             |
/// | `creation_date` | Current UNIX timestamp                                                   |
/// | `name`          | Single-file: the filename. Multi-file: last component of the common path prefix, or `"New Torrent"` if no common prefix exists. |
/// | `private`       | `false`                                                                  |
/// | `announce_list` | Empty (no trackers)                                                      |
#[derive(Debug)]
pub struct TorrentFactory<State> {
    files: Vec<(PathBuf, u64)>,
    name: Option<String>,
    piece_length: Option<NonZeroU64>,
    private: bool,
    source: Option<String>,
    announce_list: Vec<Vec<Url>>,
    url_list: Vec<Url>,
    creation_date: Option<u64>,
    created_by: Option<String>,
    comment: Option<String>,
    _state: PhantomData<State>,
}

// ── Methods available in both states ────────────────────────────────────────

impl<T> TorrentFactory<T> {
    /// Sets the torrent's display name.
    ///
    /// If not set, the name is derived automatically: for a single-file torrent it is
    /// the filename; for a multi-file torrent it is the last component of the common
    /// path prefix shared by all source files, or `"New Torrent"` if no common prefix
    /// exists.  See [`from_path`](TorrentFactory::from_path) and
    /// [`from_paths`](TorrentFactory::from_paths) for details.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the piece length in bytes.
    ///
    /// Every piece (except possibly the last) will be hashed as a contiguous block of this
    /// size.  Larger values reduce the number of hashes stored in the `.torrent` file but
    /// increase the granularity of download verification.  Common values are powers of two
    /// between 256 KiB and 16 MiB.
    ///
    /// Defaults to 512 KiB if not set.
    #[must_use]
    pub fn piece_length(mut self, piece_length: NonZeroU64) -> Self {
        self.piece_length = Some(piece_length);
        self
    }

    /// Marks the torrent as *private* (sets the `private = 1` flag in the info dictionary).
    ///
    /// Private torrents must only be used with the trackers listed in the `.torrent` file;
    /// peer exchange (PEX) and DHT are disabled by BitTorrent clients that respect this flag.
    #[must_use]
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    /// Sets the `source` field in the `info` dictionary.
    ///
    /// This field is commonly used by private trackers to tag their copies of a torrent.
    /// Because `source` is part of the `info` dictionary, changing it alters the info hash,
    /// making the resulting torrent distinct from copies without the tag (or with a
    /// different tag).
    ///
    /// If not set, the field is omitted from the serialized output.
    #[must_use]
    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Sets the torrent creation timestamp as seconds since the UNIX epoch.
    ///
    /// Defaults to the current system time if not set.
    #[must_use]
    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.creation_date = Some(creation_date);
        self
    }

    /// Sets the `created by` field, typically the name and version of the creating program.
    #[must_use]
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        self.created_by = Some(created_by.into());
        self
    }

    /// Sets an arbitrary human-readable comment stored in the torrent metadata.
    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Appends a tracker URL to the *current* announce tier.
    ///
    /// All URLs added without an intervening call to [`next_announce_tier`] are placed in
    /// the same tier.  Within a tier, clients may try URLs in random order and move to the
    /// next tier only when all URLs in the current tier have been exhausted.
    ///
    /// The first URL of the first tier is also stored as the top-level `announce` field for
    /// compatibility with older clients that do not support `announce-list`.
    ///
    /// [`next_announce_tier`]: TorrentFactory::next_announce_tier
    #[must_use]
    pub fn add_announce_url(mut self, announce_url: Url) -> Self {
        self.get_last_announce_tier().push(announce_url);
        self
    }

    /// Appends multiple tracker URLs to the *current* announce tier.
    ///
    /// Equivalent to calling [`add_announce_url`](TorrentFactory::add_announce_url)
    /// repeatedly, but consumes an iterator instead of a single URL.
    #[must_use]
    pub fn add_announce_urls<I: IntoIterator<Item = Url>>(mut self, announce_urls: I) -> Self {
        self.get_last_announce_tier().extend(announce_urls);
        self
    }

    /// Begins a new announce tier.
    ///
    /// Subsequent calls to [`add_announce_url`](TorrentFactory::add_announce_url) /
    /// [`add_announce_urls`](TorrentFactory::add_announce_urls) will add URLs to this new
    /// tier rather than the previous one.  If the current tier is already empty this method
    /// is a no-op (empty tiers are not created).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use url::Url; use bitors::torrent::factory::TorrentFactory;
    /// let factory = TorrentFactory::new()
    ///     // Tier 0 — primary trackers
    ///     .add_announce_url(Url::parse("udp://primary.example.com:6969/announce").unwrap())
    ///     .next_announce_tier()
    ///     // Tier 1 — backup trackers
    ///     .add_announce_url(Url::parse("udp://backup.example.com:6969/announce").unwrap());
    /// ```
    #[must_use]
    pub fn next_announce_tier(mut self) -> Self {
        if !self.get_last_announce_tier().is_empty() {
            self.announce_list.push(vec![]);
        }
        self
    }

    /// Appends a single web-seed URL to the `url-list` field ([BEP 19]).
    ///
    /// Web seeds let clients fall back to HTTP/HTTPS downloads when peers are unavailable.
    /// Each URL should point directly to the content described by the torrent.
    ///
    /// [BEP 19]: https://www.bittorrent.org/beps/bep_0019.html
    #[must_use]
    pub fn add_url(mut self, url: Url) -> Self {
        self.url_list.push(url);
        self
    }

    /// Appends multiple web-seed URLs to the `url-list` field ([BEP 19]).
    ///
    /// Equivalent to calling [`add_url`](TorrentFactory::add_url) repeatedly, but
    /// consumes an iterator instead of a single URL.
    ///
    /// [BEP 19]: https://www.bittorrent.org/beps/bep_0019.html
    #[must_use]
    pub fn add_urls<I: IntoIterator<Item = Url>>(mut self, urls: I) -> Self {
        self.url_list.extend(urls);
        self
    }

    /// Returns a mutable reference to the last announce tier, creating one if necessary.
    fn get_last_announce_tier(&mut self) -> &mut Vec<Url> {
        if self.announce_list.is_empty() {
            self.announce_list.push(vec![]);
        }
        self.announce_list.last_mut().unwrap()
    }

    fn add_path_internal(&mut self, path: impl Into<PathBuf>) -> Result<(), Error> {
        let path = path.into();

        match path.metadata()? {
            m if m.is_file() => self.files.push((path, m.len())),
            m if m.is_dir() => {
                let files = WalkDir::new(&path)
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
            }
            _ => return Err(Error::UnsupportedFileType(path)),
        }

        Ok(())
    }

    fn into_state<S>(self) -> TorrentFactory<S> {
        TorrentFactory {
            files: self.files,
            name: self.name,
            piece_length: self.piece_length,
            private: self.private,
            source: self.source,
            announce_list: self.announce_list,
            url_list: self.url_list,
            creation_date: self.creation_date,
            created_by: self.created_by,
            comment: self.comment,
            _state: PhantomData,
        }
    }
}

// ── Empty state ──────────────────────────────────────────────────────────────

impl Default for TorrentFactory<state::Empty> {
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentFactory<state::Empty> {
    /// Creates a new, unconfigured factory.
    ///
    /// All fields use their defaults (see the [struct-level documentation](TorrentFactory)
    /// for the default values).  At least one source path must be provided — via
    /// [`add_path`](TorrentFactory::add_path) or [`add_paths`](TorrentFactory::add_paths) —
    /// before [`build`](TorrentFactory::build) can be called.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: vec![],
            name: None,
            piece_length: None,
            private: false,
            source: None,
            announce_list: vec![],
            url_list: vec![],
            creation_date: None,
            created_by: None,
            comment: None,
            _state: PhantomData,
        }
    }

    /// Adds a single file or directory, transitioning the factory to [`state::HasFiles`].
    ///
    /// If `path` points to a directory it is walked recursively; all regular files found
    /// are added.  The factory transitions to [`state::HasFiles`] on success, enabling
    /// [`build`](TorrentFactory::build) to be called.
    ///
    /// Files are not sorted here; sorting happens inside [`build`](TorrentFactory::build)
    /// to guarantee deterministic piece hashes regardless of the order in which paths
    /// were supplied.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoFiles`] if `path` is a directory that contains no regular files.
    /// Returns [`Error::UnsupportedFileType`] if `path` is neither a file nor a directory
    /// (e.g. a socket or a broken symlink).
    /// Returns [`Error::Io`] on any I/O failure (including permission errors).
    pub fn add_path(mut self, path: impl Into<PathBuf>) -> Result<TorrentFactory<HasFiles>, Error> {
        self.add_path_internal(path)?;

        Ok(self.into_state())
    }

    /// Adds multiple files and/or directories, transitioning the factory to [`state::HasFiles`].
    ///
    /// Each element of `paths` is processed by the same rules as
    /// [`add_path`](TorrentFactory::add_path): files are added directly, directories are
    /// walked recursively.  Empty directories within the iterator are silently skipped;
    /// the iterator as a whole must resolve to at least one regular file for the transition
    /// to [`state::HasFiles`] to succeed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoFiles`] if no regular files were found across all supplied paths.
    /// Returns [`Error::UnsupportedFileType`] if any path is neither a file nor a directory.
    /// Returns [`Error::Io`] on any I/O failure.
    pub fn add_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(
        mut self,
        paths: I,
    ) -> Result<TorrentFactory<HasFiles>, Error> {
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

impl TorrentFactory<state::HasFiles> {
    /// Creates a factory pre-loaded with a single file or directory.
    ///
    /// Convenience shorthand for `TorrentFactory::new().add_path(path)`.
    /// If `path` is a directory it is walked recursively.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`TorrentFactory::add_path`].
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, Error> {
        TorrentFactory::new().add_path(path)
    }

    /// Creates a factory pre-loaded with multiple files and/or directories.
    ///
    /// Convenience shorthand for `TorrentFactory::new().add_paths(paths)`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`TorrentFactory::add_paths`].
    pub fn from_paths<I: IntoIterator<Item = impl Into<PathBuf>>>(paths: I) -> Result<Self, Error> {
        TorrentFactory::new().add_paths(paths)
    }

    /// Adds a single file or directory to an already-populated factory.
    ///
    /// Unlike [`TorrentFactory::<state::Empty>::add_path`], this method returns `Self`
    /// rather than transitioning state (the factory is already in [`state::HasFiles`]).
    /// Empty directories are silently ignored rather than returning [`Error::NoFiles`],
    /// since the factory already holds at least one file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedFileType`] if `path` is neither a file nor a directory.
    /// Returns [`Error::Io`] on any I/O failure.
    pub fn add_path(mut self, path: impl Into<PathBuf>) -> Result<Self, Error> {
        match self.add_path_internal(path) {
            Err(Error::NoFiles) | Ok(()) => Ok(self),
            Err(e) => Err(e),
        }
    }

    /// Adds multiple files and/or directories to an already-populated factory.
    ///
    /// Each element is processed by the same rules as
    /// [`add_path`](TorrentFactory::<state::HasFiles>::add_path). Empty directories
    /// are silently skipped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedFileType`] if any path is neither a file nor a directory.
    /// Returns [`Error::Io`] on any I/O failure.
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

    /// Reads all source files, computes piece hashes, and assembles the final [`TorrentBuf`].
    ///
    /// The following steps happen in order:
    ///
    /// 1. Source files are sorted lexicographically by path so that piece hashes are
    ///    reproducible across runs regardless of the order files were added.
    /// 2. The longest common path prefix is stripped from all paths to produce relative
    ///    `path` components for the `info.files` list.
    /// 3. If no `name` was set explicitly: single-file torrents use the filename;
    ///    multi-file torrents use the last component of the common prefix, or
    ///    `"New Torrent"` if no common prefix exists.
    /// 4. File contents are read sequentially and hashed into SHA-1 pieces of
    ///    `piece_length` bytes.  A piece may span the boundary between two files.
    /// 5. The `TorrentBuf` is assembled and returned.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if any file cannot be opened or read.
    /// Returns [`Error::NonUtf8Name`] if any path component is not valid UTF-8.
    /// Returns [`Error::PieceLengthTooLarge`] if the piece length exceeds `usize::MAX`
    /// (can only occur on 32-bit targets).
    pub fn build(mut self) -> Result<TorrentBuf, Error> {
        let piece_length = self.piece_length.unwrap_or_else(
            #[expect(clippy::missing_panics_doc, reason = "infallible")]
            || NonZeroU64::new(512 * 1024).unwrap(),
        );

        let creation_date = self.creation_date.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

        self.files.sort();

        let (common_prefix, files_no_prefix) = Self::remove_common_prefix(&self.files);

        let mut file_infos = vec![];
        Self::build_file_infos(&self.files, &files_no_prefix, &mut file_infos)?;

        let pieces = Self::compute_piece_hashes(&self.files, piece_length_usize(piece_length)?)?;

        let file_mode = match file_infos.len() {
            0 => unreachable!("TorrentFactory<HasFiles> does not allow an empty file vector"),
            1 => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
        };

        let name = match (self.name, &file_mode) {
            (Some(name), _) => name,
            (None, FileMode::Single { .. }) => common_prefix
                .components()
                .next_back()
                .and_then(|c| c.as_os_str().to_str())
                .ok_or(Error::NonUtf8Name)?
                .to_string(),
            (None, FileMode::Multi { .. }) => {
                if !clean(&common_prefix).starts_with("..")
                    && let Ok(absolute_prefix) = common_prefix.canonicalize()
                    && let Some(last) = absolute_prefix.components().next_back()
                {
                    last.as_os_str()
                        .to_str()
                        .ok_or(Error::NonUtf8Name)?
                        .to_string()
                } else {
                    "New Torrent".to_string()
                }
            }
        };

        let info = Info {
            name: Cow::Owned(name),
            piece_length,
            pieces: Cow::Owned(pieces),
            private: self.private,
            source: self.source.map(Cow::Owned),
            file_mode,
        };

        let announce = self
            .announce_list
            .first()
            .and_then(|tier| tier.first().cloned());

        let announce_list = self
            .announce_list
            .into_iter()
            .filter(|tier| !tier.is_empty())
            .collect::<Vec<_>>();

        let announce_list = if announce_list.is_empty() {
            None
        } else {
            Some(announce_list)
        };

        let url_list = if self.url_list.is_empty() {
            None
        } else {
            Some(self.url_list)
        };

        Ok(Torrent {
            info,
            announce,
            announce_list,
            url_list,
            creation_date: Some(creation_date),
            comment: self.comment.map(Cow::Owned),
            created_by: self.created_by.map(Cow::Owned),
            encoding: Some(Cow::Borrowed("UTF-8")),
        })
    }

    fn build_file_infos(
        files: &[(PathBuf, u64)],
        files_no_prefix: &[PathBuf],
        file_infos: &mut Vec<FileInfo<'_>>,
    ) -> Result<(), Error> {
        let file_path_comps = files_no_prefix
            .iter()
            .map(|p| -> Result<Vec<String>, Error> {
                p.components()
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
            .map(|((_, length), comps)| -> Result<FileInfo, Error> {
                Ok(FileInfo {
                    length: *length,
                    md5sum: None,
                    path: comps.into_iter().map(Cow::Owned).collect(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        file_infos.extend(res);

        Ok(())
    }

    /// Reads all source files sequentially and produces a flat list of SHA-1 piece hashes.
    ///
    /// Files are treated as a single contiguous byte stream: a piece may span the boundary
    /// between two files.  The final piece is hashed over however many bytes remain, which
    /// may be fewer than `piece_length`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if any file cannot be opened or read.
    fn compute_piece_hashes(
        paths: &[(PathBuf, u64)],
        piece_length: usize,
    ) -> Result<Vec<[u8; 20]>, Error> {
        let mut sha1 = Sha1::new();
        let mut hashes = vec![];
        let mut chunk = vec![0u8; piece_length];
        let mut iter = paths
            .iter()
            .map(|(p, _)| File::open(p).map(|f| BufReader::with_capacity(piece_length, f)));
        let mut reader = iter
            .next()
            .expect("TorrentFactory<HasFiles> guarantees at least one file")?;

        loop {
            let mut total = 0;

            while total < piece_length {
                match reader.read(&mut chunk[total..])? {
                    0 => {
                        // Current file exhausted — advance to the next one and keep filling
                        // the current chunk.  If there are no more files, stop filling.
                        if let Some(r) = iter.next() {
                            reader = r?;
                            continue;
                        }

                        break;
                    }
                    n => total += n,
                }
            }

            if total == 0 {
                break;
            }

            sha1.update(&chunk[..total]);
            hashes.push(sha1.finalize_reset().into());
        }

        Ok(hashes)
    }

    /// Strips the longest common path prefix from a slice of paths.
    ///
    /// This is used to convert absolute (or relative) source paths into the per-file
    /// `path` components stored in the torrent's `info.files` list, so that paths are
    /// relative rather than absolute inside the `.torrent` metadata.
    ///
    /// If no common prefix exists the paths are returned unchanged.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `paths` is empty.
    fn remove_common_prefix(paths: &[(PathBuf, u64)]) -> (PathBuf, Vec<PathBuf>) {
        debug_assert!(!paths.is_empty());

        let mut prefix = paths[0].0.clone();

        for (s, _) in &paths[1..] {
            while !s.starts_with(&prefix) {
                if prefix.parent().is_none() {
                    break;
                }
                prefix.pop();
            }
        }

        let paths_no_prefix = paths
            .iter()
            .map(|(p, _)| clean(p.strip_prefix(&prefix).unwrap_or(p)))
            .collect();

        if prefix.as_os_str().is_empty() {
            (prefix, paths_no_prefix)
        } else {
            (clean(prefix), paths_no_prefix)
        }
    }
}

/// Errors that can be returned by [`TorrentFactory`].
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying I/O error occurred while reading a file or querying its metadata.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// No regular files were found across all supplied paths.
    ///
    /// This error is returned by [`TorrentFactory::add_paths`] when none of the
    /// supplied paths resolve to a regular file (e.g. all paths are empty
    /// directories), and by [`TorrentFactory::add_path`] (in the [`state::Empty`]
    /// state) when the supplied path is a directory that contains no regular files.
    #[error("No files were provided to the factory")]
    NoFiles,

    /// A file or directory name cannot be represented as UTF-8.
    ///
    /// The BitTorrent specification requires all names and path components in the `info`
    /// dictionary to be UTF-8 strings.
    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,

    /// The path is neither a regular file nor a directory.
    ///
    /// Returned when a supplied path exists on the filesystem but has an unsupported
    /// type, such as a Unix socket, named pipe (FIFO), device node, or a broken
    /// symlink.  Only regular files and directories are accepted as source paths.
    #[error("Unsupported file type: {0}")]
    UnsupportedFileType(PathBuf),

    /// The requested piece length does not fit in `usize` and cannot be used as a buffer
    /// size on this platform. This can only occur on 32-bit targets.
    #[error("The provided piece length is too large (does not fit in usize): {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
