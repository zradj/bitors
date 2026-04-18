use url::Url;

use crate::torrent::owned::{OwnedInfo, OwnedTorrent};

pub struct TorrentBuilder {
    info: OwnedInfo,
    announce: Option<Url>,
    announce_list: Option<Vec<Vec<Url>>>,
    creation_date: Option<u64>,
    comment: Option<String>,
    created_by: Option<String>,
    encoding: Option<String>,
}

impl TorrentBuilder {
    pub fn new(info: OwnedInfo) -> Self {
        Self {
            info,
            announce: None,
            announce_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
            encoding: None,
        }
    }

    pub fn announce(mut self, announce: Url) -> Self {
        self.announce = Some(announce);
        self
    }

    pub fn announce_list(mut self, announce_list: Vec<Vec<Url>>) -> Self {
        self.announce_list = Some(announce_list);
        self
    }

    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.creation_date = Some(creation_date);
        self
    }

    pub fn comment(mut self, comment: &str) -> Self {
        self.comment = Some(comment.to_string());
        self
    }

    pub fn created_by(mut self, created_by: &str) -> Self {
        self.created_by = Some(created_by.to_string());
        self
    }

    pub fn encoding(mut self, encoding: &str) -> Self {
        self.encoding = Some(encoding.to_string());
        self
    }

    pub fn build(self) -> Result<OwnedTorrent, super::Error> {
        if !self.info.private && self.announce.is_none() && self.announce_list.is_none() {
            return Err(super::Error::MissingAnnounce);
        }

        Ok(OwnedTorrent {
            info: self.info,
            announce: self.announce,
            announce_list: self.announce_list,
            creation_date: self.creation_date,
            comment: self.comment,
            created_by: self.created_by,
            encoding: self.encoding,
        })
    }
}
