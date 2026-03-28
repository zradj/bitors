use std::{fs::File, io::Read};

use bitors::{bencode::Parser, error::Error, torrent::Torrent};

fn main() -> Result<(), Error> {
    let mut f = File::open("test.torrent").expect("file should open");
    let mut content = vec![];
    f.read_to_end(&mut content).expect("file should be read");
    let mut parser = Parser::new(&content);
    let bencode = &parser.parse()?;
    let torrent: Torrent = bencode.try_into()?;
    println!("{torrent:?}");
    Ok(())
}
