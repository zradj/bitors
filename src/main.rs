use std::{fs::File, io::Read};

use bitors::{error::Error, bencode::Parser};

fn main() -> Result<(), Error> {
    let mut f = File::open("test.torrent").expect("file should open");
    let mut content = vec![];
    f.read_to_end(&mut content).expect("file should be read");
    let mut parser = Parser::new(&content);
    let result = parser.parse()?;
    println!("{result:?}");
    Ok(())
}
