use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use thiserror::Error;

use crate::torrent::{
    FileInfo, FileLeaf, FileMode, FileTree, FileTreeNode, Info, Torrent, TorrentV2Ext,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Bencode<'a> {
    Int(i64),

    Bytes(&'a [u8]),

    List(Vec<Bencode<'a>>),

    Dict(BTreeMap<&'a [u8], Bencode<'a>>),
}

impl<'a> Bencode<'a> {
    pub fn as_int(&self) -> Result<i64, Error> {
        match self {
            Self::Int(i) => Ok(*i),
            _ => Err(Error::WrongType { expected: "int" }),
        }
    }

    pub fn as_bytes(&self) -> Result<&'a [u8], Error> {
        match self {
            Self::Bytes(b) => Ok(b),
            _ => Err(Error::WrongType { expected: "bytes" }),
        }
    }

    pub fn as_list(&self) -> Result<&[Bencode<'a>], Error> {
        match self {
            Self::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    pub fn as_dict(&self) -> Result<&BTreeMap<&[u8], Bencode<'a>>, Error> {
        match self {
            Self::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    pub fn as_str(&self) -> Result<&'a str, Error> {
        let bytes = self.as_bytes()?;
        Ok(std::str::from_utf8(bytes)?)
    }

    pub fn into_list(self) -> Result<Vec<Bencode<'a>>, Error> {
        match self {
            Self::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    pub fn into_dict(self) -> Result<BTreeMap<&'a [u8], Bencode<'a>>, Error> {
        match self {
            Self::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.encoded_len());
        #[expect(clippy::missing_panics_doc, reason = "infallible")]
        self.encode_to_writer(&mut buf)
            .expect("Writing to Vec should not fail");
        buf
    }

    pub fn encode_extend(&self, buf: &mut Vec<u8>) {
        buf.reserve_exact(self.encoded_len());
        #[expect(clippy::missing_panics_doc, reason = "infallible")]
        self.encode_to_writer(buf)
            .expect("Writing to Vec should not fail");
    }

    pub fn encode_to_writer<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Int(i) => write!(writer, "i{i}e")?,
            Self::Bytes(bytes) => {
                write!(writer, "{}:", bytes.len())?;
                writer.write_all(bytes)?;
            }
            Self::List(list) => {
                writer.write_all(b"l")?;
                for item in list {
                    item.encode_to_writer(writer)?;
                }
                writer.write_all(b"e")?;
            }
            Self::Dict(dict) => {
                writer.write_all(b"d")?;
                for (k, v) in dict {
                    Self::Bytes(k).encode_to_writer(writer)?;
                    v.encode_to_writer(writer)?;
                }
                writer.write_all(b"e")?;
            }
        }

        Ok(())
    }

    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Int(i) => encoded_int_len(*i),
            Self::Bytes(b) => encoded_bytes_len(b.len()),
            Self::List(l) => 2 + l.iter().map(Self::encoded_len).sum::<usize>(),
            Self::Dict(d) => {
                2 + d
                    .iter()
                    .map(|(k, v)| encoded_bytes_len(k.len()) + v.encoded_len())
                    .sum::<usize>()
            }
        }
    }
}

fn encoded_int_len(i: i64) -> usize {
    match i {
        // i0e
        0 => 3,
        // i-<abs>e
        n if n < 0 => 3 + (1 + n.unsigned_abs().ilog10() as usize),
        // i<num>e
        n => 2 + (1 + n.ilog10() as usize),
    }
}

fn encoded_bytes_len(byte_len: usize) -> usize {
    let len_str_len = if byte_len == 0 {
        1
    } else {
        1 + byte_len.ilog10() as usize
    };

    // <len>:<bytes>
    len_str_len + byte_len + 1
}

impl<'a> From<&'a Torrent<'a>> for Bencode<'a> {
    fn from(torrent: &'a Torrent) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        map.insert(b"info", (&torrent.info).into());

        if let Some(url) = &torrent.announce {
            map.insert(b"announce", Self::Bytes(url.as_str().as_bytes()));
        }

        if let Some(announce_list) = &torrent.announce_list {
            let announce_list = announce_list
                .iter()
                .map(|v| {
                    let urls = v
                        .iter()
                        .map(|url| Self::Bytes(url.as_str().as_bytes()))
                        .collect();
                    Self::List(urls)
                })
                .collect();

            map.insert(b"announce-list", Self::List(announce_list));
        }

        if let Some(url_list) = &torrent.url_list {
            let url_list = url_list
                .iter()
                .map(|url| Self::Bytes(url.as_str().as_bytes()))
                .collect();

            map.insert(b"url-list", Self::List(url_list));
        }

        if let Some(creation_date) = torrent.creation_date {
            map.insert(
                b"creation date",
                Self::Int(creation_date.try_into().unwrap_or(0)),
            );
        }

        if let Some(comment) = &torrent.comment {
            map.insert(b"comment", Self::Bytes(comment.as_bytes()));
        }

        if let Some(created_by) = &torrent.created_by {
            map.insert(b"created by", Self::Bytes(created_by.as_bytes()));
        }

        if let Some(encoding) = &torrent.encoding {
            map.insert(b"encoding", Self::Bytes(encoding.as_bytes()));
        }

        if let Some(v2_ext) = &torrent.v2_ext {
            map.insert(b"piece layers", v2_ext.into());
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a TorrentV2Ext<'a>> for Bencode<'a> {
    fn from(ext: &'a TorrentV2Ext<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        for (k, v) in &ext.piece_layers {
            map.insert(k.as_ref(), Self::Bytes(v));
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a Info<'a>> for Bencode<'a> {
    fn from(info: &'a Info<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        map.insert(b"name", Self::Bytes(info.name.as_bytes()));
        // Lengths are `u64`; Bencode integers are `i64`. A file larger than
        // i64::MAX (≈ 9.2 EB) cannot be represented, but no real torrent
        // approaches that size.
        #[allow(clippy::cast_possible_wrap)]
        map.insert(b"piece length", Self::Int(info.piece_length.get() as i64));

        if info.private {
            map.insert(b"private", Self::Int(1));
        }

        if let Some(source) = &info.source {
            map.insert(b"source", Self::Bytes(source.as_bytes()));
        }

        if let Some(v1) = &info.v1 {
            map.insert(b"pieces", Self::Bytes(v1.pieces.as_flattened()));

            match &v1.file_mode {
                FileMode::Single { length, md5sum } => {
                    // Lengths are `u64`; Bencode integers are `i64`. A file larger than
                    // i64::MAX (≈ 9.2 EB) cannot be represented, but no real torrent
                    // approaches that size.
                    #[allow(clippy::cast_possible_wrap)]
                    map.insert(b"length", Self::Int(*length as i64));

                    if let Some(md5sum) = md5sum {
                        map.insert(b"md5sum", Self::Bytes(md5sum.as_bytes()));
                    }
                }
                FileMode::Multi { files } => {
                    let files = files.iter().map(Bencode::from).collect();
                    map.insert(b"files", Self::List(files));
                }
            }
        }

        if let Some(v2) = &info.v2 {
            map.insert(b"meta version", Self::Int(i64::from(v2.meta_version)));
            map.insert(b"file tree", (&v2.file_tree).into());
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a FileInfo<'a>> for Bencode<'a> {
    fn from(file_info: &'a FileInfo<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        if let Some(attr) = &file_info.attr {
            map.insert(b"attr", Self::Bytes(attr.as_bytes()));
        }

        // Lengths are `u64`; Bencode integers are `i64`. A file larger than
        // i64::MAX (≈ 9.2 EB) cannot be represented, but no real torrent
        // approaches that size.
        #[allow(clippy::cast_possible_wrap)]
        map.insert(b"length", Self::Int(file_info.length as i64));

        let path: Vec<Self> = file_info
            .path
            .iter()
            .map(|s| Self::Bytes(s.as_bytes()))
            .collect();
        map.insert(b"path", Self::List(path));

        if let Some(md5sum) = &file_info.md5sum {
            map.insert(b"md5sum", Self::Bytes(md5sum.as_bytes()));
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a FileTree<'a>> for Bencode<'a> {
    fn from(tree: &'a FileTree<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        for (k, v) in &tree.0 {
            map.insert(k.as_bytes(), v.into());
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a FileTreeNode<'a>> for Bencode<'a> {
    fn from(node: &'a FileTreeNode<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        match node {
            FileTreeNode::Directory(file_tree) => {
                for (k, v) in &file_tree.0 {
                    map.insert(k.as_bytes(), v.into());
                }
            }
            FileTreeNode::File(file_leaf) => {
                map.insert(b"", file_leaf.into());
            }
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a FileLeaf<'a>> for Bencode<'a> {
    fn from(leaf: &'a FileLeaf<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        map.insert(b"length", Self::Int(leaf.length as i64));

        if let Some(pieces_root) = &leaf.pieces_root {
            map.insert(b"pieces root", Self::Bytes(pieces_root.as_ref()));
        }

        Self::Dict(map)
    }
}

#[derive(Debug)]
pub struct Parser<'a> {
    data: &'a [u8],

    cursor: usize,

    max_depth: usize,
}

impl<'a> Parser<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self::with_max_depth(data, 64)
    }

    #[must_use]
    pub fn with_max_depth(data: &'a [u8], max_depth: usize) -> Self {
        Self {
            data,
            cursor: 0,
            max_depth,
        }
    }

    pub fn parse(&mut self) -> Result<Bencode<'a>, Error> {
        self.parse_internal(0)
    }

    fn peek(&self) -> Result<u8, Error> {
        self.data
            .get(self.cursor)
            .copied()
            .ok_or(Error::UnexpectedEof)
    }

    fn peek_slice(&self, len: usize) -> Result<&'a [u8], Error> {
        self.data
            .get(self.cursor..self.cursor + len)
            .ok_or(Error::UnexpectedEof)
    }

    fn parse_internal(&mut self, depth: usize) -> Result<Bencode<'a>, Error> {
        if depth > self.max_depth {
            return Err(Error::DepthLimitExceeded);
        }

        match self.peek()? {
            b'i' => self.parse_integer(),
            b'l' => self.parse_list(depth),
            b'd' => self.parse_dict(depth),
            b'0'..=b'9' => self.parse_bytes(),
            b => Err(Error::UnexpectedByte(self.cursor, b)),
        }
    }

    fn parse_integer(&mut self) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let end = self.data[self.cursor..]
            .iter()
            .position(|&b| b == b'e')
            .ok_or(Error::UnexpectedEof)?;
        let s = std::str::from_utf8(&self.data[self.cursor..self.cursor + end])?;

        if s.starts_with("-0") || (s.starts_with('0') && s.len() > 1) {
            return Err(Error::InvalidBencodeInteger(s.to_string()));
        }

        let i = s.parse()?;
        self.cursor += end + 1;

        Ok(Bencode::Int(i))
    }

    fn parse_bytes(&mut self) -> Result<Bencode<'a>, Error> {
        let colon = self.data[self.cursor..]
            .iter()
            .position(|&b| b == b':')
            .ok_or(Error::UnexpectedEof)?;
        let len_str = std::str::from_utf8(&self.data[self.cursor..self.cursor + colon])?;

        if len_str.starts_with('0') && len_str.len() > 1 {
            return Err(Error::InvalidBencodeInteger(len_str.to_string()));
        }

        let len = len_str.parse()?;
        self.cursor += colon + 1;
        let bytes = self.peek_slice(len)?;
        self.cursor += len;

        Ok(Bencode::Bytes(bytes))
    }

    fn parse_list(&mut self, depth: usize) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let mut list = vec![];
        while self.peek()? != b'e' {
            let item = self.parse_internal(depth + 1)?;
            list.push(item);
        }
        self.cursor += 1;

        Ok(Bencode::List(list))
    }

    fn parse_dict(&mut self, depth: usize) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let mut map = BTreeMap::new();
        let mut last_key = None;

        while self.peek()? != b'e' {
            let Bencode::Bytes(key) = self.parse_internal(depth + 1)? else {
                return Err(Error::NonStringKey);
            };

            if let Some(prev) = last_key
                && key <= prev
            {
                return Err(Error::UnsortedDictKeys);
            }

            last_key = Some(key);

            let value = self.parse_internal(depth + 1)?;
            map.insert(key, value);
        }
        self.cursor += 1;

        Ok(Bencode::Dict(map))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    #[error("Integer parsing error: {0}")]
    InvalidInteger(#[from] std::num::ParseIntError),

    #[error("Invalid Bencode integer representation: {0}")]
    InvalidBencodeInteger(String),

    #[error("Unexpected byte at position {0}: {1}")]
    UnexpectedByte(usize, u8),

    #[error("Depth limit exceeded")]
    DepthLimitExceeded,

    #[error("Unexpected EOF")]
    UnexpectedEof,

    #[error("Unsorted dict keys")]
    UnsortedDictKeys,

    #[error("Keys of Bencode dictionaries must be strings")]
    NonStringKey,

    #[error("Wrong Bencode type, expected {expected}")]
    WrongType { expected: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_integer() {
        assert_eq!(Parser::new(b"i42e").parse().unwrap().as_int().unwrap(), 42);
        assert_eq!(
            Parser::new(b"i-42e").parse().unwrap().as_int().unwrap(),
            -42
        );
        assert_eq!(Parser::new(b"i0e").parse().unwrap().as_int().unwrap(), 0);
    }

    #[test]
    fn test_invalid_integers() {
        // Leading zeros are invalid in Bencode
        assert!(matches!(
            Parser::new(b"i03e").parse(),
            Err(Error::InvalidBencodeInteger(_))
        ));
        // Negative zero is invalid
        assert!(matches!(
            Parser::new(b"i-0e").parse(),
            Err(Error::InvalidBencodeInteger(_))
        ));
        // Missing numbers
        assert!(Parser::new(b"ie").parse().is_err());
        assert!(Parser::new(b"i-e").parse().is_err());
    }

    #[test]
    fn test_parse_bytes() {
        let mut parser = Parser::new(b"4:spam");
        let val = parser.parse().unwrap();
        assert_eq!(val.as_bytes().unwrap(), b"spam");
        assert_eq!(val.as_str().unwrap(), "spam");

        // Empty string
        let mut parser = Parser::new(b"0:");
        let val = parser.parse().unwrap();
        assert_eq!(val.as_bytes().unwrap(), b"");
    }

    #[test]
    fn test_invalid_bytes() {
        // Truncated data
        assert!(matches!(
            Parser::new(b"4:spa").parse(),
            Err(Error::UnexpectedEof)
        ));
        // Missing colon
        assert!(Parser::new(b"4spam").parse().is_err());
    }

    #[test]
    fn test_parse_list() {
        let mut parser = Parser::new(b"li42e4:spame");
        let val = parser.parse().unwrap();
        let list = val.as_list().unwrap();

        assert_eq!(list.len(), 2);
        assert_eq!(list[0].as_int().unwrap(), 42);
        assert_eq!(list[1].as_str().unwrap(), "spam");

        // Empty list
        let mut parser = Parser::new(b"le");
        let val = parser.parse().unwrap();
        assert!(val.as_list().unwrap().is_empty());
    }

    #[test]
    fn test_parse_dict() {
        let mut parser = Parser::new(b"d3:bar4:spam3:fooi42ee");
        let val = parser.parse().unwrap();
        let dict = val.as_dict().unwrap();

        assert_eq!(dict.len(), 2);
        assert_eq!(dict.get(&b"bar"[..]).unwrap().as_str().unwrap(), "spam");
        assert_eq!(dict.get(&b"foo"[..]).unwrap().as_int().unwrap(), 42);

        // Empty dict
        let mut parser = Parser::new(b"de");
        let val = parser.parse().unwrap();
        assert!(val.as_dict().unwrap().is_empty());
    }

    #[test]
    fn test_invalid_dict_keys() {
        // Dictionaries must have string/bytes keys (this uses an integer as a key)
        assert!(matches!(
            Parser::new(b"di42e4:spame").parse(),
            Err(Error::NonStringKey)
        ));
    }

    #[test]
    fn test_type_coercion_errors() {
        let val = Bencode::Int(42);
        assert!(matches!(
            val.as_str(),
            Err(Error::WrongType { expected: "bytes" })
        ));
        assert!(matches!(
            val.as_list(),
            Err(Error::WrongType { expected: "list" })
        ));
    }

    #[test]
    fn test_deeply_nested_structure() {
        // Parses `d1:ad1:bd1:ci42eeee` which translates to {"a": {"b": {"c": 42}}}
        let mut parser = Parser::new(b"d1:ad1:bd1:ci42eeee");
        let val = parser.parse().unwrap();

        let a = val.as_dict().unwrap().get(&b"a"[..]).unwrap();
        let b = a.as_dict().unwrap().get(&b"b"[..]).unwrap();
        let c = b.as_dict().unwrap().get(&b"c"[..]).unwrap();

        assert_eq!(c.as_int().unwrap(), 42);
    }

    #[test]
    fn test_depth_limit() {
        let mut parser = Parser::with_max_depth(b"lllleeee", 2);
        assert!(matches!(parser.parse(), Err(Error::DepthLimitExceeded)));
    }

    #[test]
    fn test_unsorted_dict() {
        assert!(matches!(
            Parser::new(b"d4:spam3:foo3:bari42ee").parse(),
            Err(Error::UnsortedDictKeys)
        ));
    }
}
