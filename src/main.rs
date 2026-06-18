use std::{error::Error, fs::File};

use bitors::TorrentBuilder;
use sha1::{Digest, Sha1};
use sha2::Sha256;

fn main() -> Result<(), Box<dyn Error>> {
    let torrent = TorrentBuilder::from_path("/home/zaur/projects")?.build_hybrid()?;

    let mut file = File::create("new.torrent")?;
    torrent.to_bencode().encode_to_writer(&mut file)?;

    // let mut file = File::open("new.torrent")?;
    // let mut data = vec![];
    // file.read_to_end(&mut data)?;
    // let torrent = parse_torrent(&data)?;

    // println!("{torrent:#?}");

    // println!("{}", torrent.magnet_link());

    Ok(())
}
