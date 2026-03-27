use url::Url;

#[derive(Debug)]
pub struct Torrent {
    pub announce: Option<Url>,
    pub announce_list: Option<Vec<Vec<Url>>>,
    pub info: Info,
    pub info_hash: [u8; 20],
}

#[derive(Debug)]
pub struct Info {
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<u8>,
    pub file_mode: FileMode,
}

#[derive(Debug)]
pub enum FileMode {
    Single { length: u64 },
    Multi { files: Vec<FileInfo> },
}

#[derive(Debug)]
pub struct FileInfo {
    pub length: u64,
    pub path: Vec<String>,
}
