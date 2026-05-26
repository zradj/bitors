//! Magnet link generation for BitTorrent torrents.
//!
//! This module provides [`MagnetLink`], which represents a [magnet URI] for a
//! BitTorrent torrent.  Magnet links encode the information needed to locate and
//! download a torrent — the info hash, optional display name, tracker URLs, and
//! total content size — in a compact URI that does not require a separate `.torrent`
//! file to be distributed.
//!
//! [magnet URI]: https://en.wikipedia.org/wiki/Magnet_URI_scheme
//!
//! # Creating a magnet link
//!
//! The most common path is to convert a parsed or constructed [`Torrent`] directly
//! via [`Torrent::magnet_link`]:
//!
//! ```no_run
//! use bitors::parse_torrent;
//!
//! let bytes = std::fs::read("ubuntu.torrent")?;
//! let torrent = parse_torrent(&bytes)?;
//!
//! let link = torrent.magnet_link();
//! println!("{link}");   // magnet:?xt=urn:btih:<hex-hash>&dn=...&tr=...
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! You can also construct a [`MagnetLink`] manually when you only have an info
//! hash (e.g. from a database) and want to attach your own tracker list:
//!
//! ```no_run
//! use bitors::magnet::MagnetLink;
//!
//! let link = MagnetLink {
//!     info_hash: [0xab; 20],
//!     name: Some("My Torrent".to_string()),
//!     trackers: vec!["udp://tracker.example.com:6969/announce".parse()?],
//!     size: Some(1_073_741_824),
//! };
//!
//! println!("{link}");                  // hex format (default)
//! println!("{}", link.to_uri_base32()); // Base32 format
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # URI formats
//!
//! The BitTorrent specification allows the info hash in a magnet URI to be encoded
//! in two ways:
//!
//! - **Hex** (the de-facto standard): `xt=urn:btih:<40 lowercase hex chars>`.
//!   Produced by [`Display`](std::fmt::Display) and [`MagnetLink::to_uri_hex`].
//! - **Base32**: `xt=urn:btih:<32 uppercase Base32 chars>`.
//!   Produced by [`MagnetLink::to_uri_base32`].
//!
//! Both formats encode the same 20-byte SHA-1 info hash and are accepted by all
//! major BitTorrent clients.  Hex is more common and is therefore the default
//! `Display` representation.

use std::fmt::{self, Write};

use data_encoding::{BASE32, HEXLOWER};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use url::Url;

use crate::torrent::Torrent;

const URI_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// A magnet link for a BitTorrent torrent.
///
/// A `MagnetLink` can be created from a [`Torrent`] via the [`From`] impl or the
/// convenience method [`Torrent::magnet_link`], or constructed manually when only
/// an info hash is available.
///
/// The URI is produced by [`Display`](std::fmt::Display) (hex format) or by
/// [`to_uri_hex`](Self::to_uri_hex) / [`to_uri_base32`](Self::to_uri_base32).
///
/// # Example
///
/// ```no_run
/// use bitors::parse_torrent;
///
/// let bytes = std::fs::read("ubuntu.torrent")?;
/// let torrent = parse_torrent(&bytes)?;
///
/// let link = torrent.magnet_link();
/// println!("{link}");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MagnetLink {
    /// The 20-byte SHA-1 info hash that uniquely identifies the torrent's content.
    pub info_hash: [u8; 20],
    /// An optional 32-byte SHA-256 digest of the `info` dictionary ([BEP 52]).
    ///
    /// When set, this hash is appended to the URI as an additional
    /// `&xt=urn:btmh:<64 hex chars>` parameter alongside the standard SHA-1
    /// `xt=urn:btih` parameter, producing a hybrid v1/v2 magnet link.  `None`
    /// produces a v1-only URI.
    ///
    /// [BEP 52]: https://www.bittorrent.org/beps/bep_0052.html
    pub info_hash_v2: Option<[u8; 32]>,
    /// An optional human-readable display name (`dn` parameter).
    ///
    /// When present this is percent-encoded and appended to the URI as `&dn=<name>`.
    /// Most clients show this name before the download starts.
    pub name: Option<String>,
    /// Tracker URLs to include in the URI (`tr` parameters).
    ///
    /// Each URL is percent-encoded and appended as a separate `&tr=<url>` parameter.
    /// Clients that support the magnet scheme will use these URLs to find peers.
    pub trackers: Vec<Url>,
    /// The total content size in bytes (`xl` parameter), if known.
    ///
    /// When present this is appended as `&xl=<bytes>`.  Clients may use this value
    /// to display download progress before the metadata has been fetched.
    pub size: Option<u64>,
}

impl MagnetLink {
    /// Creates a [`MagnetLink`] from a [`Torrent`], including the SHA-256 info hash.
    ///
    /// Identical to [`From<&Torrent>`] except that the SHA-256 digest of the `info`
    /// dictionary is also populated in [`info_hash_v2`](Self::info_hash_v2).  The
    /// resulting URI includes both `xt=urn:btih` (SHA-1) and `xt=urn:btmh` (SHA-256)
    /// parameters, producing a hybrid BitTorrent v1/v2 magnet link as described in
    /// [BEP 52].
    ///
    /// Use [`Torrent::magnet_link_v2`](crate::torrent::Torrent::magnet_link_v2) for
    /// the same operation via a method on `Torrent`.
    ///
    /// [BEP 52]: https://www.bittorrent.org/beps/bep_0052.html
    ///
    /// # Example
    ///
    /// ```no_run
    /// use bitors::{parse_torrent, magnet::MagnetLink};
    ///
    /// let bytes = std::fs::read("ubuntu.torrent")?;
    /// let torrent = parse_torrent(&bytes)?;
    ///
    /// let link = MagnetLink::from_torrent_v2(&torrent);
    /// println!("{link}");   // magnet:?xt=urn:btih:<sha1>&xt=urn:btmh:<sha256>&dn=...
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn from_torrent_v2(torrent: &Torrent) -> Self {
        let mut res = Self::from(torrent);
        res.info_hash_v2 = Some(torrent.info_hash_v2());
        res
    }
    /// Returns the magnet URI with the info hash encoded as 40 lowercase hex characters.
    ///
    /// This is equivalent to calling `self.to_string()` and is the most widely
    /// supported format.  Use [`to_uri_base32`](Self::to_uri_base32) if you need
    /// the alternative Base32 encoding instead.
    #[must_use]
    pub fn to_uri_hex(&self) -> String {
        self.to_string()
    }

    /// Returns the magnet URI with the info hash encoded as 32 uppercase Base32 characters.
    ///
    /// Both hex and Base32 encodings represent the same 20-byte SHA-1 info hash and
    /// are accepted by all major BitTorrent clients.  Hex is more common; prefer
    /// [`to_uri_hex`](Self::to_uri_hex) unless you specifically need Base32.
    #[must_use]
    pub fn to_uri_base32(&self) -> String {
        let mut res = String::with_capacity(128);
        let mut buf = [0u8; 32];
        BASE32.encode_mut(&self.info_hash, &mut buf);

        unsafe {
            let _ = self.write_link(&mut res, std::str::from_utf8_unchecked(&buf));
        };

        res
    }

    fn write_link(&self, w: &mut impl Write, hash_enc: &str) -> fmt::Result {
        write!(w, "magnet:?xt=urn:btih:{hash_enc}")?;

        if let Some(info_hash_v2) = &self.info_hash_v2 {
            let mut buf = [0u8, 64];
            HEXLOWER.encode_mut(info_hash_v2, &mut buf);
            unsafe {
                write!(w, "&xt=urn:btmh:{}", std::str::from_utf8_unchecked(&buf))?;
            }
        }

        if let Some(name) = &self.name {
            let encoded = utf8_percent_encode(name, URI_SET);
            write!(w, "&dn={encoded}")?;
        }

        if let Some(size) = self.size {
            write!(w, "&xl={size}")?;
        }

        for tracker in &self.trackers {
            let encoded = utf8_percent_encode(tracker.as_str(), URI_SET);
            write!(w, "&tr={encoded}")?;
        }

        Ok(())
    }
}

impl From<&Torrent<'_>> for MagnetLink {
    fn from(torrent: &Torrent) -> Self {
        let trackers = torrent.trackers().into_iter().flatten().cloned().collect();

        Self {
            info_hash: torrent.info_hash(),
            info_hash_v2: None,
            name: Some(torrent.info.name.to_string()),
            trackers,
            size: Some(torrent.total_size()),
        }
    }
}

impl fmt::Display for MagnetLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; 40];
        HEXLOWER.encode_mut(&self.info_hash, &mut buf);

        unsafe { self.write_link(f, std::str::from_utf8_unchecked(&buf))? };

        Ok(())
    }
}
