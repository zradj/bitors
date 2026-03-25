use std::{fs::File, io::Read};

use bitors::{error::Error, parse::BencodeValue};

fn main() -> Result<(), Error> {
    let mut f = File::open("test.torrent").expect("file should open");
    let mut content = vec![];
    f.read_to_end(&mut content).expect("file should be read");
    let bencode = BencodeValue::parse(&content, &mut 0);
    println!("{bencode:?}");
    Ok(())
}
