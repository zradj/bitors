use std::{error::Error, fs::File, io::Read};

use bitors::{TorrentBuilder, parse_torrent};

fn main() -> Result<(), Box<dyn Error>> {
    let torrent = TorrentBuilder::from_path("/home/zaur/Telegram")?.build_hybrid()?;

    let mut file = File::create("new.torrent")?;
    torrent.to_bencode().encode_to_writer(&mut file)?;

    // let mut file = File::open("new.torrent")?;
    // let mut data = vec![];
    // file.read_to_end(&mut data)?;
    // let torrent = parse_torrent(&data)?;

    // println!("{torrent:#?}");

    println!("{}", torrent.magnet_link());

    Ok(())
}
