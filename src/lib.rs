#![doc = include_str!("../README.md")]
#![warn(clippy::pedantic)]
#![warn(missing_docs)]

pub mod bencode;
pub mod error;
pub mod magnet;
pub mod torrent;

pub use bencode::Parser;
pub use torrent::Torrent;
pub use torrent::builder::TorrentBuilder;

use crate::error::Error;

/// Parses a [`Torrent`] from raw data.
///
/// Prefer this function over calling [`Torrent::try_from`] on the output of [`Parser`]: it also
/// records the original bytes of the `info` dictionary, so the info hash always matches the
/// original file (see [`Torrent::raw_info`]).
///
/// The data must contain nothing after the torrent. Use [`parse_torrent_prefix`] to allow
/// trailing data.
///
/// The returned [`Torrent`]'s lifetime is tied to the passed data.
///
/// # Examples
///
/// ```no_run
/// # use std::{error::Error, fs::File, io::Read};
/// # use bitors::parse_torrent;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// let mut file = File::open("my_torrent.torrent")?;
/// let mut data = vec![];
/// file.read_to_end(&mut data)?;
///
/// let torrent = parse_torrent(&data)?;
///
/// // Do something else...
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns an error if `data` is not valid bencode (including when there are bytes after the
/// torrent's root dictionary), or if the decoded structure does not represent a valid torrent
/// metainfo file.
pub fn parse_torrent(data: &[u8]) -> Result<Torrent<'_>, Error> {
    let (torrent, end) = parse_torrent_prefix(data)?;
    if end < data.len() {
        return Err(bencode::Error::TrailingData(end).into());
    }
    Ok(torrent)
}

/// Parses a [`Torrent`] from the start of `data` and allows any bytes after it.
///
/// Returns the torrent and the position where it ends, i.e. the position of the first byte after
/// the torrent. Otherwise, this function behaves like [`parse_torrent`].
///
/// # Examples
///
/// ```no_run
/// # use std::{error::Error, fs};
/// # use bitors::parse_torrent_prefix;
/// # fn main() -> Result<(), Box<dyn Error>> {
/// let data = fs::read("my_torrent_with_trailer.torrent")?;
///
/// let (torrent, end) = parse_torrent_prefix(&data)?;
/// let trailer = &data[end..];
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns an error if `data` does not start with valid bencode, or if the decoded structure
/// does not represent a valid torrent metainfo file.
pub fn parse_torrent_prefix(data: &[u8]) -> Result<(Torrent<'_>, usize), Error> {
    let mut parser = Parser::new(data);
    let mut torrent = Torrent::try_from(parser.parse_prefix()?)?;
    torrent.set_raw_info(parser.info_bytes());
    Ok((torrent, parser.position()))
}
