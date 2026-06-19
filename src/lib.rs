#![warn(clippy::pedantic)]

pub mod bencode;
pub mod error;
pub mod magnet;
pub mod torrent;

pub use bencode::Parser;
pub use torrent::Torrent;
pub use torrent::builder::TorrentBuilder;

use crate::error::Error;

/// A convenience function to quickly parse a [`Torrent`] from raw data.
///
/// This function is equivalent to creating a [`Parser`] instance, parsing the data, and then
/// calling [`Torrent::try_from`] on the resulting [`Bencode`](crate::bencode::Bencode).
///
/// The returned [`Torrent`]'s lifetime is tied to the passed data.
///
/// # Examples
///
/// ```no_run
/// # use std::fs::File;
/// # fn main() -> Result<(), Error> {
/// let file = File::open("my_torrent.torrent")?;
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
/// Returns an error if `data` is not valid bencode, or if the the decoded
/// structure does not represent a valid torrent metainfo file.
pub fn parse_torrent(data: &[u8]) -> Result<Torrent<'_>, Error> {
    Ok(Torrent::try_from(Parser::new(data).parse()?)?)
}
