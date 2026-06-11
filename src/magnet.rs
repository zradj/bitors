use std::fmt::{self};

use data_encoding::{BASE32, HEXLOWER};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use url::Url;

use crate::torrent::Torrent;

const URI_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MagnetLink {
    pub info_hash_v1: Option<[u8; 20]>,

    pub info_hash_v2: Option<[u8; 32]>,

    pub name: Option<String>,

    pub trackers: Vec<Url>,

    pub size: Option<u64>,

    pub v1_base32: bool,
}

impl MagnetLink {
    #[must_use]
    pub fn v1_base32(mut self) -> Self {
        self.v1_base32 = true;
        self
    }
}

impl From<&Torrent<'_>> for MagnetLink {
    fn from(torrent: &Torrent) -> Self {
        let trackers = torrent.trackers().into_iter().flatten().cloned().collect();

        Self {
            info_hash_v1: torrent.info_hash_v1(),
            info_hash_v2: torrent.info_hash_v2(),
            name: Some(torrent.info.name.to_string()),
            trackers,
            size: Some(torrent.total_size()),
            v1_base32: false,
        }
    }
}

impl fmt::Display for MagnetLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "magnet:")?;

        if let Some(info_hash_v1) = &self.info_hash_v1 {
            let mut buf = if self.v1_base32 {
                vec![0u8; 32]
            } else {
                vec![0u8; 40]
            };
            let encoding = if self.v1_base32 { BASE32 } else { HEXLOWER };
            encoding.encode_mut(info_hash_v1, &mut buf);

            unsafe {
                write!(f, "?xt=urn:btih:{}", std::str::from_utf8_unchecked(&buf))?;
            }
        }

        if let Some(info_hash_v2) = &self.info_hash_v2 {
            let mut buf = [0u8; 64];
            HEXLOWER.encode_mut(info_hash_v2, &mut buf);

            let initial_char = if self.info_hash_v1.is_some() {
                '&'
            } else {
                '?'
            };
            unsafe {
                write!(
                    f,
                    "{}xt=urn:btmh:1220{}",
                    initial_char,
                    std::str::from_utf8_unchecked(&buf)
                )?;
            }
        }

        if let Some(name) = &self.name {
            let encoded = utf8_percent_encode(name, URI_SET);
            write!(f, "&dn={encoded}")?;
        }

        if let Some(size) = self.size {
            write!(f, "&xl={size}")?;
        }

        for tracker in &self.trackers {
            let encoded = utf8_percent_encode(tracker.as_str(), URI_SET);
            write!(f, "&tr={encoded}")?;
        }

        Ok(())
    }
}
