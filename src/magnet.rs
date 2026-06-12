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

/// A [magnet link](https://en.wikipedia.org/wiki/Magnet_URI_scheme) representation.
///
/// A magnet link is an [URI](https://en.wikipedia.org/wiki/Uniform_Resource_Identifier) that contains
/// the info hash of the torrent (or the two hashes in the case of a hybrid torrent).
/// It can also optionally contain the name of the torrent, its total size, and the trackers.
///
/// To construct a [`MagnetLink`] from a [`Torrent`], use [`Torrent::magnet_link`] or [`MagnetLink::from`].
///
/// # Examples
///
/// ```no_run
/// use bitors::Torrent;
///
/// let torrent = Torrent::builder().add_path("my_file").unwrap().build().unwrap();
/// println!("{}", torrent.magnet_link());
/// // Hybrid torrent: magnet:?xt=urn:btih:<40 chars of v1 hash in hex>&xt=urn:btmh:<64 chars of v2 hash in hex>...
/// ```
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct MagnetLink {
    /// The SHA-1 info hash of a v1-only or hybrid torrent. Set to [`None`] if the torrent is v2-only.
    pub info_hash_v1: Option<[u8; 20]>,
    /// The SHA-256 info hash of a v2-only or hybrid torrent. Set to [`None`] if the torrent is v1-only.
    pub info_hash_v2: Option<[u8; 32]>,
    /// The optional name of the torrent.
    pub name: Option<String>,
    /// A flat list of the torrent trackers ([`Torrent::announce_list`]).
    pub trackers: Vec<Url>,
    /// The total size of the torrent.
    pub size: Option<u64>,
    /// Indicates whether [`MagnetLink::info_hash_v1`] should be encoded as Base32 instead of Hex.
    /// Set to `false` by default but can be toggled using the builder method [`MagnetLink::v1_base32`].
    ///
    /// Note that [`MagnetLink::info_hash_v2`] can only be encoded in Hex.
    pub v1_base32: bool,
}

impl MagnetLink {
    /// Encode [`MagnetLink::info_hash_v1`] as Base32 instead of Hex.
    ///
    /// This is the preferred option by some clients.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use bitors::Torrent;
    ///
    /// let torrent = Torrent::builder().add_path("my_file").unwrap().build_v1().unwrap();
    /// println!("{}", torrent.magnet_link().v1_base32());
    /// // v1 torrent: magnet:?xt=urn:btih:<32 chars of v1 hash in base32>...
    /// ```
    #[must_use]
    pub fn v1_base32(mut self) -> Self {
        self.v1_base32 = true;
        self
    }
}

impl From<&Torrent<'_>> for MagnetLink {
    /// Constructs a [`MagnetLink`] from a [`Torrent`].
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
    /// Formats this value as a [magnet URI](https://en.wikipedia.org/wiki/Magnet_URI_scheme).
    ///
    /// The output always starts with `magnet:` and then appends query parameters in the following order:
    /// `xt` for the v1 info hash, `xt` for the v2 info hash, `dn` for the name of the torrent, `xl` for the total size,
    /// and `tr` for each tracker URI.
    ///
    /// Encoding details:
    /// - `xt` contains the prefix `urn:btih:` for the v1 info hash and `urn:btmh:1220` for the v2 info hash.
    /// - The v1 info hash is encoded either as Base32 (32 characters) or Hex (40 characters) depending on
    ///   [`field@MagnetLink::v1_base32`]. The v2 info hash is always encoded as Hex (64 characters).
    ///
    /// Magnet links for hybrid torrents contain the v1 info hash first, followed by the v2 info hash. The v1-only and
    /// v2-only torrents contain only their respective info hashes in the magnet link.
    ///
    /// Either [`MagnetLink::info_hash_v1`] or [`MagnetLink::info_hash_v2`] must be set to produce a valid link.
    /// This is guaranteed by [`Torrent`] via [`Torrent::magnet_link`] or [`MagnetLink::from`] if the torrent
    /// was parsed/constructed using this crate's methods.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "magnet:")?;
        let mut has_query = false;

        if let Some(info_hash_v1) = &self.info_hash_v1 {
            let mut buf = if self.v1_base32 {
                vec![0u8; 32]
            } else {
                vec![0u8; 40]
            };
            let encoding = if self.v1_base32 { BASE32 } else { HEXLOWER };
            encoding.encode_mut(info_hash_v1, &mut buf);

            write!(f, "?xt=urn:btih:{}", std::str::from_utf8(&buf).unwrap())?;
            has_query = true;
        }

        if let Some(info_hash_v2) = &self.info_hash_v2 {
            let mut buf = [0u8; 64];
            HEXLOWER.encode_mut(info_hash_v2, &mut buf);

            let prefix = if has_query { '&' } else { '?' };
            write!(
                f,
                "{}xt=urn:btmh:1220{}",
                prefix,
                std::str::from_utf8(&buf).unwrap()
            )?;
            has_query = true;
        }

        if let Some(name) = &self.name {
            let encoded = utf8_percent_encode(name, URI_SET);
            let prefix = if has_query { '&' } else { '?' };
            write!(f, "{prefix}dn={encoded}")?;
            has_query = true;
        }

        if let Some(size) = self.size {
            let prefix = if has_query { '&' } else { '?' };
            write!(f, "{prefix}xl={size}")?;
            has_query = true;
        }

        for tracker in &self.trackers {
            let encoded = utf8_percent_encode(tracker.as_str(), URI_SET);
            let prefix = if has_query { '&' } else { '?' };
            write!(f, "{prefix}tr={encoded}")?;
            has_query = true;
        }

        Ok(())
    }
}
