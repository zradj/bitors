#![warn(clippy::pedantic)]

pub mod bencode;
pub mod error;
pub mod magnet;
pub mod torrent;

pub use bencode::Parser;
pub use torrent::Torrent;
pub use torrent::builder::TorrentBuilder;

use crate::error::Error;

pub fn parse_torrent(data: &[u8]) -> Result<Torrent<'_>, Error> {
    Ok(Parser::new(data).parse()?.try_into()?)
}
