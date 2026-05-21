//! Assembles a [`TorrentBuf`] from a pre-built [`InfoBuf`] and optional metadata.
//!
//! [`TorrentBuilder`] is the right tool when you already hold a fully constructed
//! [`InfoBuf`] — for example, one produced by [`TorrentFactory`] or extracted from
//! an existing `.torrent` file — and you want to wrap it in a top-level
//! [`Torrent`](crate::torrent::Torrent) with tracker URLs and descriptive fields.
//!
//! If you need the library to read source files from disk and compute piece hashes
//! for you, use [`TorrentFactory`](crate::torrent::factory::TorrentFactory) instead.
//!
//! # Usage
//!
//! 1. Obtain an [`InfoBuf`].
//! 2. Call [`Torrent::builder`](crate::torrent::Torrent::builder) (or
//!    [`TorrentBuilder::new`] directly) to create a builder.
//! 3. Chain any combination of the optional setter methods.
//! 4. Call [`build`](TorrentBuilder::build) to validate and produce the [`TorrentBuf`].
//!
//! # Example
//!
//! ```no_run
//! use bitors::torrent::{Torrent, InfoBuf};
//!
//! # fn make_info() -> InfoBuf { todo!() }
//! let torrent = Torrent::builder(make_info())
//!     .announce("udp://tracker.example.com:6969/announce".parse().unwrap())
//!     .comment("nightly build")
//!     .created_by("my-tool/1.0")
//!     .build()
//!     .expect("announce URL was provided");
//! ```
//!
//! # Validation
//!
//! [`build`](TorrentBuilder::build) enforces one rule from the BitTorrent
//! specification: a non-private torrent **must** have at least one tracker URL
//! (either `announce` or `announce_list`).  Private torrents may omit trackers
//! entirely — they distribute peer lists through other means.  Any violation
//! returns [`torrent::Error::MissingAnnounce`](super::Error::MissingAnnounce).

use std::borrow::Cow;

use url::Url;

use crate::torrent::{InfoBuf, TorrentBuf};

/// A builder for assembling a [`TorrentBuf`] around a pre-built [`InfoBuf`].
///
/// All setter methods consume `self` and return `Self`, so calls can be chained
/// fluently.  Every field except [`info`](TorrentBuilder::new) is optional; unset
/// fields are omitted from the serialized output.
///
/// Construct this type via [`Torrent::builder`](crate::torrent::Torrent::builder)
/// or [`TorrentBuilder::new`].
pub struct TorrentBuilder {
    info: InfoBuf,
    announce: Option<Url>,
    announce_list: Option<Vec<Vec<Url>>>,
    url_list: Option<Vec<Url>>,
    creation_date: Option<u64>,
    comment: Option<String>,
    created_by: Option<String>,
    encoding: Option<String>,
}

impl TorrentBuilder {
    /// Creates a new builder pre-loaded with `info`.
    ///
    /// All optional fields default to `None`; set them with the builder methods
    /// before calling [`build`](Self::build).
    #[must_use]
    pub fn new(info: InfoBuf) -> Self {
        Self {
            info,
            announce: None,
            announce_list: None,
            url_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
            encoding: None,
        }
    }

    /// Sets the primary tracker URL (`announce` field).
    ///
    /// This is the single URL stored in the top-level `announce` key for
    /// backward compatibility with clients that do not understand
    /// `announce-list`.  If you only need one tracker, setting this field is
    /// sufficient; if you need tiered fallback trackers, use
    /// [`announce_list`](Self::announce_list) instead (or in addition).
    #[must_use]
    pub fn announce(mut self, announce: Url) -> Self {
        self.announce = Some(announce);
        self
    }

    /// Sets the full tiered tracker list (`announce-list` field).
    ///
    /// Each inner `Vec<Url>` is one *tier*. Within a tier, clients may try
    /// URLs in random order; they move to the next tier only after all URLs in
    /// the current tier have failed.  See [BEP 12] for the full specification.
    ///
    /// When `announce_list` is present, clients that support it ignore the
    /// top-level `announce` field.  Setting both fields ensures compatibility
    /// with older clients.
    ///
    /// [BEP 12]: https://www.bittorrent.org/beps/bep_0012.html
    #[must_use]
    pub fn announce_list(mut self, announce_list: Vec<Vec<Url>>) -> Self {
        self.announce_list = Some(announce_list);
        self
    }

    #[must_use]
    pub fn url_list(mut self, url_list: Vec<Url>) -> Self {
        self.url_list = Some(url_list);
        self
    }

    /// Sets the torrent creation timestamp as seconds since the UNIX epoch.
    ///
    /// If not set, the field is omitted from the serialized output.  Many
    /// clients display this as a human-readable creation date in their UI.
    #[must_use]
    pub fn creation_date(mut self, creation_date: u64) -> Self {
        self.creation_date = Some(creation_date);
        self
    }

    /// Sets the free-form `comment` field.
    ///
    /// Displayed verbatim by most torrent clients.  No length limit is imposed
    /// by the specification, but extremely long comments may be ignored by some
    /// clients.
    #[must_use]
    pub fn comment(mut self, comment: &str) -> Self {
        self.comment = Some(comment.to_string());
        self
    }

    /// Sets the `created by` field.
    ///
    /// Conventionally contains the name and version of the program used to
    /// create the torrent, e.g. `"my-tool/2.3.1"`.  If not set, the field is
    /// omitted from the output.
    #[must_use]
    pub fn created_by(mut self, created_by: &str) -> Self {
        self.created_by = Some(created_by.to_string());
        self
    }

    /// Sets the `encoding` field.
    ///
    /// Declares the string encoding used for the `name` and `path` fields in
    /// the `info` dictionary.  The BitTorrent specification recommends `"UTF-8"`.
    /// If not set, the field is omitted — most modern clients assume UTF-8
    /// regardless.
    #[must_use]
    pub fn encoding(mut self, encoding: &str) -> Self {
        self.encoding = Some(encoding.to_string());
        self
    }

    /// Validates the builder state and constructs the final [`TorrentBuf`].
    ///
    /// # Errors
    ///
    /// Returns [`torrent::Error::MissingAnnounce`](super::Error::MissingAnnounce)
    /// if the torrent is not marked private and neither `announce` nor
    /// `announce_list` has been set.
    pub fn build(self) -> Result<TorrentBuf, super::Error> {
        if !self.info.private && self.announce.is_none() && self.announce_list.is_none() {
            return Err(super::Error::MissingAnnounce);
        }

        Ok(TorrentBuf {
            info: self.info,
            announce: self.announce,
            announce_list: self.announce_list,
            url_list: self.url_list,
            creation_date: self.creation_date,
            comment: self.comment.map(Cow::Owned),
            created_by: self.created_by.map(Cow::Owned),
            encoding: self.encoding.map(Cow::Owned),
        })
    }
}
