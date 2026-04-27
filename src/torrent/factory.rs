use std::{
    borrow::Cow,
    fs::File,
    io::{self, BufReader, Read},
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

pub mod state {
    #[derive(Debug)]
    pub struct Empty;
    #[derive(Debug)]
    pub struct HasFiles;
}

fn piece_length_usize(piece_length: NonZeroU64) -> Result<usize, Error> {
    piece_length
        .get()
        .try_into()
        .map_err(|_| Error::PieceLengthTooLarge(piece_length))
}

#[derive(Debug)]
pub struct TorrentFactory<State> {
    files: Vec<PathBuf>,
    name: Option<String>,
    piece_length: Option<NonZeroU64>,
    private: bool,
    announce_list: Vec<Vec<Url>>,
    creation_date: Option<u64>,
    created_by: Option<String>,
    comment: Option<String>,
    _state: PhantomData<State>,
}

impl<T> TorrentFactory<T> {
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    #[must_use]
    pub fn piece_length(mut self, piece_length: NonZeroU64) -> Self {
        self.piece_length = Some(piece_length);
        self
    }

    #[must_use]
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    #[must_use]
    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.creation_date = Some(creation_date);
        self
    }

    #[must_use]
    pub fn created_by(mut self, created_by: impl Into<String>) -> Self {
        self.created_by = Some(created_by.into());
        self
    }

    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    #[must_use]
    pub fn add_announce(mut self, announce: Url) -> Self {
        self.get_last_announce_tier().push(announce);
        self
    }

    #[must_use]
    pub fn add_announces<I: IntoIterator<Item = Url>>(mut self, announces: I) -> Self {
        self.get_last_announce_tier().extend(announces);
        self
    }

    #[must_use]
    pub fn next_announce_tier(mut self) -> Self {
        if !self.get_last_announce_tier().is_empty() {
            self.announce_list.push(vec![]);
        }
        self
    }

    fn get_last_announce_tier(&mut self) -> &mut Vec<Url> {
        if self.announce_list.is_empty() {
            self.announce_list.push(vec![]);
        }
        self.announce_list.last_mut().unwrap()
    }
}

impl Default for TorrentFactory<state::Empty> {
    fn default() -> Self {
        Self::new()
    }
}

impl TorrentFactory<state::Empty> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: vec![],
            name: None,
            piece_length: None,
            private: false,
            announce_list: vec![],
            creation_date: None,
            created_by: None,
            comment: None,
            _state: PhantomData,
        }
    }

    pub fn add_file(
        self,
        file: impl Into<PathBuf>,
    ) -> Result<TorrentFactory<state::HasFiles>, Error> {
        let path = file.into();

        if !path.is_file() {
            return Err(Error::NotAFile(path));
        }

        Ok(TorrentFactory {
            files: vec![path],
            name: self.name,
            piece_length: self.piece_length,
            private: self.private,
            announce_list: self.announce_list,
            creation_date: self.creation_date,
            created_by: self.created_by,
            comment: self.comment,
            _state: PhantomData,
        })
    }

    pub fn add_files<I: IntoIterator<Item = impl Into<PathBuf>>>(
        self,
        files: I,
    ) -> Result<TorrentFactory<state::HasFiles>, Error> {
        let mut iter = files.into_iter().peekable();

        if iter.peek().is_none() {
            return Err(Error::NoFiles);
        }

        Ok(TorrentFactory {
            files: iter.map(Into::into).collect(),
            name: self.name,
            piece_length: self.piece_length,
            private: self.private,
            announce_list: self.announce_list,
            creation_date: self.creation_date,
            created_by: self.created_by,
            comment: self.comment,
            _state: PhantomData,
        })
    }
}

impl TorrentFactory<state::HasFiles> {
    pub fn from_file(file: impl Into<PathBuf>) -> Result<Self, Error> {
        TorrentFactory::new().add_file(file)
    }

    pub fn from_files<I: IntoIterator<Item = impl Into<PathBuf>>>(files: I) -> Result<Self, Error> {
        TorrentFactory::new().add_files(files)
    }

    pub fn from_directory(dir: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = dir.into();

        if !path.is_dir() {
            return Err(Error::NotADir(path));
        }

        let name = path
            .file_name()
            .ok_or(Error::InvalidPath)?
            .to_str()
            .ok_or(Error::NonUtf8Name)?
            .to_owned();

        let files = WalkDir::new(&path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(walkdir::DirEntry::into_path)
            .collect::<Vec<_>>();

        if files.is_empty() {
            Err(Error::EmptyDir)
        } else {
            Ok(Self {
                files,
                name: Some(name),
                piece_length: None,
                private: false,
                announce_list: vec![],
                creation_date: None,
                created_by: None,
                comment: None,
                _state: PhantomData,
            })
        }
    }

    #[must_use]
    pub fn add_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.files.push(file.into());
        self
    }

    #[must_use]
    pub fn add_files<I: IntoIterator<Item = impl Into<PathBuf>>>(mut self, files: I) -> Self {
        self.files.extend(files.into_iter().map(Into::into));
        self
    }

    pub fn create(self) -> Result<TorrentBuf, Error> {
        let piece_length = self
            .piece_length
            .unwrap_or(NonZeroU64::new(512 * 1024).unwrap());

        let creation_date = self.creation_date.unwrap_or(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );

        let file_path_comps = Self::remove_common_prefix(&self.files)
            .iter()
            .map(|p| -> Result<Vec<Cow<'_, str>>, Error> {
                p.components()
                    .map(|c| {
                        Ok(Cow::Owned(
                            c.as_os_str()
                                .to_str()
                                .ok_or(Error::NonUtf8Name)?
                                .to_string(),
                        ))
                    })
                    .collect()
            })
            .collect::<Result<Vec<_>, _>>()?;

        let file_infos = self
            .files
            .iter()
            .map(File::open)
            .zip(file_path_comps)
            .map(|(file, comps)| -> Result<FileInfo, Error> {
                Ok(FileInfo {
                    length: file?.metadata()?.len(),
                    md5sum: None,
                    path: comps,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let pieces = Self::compute_piece_hashes(self.files, piece_length_usize(piece_length)?)?;

        let name = match self.name {
            Some(name) => name,
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
            file_mode,
        };

        let announce = self
            .announce_list
            .first()
            .and_then(|tier| tier.first().cloned());

        Ok(Torrent {
            info,
            announce,
            announce_list: Some(self.announce_list),
            creation_date: Some(creation_date),
            comment: self.comment.map(Cow::Owned),
            created_by: self.created_by.map(Cow::Owned),
            encoding: Some(Cow::Owned("UTF-8".to_string())),
        })
    }

    fn compute_piece_hashes<I: IntoIterator<Item = PathBuf>>(
        paths: I,
        piece_length: usize,
    ) -> Result<Vec<[u8; 20]>, Error> {
        let mut hashes = vec![];
        let mut chunk = vec![0u8; piece_length];
        let mut iter = paths.into_iter().map(File::open);
        let mut file = iter
            .next()
            .expect("TorrentFactory<HasFiles> guarantees at least one file")?;

        loop {
            let mut total = 0;

            while total < piece_length {
                match file.read(&mut chunk[total..])? {
                    0 => {
                        if let Some(f) = iter.next() {
                            file = f?;
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

    fn remove_common_prefix(paths: &[PathBuf]) -> Vec<PathBuf> {
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

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("No files were provided to the factory")]
    NoFiles,
    #[error("An empty directory was provided to the factory")]
    EmptyDir,
    #[error("Path has no file name component")]
    InvalidPath,
    #[error("File/directory name is not valid UTF-8")]
    NonUtf8Name,
    #[error("The provided path does not correspond to a file: {0}")]
    NotAFile(PathBuf),
    #[error("The provided path does not correspond to a directory: {0}")]
    NotADir(PathBuf),
    #[error("The provided piece length is too large (does not fit in usize): {0}")]
    PieceLengthTooLarge(NonZeroU64),
}
