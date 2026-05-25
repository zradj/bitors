use std::{error::Error, fs::File};

use bitors::TorrentFactory;

fn main() -> Result<(), Box<dyn Error>> {
    let torrent = TorrentFactory::new()
        .add_announce_url("http://127.0.0.1:6969/announce".parse()?)
        .add_path("../../Telegram")?
        .build()?;

    let mut file = File::create("test.torrent")?;
    torrent.to_bencode().encode_to_writer(&mut file)?;

    Ok(())
}
