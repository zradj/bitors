use std::{marker::PhantomData, path::PathBuf};

use thiserror::Error;
use url::Url;
use walkdir::WalkDir;

use crate::torrent::TorrentBuf;

mod state {
    #[derive(Debug)]
    pub struct Empty;
    #[derive(Debug)]
    pub struct HasFiles;
}

#[derive(Debug)]
pub struct TorrentFactory<State> {
    files: Vec<PathBuf>,
    name: Option<String>,
    piece_length: Option<u64>,
    private: bool,
    announce_list: Vec<Vec<Url>>,
    creation_date: Option<u64>,
    comment: Option<String>,
    _state: PhantomData<State>,
}

impl<T> TorrentFactory<T> {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn piece_length(mut self, piece_length: u64) -> Self {
        self.piece_length = Some(piece_length);
        self
    }

    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.creation_date = Some(creation_date);
        self
    }

    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    pub fn add_announce(mut self, announce: Url) -> Self {
        self.get_last_announce_tier().push(announce);
        self
    }

    pub fn add_announces<I: IntoIterator<Item = Url>>(mut self, announces: I) -> Self {
        self.get_last_announce_tier().extend(announces);
        self
    }

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
    pub fn new() -> Self {
        Self {
            files: vec![],
            name: None,
            piece_length: None,
            private: false,
            announce_list: vec![],
            creation_date: None,
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
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.into_path())
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
                comment: None,
                _state: PhantomData,
            })
        }
    }

    pub fn add_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.files.push(file.into());
        self
    }

    pub fn add_files<I: IntoIterator<Item = impl Into<PathBuf>>>(mut self, files: I) -> Self {
        self.files.extend(files.into_iter().map(Into::into));
        self
    }

    pub fn create(self) -> Result<TorrentBuf, Error> {
        todo!()
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
}
