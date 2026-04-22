use std::{error::Error, fs::File, io::Read};

use bitors::{
    bencode::{Bencode, Parser},
    torrent::Torrent,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut f = File::open("test.torrent").expect("file should open");
    let mut content = vec![];
    f.read_to_end(&mut content).expect("file should be read");
    let mut parser = Parser::new(&content);
    let bencode = &parser.parse()?;
    let torrent: Torrent = bencode.try_into()?;
    println!("{torrent:?}");
    let mut new_torrent = File::create("new.torrent")?;
    let new_bencode = Bencode::from(&torrent);
    new_bencode.encode_to_writer(&mut new_torrent)?;
    Ok(())
}
