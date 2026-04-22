use std::{marker::PhantomData, path::PathBuf};

use thiserror::Error;
use url::Url;

use crate::torrent::TorrentBuf;

#[derive(Debug)]
pub struct TorrentCreator<State> {
    files: Vec<PathBuf>,
    name: Option<String>,
    piece_length: Option<u64>,
    private: bool,
    announce_list: Vec<Vec<Url>>,
    creation_date: Option<u64>,
    comment: Option<String>,
    _state: PhantomData<State>,
}

#[derive(Debug)]
pub struct Empty;
#[derive(Debug)]
pub struct HasFiles;

impl<T> TorrentCreator<T> {
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
        self.announce_list.last_mut().unwrap().push(announce);
        self
    }

    pub fn add_announces<I: IntoIterator<Item = Url>>(mut self, announces: I) -> Self {
        self.announce_list.last_mut().unwrap().extend(announces);
        self
    }

    pub fn next_announce_tier(mut self) -> Self {
        if !self.announce_list.last().unwrap().is_empty() {
            self.announce_list.push(vec![]);
        }
        self
    }
}

impl TorrentCreator<Empty> {
    pub fn new() -> Self {
        Self {
            files: vec![],
            name: None,
            piece_length: None,
            private: false,
            announce_list: vec![vec![]],
            creation_date: None,
            comment: None,
            _state: PhantomData,
        }
    }

    pub fn add_file(self, file: PathBuf) -> TorrentCreator<HasFiles> {
        TorrentCreator {
            files: vec![file],
            name: self.name,
            piece_length: self.piece_length,
            private: self.private,
            announce_list: self.announce_list,
            creation_date: self.creation_date,
            comment: self.comment,
            _state: PhantomData,
        }
    }

    pub fn add_files<I: IntoIterator<Item = PathBuf>>(
        self,
        files: I,
    ) -> Result<TorrentCreator<HasFiles>, Error> {
        let files = files.into_iter().collect::<Vec<_>>();

        if files.is_empty() {
            return Err(Error::NoFiles);
        }

        Ok(TorrentCreator {
            files,
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

impl TorrentCreator<HasFiles> {
    pub fn add_file(mut self, file: PathBuf) -> Self {
        self.files.push(file);
        self
    }

    pub fn add_files<I: IntoIterator<Item = PathBuf>>(mut self, files: I) -> Self {
        self.files.extend(files);
        self
    }

    pub fn create(self) -> Result<TorrentBuf, Error> {
        todo!()
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("No files were provided to the creator")]
    NoFiles,
}
