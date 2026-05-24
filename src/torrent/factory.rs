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
//! 3. Supply at least one source via [`add_file`](TorrentFactory::add_file),
//!    [`add_files`](TorrentFactory::add_files), or the convenience constructors
//!    [`TorrentFactory::from_file`], [`TorrentFactory::from_files`], or
//!    [`TorrentFactory::from_directory`].
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
//!         .add_announce(Url::parse("udp://tracker.example.com:6969/announce").unwrap())
//!         .add_file("path/to/file.iso")?
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
//!     let torrent = TorrentFactory::from_directory("path/to/my-album/")?
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

use sha1::{Digest, Sha1};
use thiserror::Error;
use url::Url;
use walkdir::WalkDir;

use crate::torrent::{FileInfo, FileMode, Info, Torrent, TorrentBuf};

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
/// | Field           | Default                                     |
/// |-----------------|---------------------------------------------|
/// | `piece_length`  | 512 KiB (`512 * 1024` bytes)                |
/// | `creation_date` | Current UNIX timestamp                      |
/// | `name`          | File name of the first source file          |
/// | `private`       | `false`                                     |
/// | `announce_list` | Empty (no trackers)                         |
#[derive(Debug)]
pub struct TorrentFactory<State> {
    files: Vec<PathBuf>,
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
    /// If not set, the name defaults to the file name of the first source file (single-file
    /// mode) or the directory name (when constructed via [`from_directory`]).
    ///
    /// [`from_directory`]: TorrentFactory::from_directory
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
    ///
    /// # Panics
    ///
    /// Does not panic; use [`NonZeroU64::new`] to construct the argument safely.
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
    pub fn add_announce(mut self, announce: Url) -> Self {
        self.get_last_announce_tier().push(announce);
        self
    }

    /// Appends multiple tracker URLs to the *current* announce tier.
    ///
    /// Equivalent to calling [`add_announce`] repeatedly, but consumes an iterator instead
    /// of a single URL.
    ///
    /// [`add_announce`]: TorrentFactory::add_announce
    #[must_use]
    pub fn add_announces<I: IntoIterator<Item = Url>>(mut self, announces: I) -> Self {
        self.get_last_announce_tier().extend(announces);
        self
    }

    /// Begins a new announce tier.
    ///
    /// Subsequent calls to [`Self::add_announce`] / [`Self::add_announces`] will add URLs to this new
    /// tier rather than the previous one.  If the current tier is already empty this method
    /// is a no-op (empty tiers are not created).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use url::Url; use bitors::torrent::factory::TorrentFactory;
    /// let factory = TorrentFactory::new()
    ///     // Tier 0 — primary trackers
    ///     .add_announce(Url::parse("udp://primary.example.com:6969/announce").unwrap())
    ///     .next_announce_tier()
    ///     // Tier 1 — backup trackers
    ///     .add_announce(Url::parse("udp://backup.example.com:6969/announce").unwrap());
    /// ```
    ///
    /// [`add_announce`]: TorrentFactory::add_announce
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
    /// [`next_announce_tier`]: TorrentFactory::next_announce_tier
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
    /// for the default values).  At least one source file must be provided — via
    /// [`add_file`](TorrentFactory::add_file) or [`add_files`](TorrentFactory::add_files) —
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

    /// Transitions the factory to [`state::HasFiles`] by supplying a single source file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAFile`] if `file` does not exist or is not a regular file
    /// (e.g. it is a directory or a symlink to a directory).
    pub fn add_file(
        self,
        file: impl Into<PathBuf>,
    ) -> Result<TorrentFactory<state::HasFiles>, Error> {
        let file = file.into();

        if !file.is_file() {
            return Err(Error::NotAFile(file));
        }

        Ok(TorrentFactory {
            files: vec![file],
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
        })
    }

    /// Transitions the factory to [`state::HasFiles`] by supplying multiple source files.
    ///
    /// # Errors
    ///
    /// - [`Error::NoFiles`] — `files` is empty.
    /// - [`Error::NotAFile`] — any path in `files` is not a regular file.
    pub fn add_files<I: IntoIterator<Item = impl Into<PathBuf>>>(
        self,
        files: I,
    ) -> Result<TorrentFactory<state::HasFiles>, Error> {
        let files = files.into_iter().map(Into::into).collect::<Vec<_>>();

        if files.is_empty() {
            return Err(Error::NoFiles);
        }

        if let Some(p) = files.iter().find(|p| !p.is_file()) {
            return Err(Error::NotAFile(p.clone()));
        }

        Ok(TorrentFactory {
            files,
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
        })
    }
}

// ── HasFiles state ───────────────────────────────────────────────────────────

impl TorrentFactory<state::HasFiles> {
    /// Creates a factory pre-loaded with a single source file.
    ///
    /// Shorthand for `TorrentFactory::new().add_file(file)`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAFile`] if `file` does not exist or is not a regular file.
    pub fn from_file(file: impl Into<PathBuf>) -> Result<Self, Error> {
        TorrentFactory::new().add_file(file)
    }

    /// Creates a factory pre-loaded with multiple source files.
    ///
    /// Shorthand for `TorrentFactory::new().add_files(files)`.
    ///
    /// # Errors
    ///
    /// - [`Error::NoFiles`] — `files` is empty.
    /// - [`Error::NotAFile`] — any path in `files` is not a regular file.
    pub fn from_files<I: IntoIterator<Item = impl Into<PathBuf>>>(files: I) -> Result<Self, Error> {
        TorrentFactory::new().add_files(files)
    }

    /// Creates a factory pre-loaded with every file found (recursively) inside `dir`.
    ///
    /// The directory name is automatically used as the torrent name (overridable with
    /// [`name`](TorrentFactory::name)).  Files are sorted lexicographically so that the
    /// piece hashes are reproducible across runs.
    ///
    /// # Errors
    ///
    /// - [`Error::NotADir`] — `dir` does not exist or is not a directory.
    /// - [`Error::EmptyDir`] — `dir` contains no regular files (after recursion).
    /// - [`Error::InvalidPath`] — `dir` has no final component (e.g. `/`).
    /// - [`Error::NonUtf8Name`] — the directory name is not valid UTF-8.
    pub fn from_directory(dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let dir = dir.into();

        if !dir.is_dir() {
            return Err(Error::NotADir(dir));
        }

        let name = dir
            .file_name()
            .ok_or(Error::InvalidPath)?
            .to_str()
            .ok_or(Error::NonUtf8Name)?
            .to_owned();

        let mut files = WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();
        files.sort();

        if files.is_empty() {
            Err(Error::EmptyDir)
        } else {
            Ok(Self {
                files,
                name: Some(name),
                piece_length: None,
                private: false,
                source: None,
                announce_list: vec![],
                url_list: vec![],
                creation_date: None,
                created_by: None,
                comment: None,
                _state: PhantomData,
            })
        }
    }

    /// Adds a single source file to an already-populated factory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAFile`] if `file` does not exist or is not a regular file.
    pub fn add_file(mut self, file: impl Into<PathBuf>) -> Result<Self, Error> {
        let file = file.into();

        if !file.is_file() {
            return Err(Error::NotAFile(file));
        }

        self.files.push(file);
        Ok(self)
    }

    /// Adds multiple source files to an already-populated factory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAFile`] if any path in `files` is not a regular file.
    /// An empty iterator is accepted without error (the existing file list is unchanged).
    pub fn add_files<I: IntoIterator<Item = impl Into<PathBuf>>>(
        mut self,
        files: I,
    ) -> Result<Self, Error> {
        let files = files.into_iter().map(Into::into).collect::<Vec<_>>();

        if let Some(p) = files.iter().find(|p| !p.is_file()) {
            return Err(Error::NotAFile(p.clone()));
        }

        self.files.extend(files);
        Ok(self)
    }

    /// Consumes the factory and produces a serializable [`TorrentBuf`].
    ///
    /// This method performs all I/O: it reads every source file in order, computes SHA-1
    /// piece hashes, and gathers filesystem metadata (file sizes).  The resulting
    /// [`TorrentBuf`] can be serialized to a `.torrent` file with the encoding layer of
    /// your choice.
    ///
    /// # File mode
    ///
    /// | Source files | Torrent mode |
    /// |---|---|
    /// | Exactly one | Single-file (`length` key in `info`) |
    /// | More than one | Multi-file (`files` list in `info`) |
    ///
    /// # Announce list
    ///
    /// The first URL of the first non-empty tier becomes the top-level `announce` field for
    /// backward compatibility with clients that do not support `announce-list`.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] — any file could not be opened or read, or metadata could not be
    ///   queried.
    /// - [`Error::NonUtf8Name`] — a source file's path contains a non-UTF-8 component.
    /// - [`Error::PieceLengthTooLarge`] — the piece length overflows `usize` (only possible
    ///   on 32-bit targets).
    pub fn build(self) -> Result<TorrentBuf, Error> {
        let piece_length = self.piece_length.unwrap_or(
            #[expect(clippy::missing_panics_doc, reason = "infallible")]
            NonZeroU64::new(512 * 1024).unwrap(),
        );

        let creation_date = self.creation_date.unwrap_or(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );

        let file_path_comps = Self::remove_common_prefix(&self.files)
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

        let file_infos = self
            .files
            .iter()
            .zip(file_path_comps)
            .map(|(path, comps)| -> Result<FileInfo, Error> {
                Ok(FileInfo {
                    length: std::fs::metadata(path)?.len(),
                    md5sum: None,
                    path: comps.into_iter().map(Cow::Owned).collect(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let pieces = Self::compute_piece_hashes(self.files, piece_length_usize(piece_length)?)?;

        let name = match self.name {
            Some(name) => name,
            #[expect(clippy::missing_panics_doc, reason = "infallible")]
            None => file_infos[0]
                .full_path()
                .file_name()
                .expect("The file name should be correct since the file has already been processed")
                .to_str()
                .expect("Should not fail since UTF-8 validity has already been checked earlier")
                .to_string(),
        };

        let file_mode = match file_infos.len() {
            0 => unreachable!("TorrentFactory<HasFiles> does not allow an empty file vector"),
            1 => FileMode::Single {
                length: file_infos[0].length,
                md5sum: None,
            },
            _ => FileMode::Multi { files: file_infos },
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
            encoding: Some(Cow::Owned("UTF-8".to_string())),
        })
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
    fn compute_piece_hashes<I: IntoIterator<Item = PathBuf>>(
        paths: I,
        piece_length: usize,
    ) -> Result<Vec<[u8; 20]>, Error> {
        let mut hashes = vec![];
        let mut chunk = vec![0u8; piece_length];
        let mut iter = paths
            .into_iter()
            .map(|p| File::open(p).map(|f| BufReader::with_capacity(piece_length, f)));
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

            hashes.push(Sha1::digest(&chunk[..total]).into());
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
    fn remove_common_prefix(paths: &[PathBuf]) -> Vec<PathBuf> {
        debug_assert!(!paths.is_empty());

        let mut prefix = paths[0].clone();

        for s in &paths[1..] {
            while !s.starts_with(&prefix) {
                if prefix.parent().is_none() {
                    break;
                }
                prefix.pop();
            }
        }

        paths
            .iter()
            .map(|s| s.strip_prefix(&prefix).unwrap_or(s).to_owned())
            .collect()
    }
}

/// Errors that can be returned by [`TorrentFactory`].
#[derive(Debug, Error)]
pub enum Error {
    /// An underlying I/O error occurred while reading a file or querying its metadata.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// [`TorrentFactory::add_files`] was called with an empty iterator.
    #[error("No files were provided to the factory")]
    NoFiles,

    /// [`TorrentFactory::from_directory`] was called on a directory that contains no
    /// regular files (after recursing into subdirectories).
    #[error("An empty directory was provided to the factory")]
    EmptyDir,

    /// A path has no final component and therefore no usable file name (e.g. the root `/`).
    #[error("Path has no file name component")]
    InvalidPath,

    /// A file or directory name cannot be represented as UTF-8.
    ///
    /// The BitTorrent specification requires all names and path components in the `info`
    /// dictionary to be UTF-8 strings.
    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,

    /// A path that was expected to point to a regular file does not.
    #[error("The provided path does not correspond to a file: {0}")]
    NotAFile(PathBuf),

    /// A path that was expected to point to a directory does not.
    #[error("The provided path does not correspond to a directory: {0}")]
    NotADir(PathBuf),

    /// The requested piece length does not fit in `usize` and cannot be used as a buffer
    /// size on this platform.  This can only occur on 32-bit targets.
    #[error("The provided piece length is too large (does not fit in usize): {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
