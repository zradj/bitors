use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use thiserror::Error;

use crate::torrent::{
    FileInfo, FileLeaf, FileMode, FileTree, FileTreeNode, Info, Torrent, TorrentV2Ext,
};

/// A zero-copy [Bencode](https://en.wikipedia.org/wiki/Bencode) element representation.
///
/// Bencode is the encoding used by BitTorrent. Four types of data can be represented by bencode:
/// 1. *Signed integers*: encoded in the form `i<num>e`.
/// 2. *Bytes*: encoded in the form `<length>:<bytes>`.
/// 3. *Lists*: encoded in the form `l<items>e`.
/// 4. *Dictionaries*: encoded in the form `d<key1><value1><key2>...e`. The keys must be
///    represented as [byte strings](`Bencode::Bytes`) and appear in sorted order.
///
/// [`Parser`] provides a way to produce [`Bencode`] from raw data.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Bencode<'a> {
    /// A signed 64-bit integer.
    Int(i64),
    /// A slice of bytes.
    Bytes(&'a [u8]),
    /// A list of other bencoded items.
    List(Vec<Bencode<'a>>),
    /// A dictionary mapping byte keys to bencoded items.
    Dict(BTreeMap<&'a [u8], Bencode<'a>>),
}

impl<'a> Bencode<'a> {
    /// Returns the 64-bit signed integer stored in this [`Bencode`] element.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::Int`].
    pub fn as_int(&self) -> Result<i64, Error> {
        match self {
            Self::Int(i) => Ok(*i),
            _ => Err(Error::WrongType { expected: "int" }),
        }
    }

    /// Returns the byte slice referenced in this [`Bencode`] element.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::Bytes`].
    pub fn as_bytes(&self) -> Result<&'a [u8], Error> {
        match self {
            Self::Bytes(b) => Ok(b),
            _ => Err(Error::WrongType { expected: "bytes" }),
        }
    }

    /// Returns a reference to the list stored in this [`Bencode`] element.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::List`].
    pub fn as_list(&self) -> Result<&[Bencode<'a>], Error> {
        match self {
            Self::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    /// Returns a reference to the dictionary stored in this [`Bencode`] element.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::Dict`].
    pub fn as_dict(&self) -> Result<&BTreeMap<&[u8], Bencode<'a>>, Error> {
        match self {
            Self::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    /// Converts the bytes referenced in this [`Bencode`] element to a string slice and
    /// returns it.
    ///
    /// # Errors
    ///
    /// - [`Error::WrongType`] if the underlying variant is not [`Bencode::Bytes`].
    /// - [`Error::InvalidUtf8`] if the bytes cannot be represented as a valid UTF-8 encoded
    ///   string.
    pub fn as_str(&self) -> Result<&'a str, Error> {
        let bytes = self.as_bytes()?;
        Ok(std::str::from_utf8(bytes)?)
    }

    /// Consumes this [`Bencode`] element and returns the list stored in it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::List`].
    pub fn into_list(self) -> Result<Vec<Bencode<'a>>, Error> {
        match self {
            Self::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    /// Consumes this [`Bencode`] element and returns the dictionary stored in it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongType`] if the underlying variant is not [`Bencode::Dict`].
    pub fn into_dict(self) -> Result<BTreeMap<&'a [u8], Bencode<'a>>, Error> {
        match self {
            Self::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    /// Encodes this [`Bencode`] element as a vector of bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.encoded_len());
        let _ = self.encode_to_writer(&mut buf);
        buf
    }

    /// Encodes this [`Bencode`] element by extending an existing vector of bytes.
    pub fn encode_extend(&self, buf: &mut Vec<u8>) {
        buf.reserve_exact(self.encoded_len());
        let _ = self.encode_to_writer(buf);
    }

    /// Encodes this [`Bencode`] element directly to a [writer](`Write`).
    ///
    /// # Errors
    ///
    /// Propagates any [`io::Error`] returned by the [writer](`Write`).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{error::Error, fs::File};
    /// # use bitors::{Torrent, bencode::Bencode};
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// let torrent = Torrent::builder().add_path("my_file")?.build()?;
    /// let mut file = File::create("my_file.torrent")?;
    ///
    /// torrent.to_bencode().encode_to_writer(&mut file)?;
    /// # Ok(())
    /// # }
    ///
    /// ```
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

    /// Calculates the exact length of the encoded form of this [`Bencode`] element in bytes.
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

/// A helper function that calculates the length of a [`Bencode::Int`] variant in its encoded form.
///
/// The length of the text representation of an integer is calculated mathematically. The formula for
/// a positive integer is as follows: `1 + floor(log10(num))`. The length of a negative number is calculated
/// similarly by taking the absolute value and accounting for the minus sign (+1 to the length). The length
/// of 0 is always 1.
///
/// The result of the previous calculation is then increased by 2 to account for the bencode format (`i<num>e`).
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

/// A helper function that calculates the length of a [`Bencode::Bytes`] variant in its encoded form.
///
/// The function first calculates the length of the text representation of the length of the byte slice (see
/// [`encoded_int_len`]). After that, it adds the length of the byte slice and 1 for the colon in the bencode
/// representation.
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
    /// Serializes a [`Torrent`] into its bencode representation.
    ///
    /// The output is a [`Bencode::Dict`] that contains the serialized [`info`](`Info`) dictionary as well as other
    /// optional fields from [`Torrent`] if set.
    ///
    /// The `piece layers` field (represented by [`TorrentV2Ext`]) is not included in v1-only torrents. See
    /// the `From<&TorrentV2Ext>` implementation on [`Bencode`] for more information.
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
    /// Serializes a [`TorrentV2Ext`] into its bencode representation.
    ///
    /// The output is a [`Bencode::Dict`] that contains a dictionary that maps
    /// v2 file hashes from the [`file tree`](`crate::torrent::InfoV2::file_tree`) field in the
    /// [`info`](`crate::torrent::InfoV2`) dictionary to the hashes from a certain layer of the Merkle hash tree
    /// of that file. See [BEP 52](https://www.bittorrent.org/beps/bep_0052.html) for more details.
    ///
    /// Not present in v1-only torrents.
    fn from(ext: &'a TorrentV2Ext<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        for (k, v) in &ext.piece_layers {
            map.insert(k.as_ref(), Self::Bytes(v));
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a Info<'a>> for Bencode<'a> {
    /// Serializes an [`Info`] into its bencode representation, the `info` dictionary.
    ///
    /// The output is a [`Bencode::Dict`]. The following keys are always present: [`name`](Info::name),
    /// [`piece length`](Info::piece_length). The `private` field is included (and set to 1) only if [`Info::private`] is `true`.
    ///
    /// Other fields are included only if set to [`Some`]. These include the [`source`](Info::source) field, as well as the
    /// following version-specific fields:
    ///
    /// - [`pieces`](crate::torrent::InfoV1::pieces) - always included in a v1 torrent.
    ///   - [`length`](FileMode::Single::length), [`md5sum`](FileMode::Single::md5sum) - included if the v1 torrent
    ///     consists of a single file.
    ///   - [`files`](FileMode::Multi::files) - included if the v1 torrent consists of multiple files. In this case,
    ///     a list of [file information items](FileInfo) is included. See the `From<&FileInfo>` implementation on [`Bencode`]
    ///     for more information.
    /// - [`meta version`](crate::torrent::InfoV2::meta_version), [`file tree`](crate::torrent::InfoV2::file_tree) -
    ///   always included in a v2 torrent. `meta version` is always set to 2. `file tree` contains the v2 file tree. See
    ///   the `From<&FileTree>` implementation of [`Bencode`] for more information.
    ///
    /// Hybrid torrents contain all of the fields from both of the options above.
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
    /// Serializes a [`FileInfo`] into its bencode representation.
    ///
    /// Since [`FileInfo`] represents an entry in the [`files`](FileMode::Multi::files) list,
    /// it is serialized as a [`Bencode::Dict`] with the fields [`length`](FileInfo::length)
    /// and [`path`](FileInfo::path). It may also optionally contain the fields [`md5sum`](FileInfo::md5sum)
    /// and [`attr`](FileInfo::attr). For more information on the latter, see
    /// [`FileInfoAttr::encoded`](`crate::torrent::FileInfoAttr::encoded`).
    ///
    /// Not present in v2-only torrents.
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
    /// Serializes a [`FileTree`] into its bencode representation, the `file tree` field,
    /// as per [BEP 52](https://www.bittorrent.org/beps/bep_0052.html#file-tree-layout).
    ///
    /// The output is a [`Bencode::Dict`] that contains one or more trees of path components.
    /// Each child, represented by [`FileTreeNode`], contains either another [`FileTree`] or
    /// a [`FileLeaf`] representation. See the `From<&FileTreeNode>` implementation on [`Bencode`]
    /// for more information.
    ///
    /// Not present in v1-only torrents.
    fn from(tree: &'a FileTree<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        for (k, v) in &tree.0 {
            map.insert(k.as_bytes(), v.into());
        }

        Self::Dict(map)
    }
}

impl<'a> From<&'a FileTreeNode<'a>> for Bencode<'a> {
    /// Serializes a [`FileTreeNode`] inside a [`FileTree`] into its bencode representation
    /// as per [BEP 52](https://www.bittorrent.org/beps/bep_0052.html#file-tree-layout).
    ///
    /// The output is a [`Bencode::Dict`] whose content depends on the variant of [`FileTreeNode`]:
    ///
    /// - **[`FileTreeNode::Directory`]** - all of [`FileTreeNode`] representations in the underlying [`FileTree`].
    /// - **[`FileTreeNode::File`]** - an entry with an empty key (`b""`) and the representation of the underlying
    ///   [`FileLeaf`] as the value. The file name is the concatenation of all path components leading to this empty key.
    ///   See the `From<&FileLeaf>` implementation on [`Bencode`] for more information.
    ///
    /// Not present in v1-only torrents.
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
    /// Serializes a [`FileLeaf`] inside a [`FileTreeNode`] into its bencode representation.
    ///
    /// The output is a [`Bencode::Dict`] that contains the [`length`](`FileLeaf::length`) field.
    /// It also contains the [`pieces root`](`FileLeaf::pieces_root`) field if the file is not empty.
    ///
    /// Not present in v1-only torrents.
    fn from(leaf: &'a FileLeaf<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        // Lengths are `u64`; Bencode integers are `i64`. A file larger than
        // i64::MAX (≈ 9.2 EB) cannot be represented, but no real torrent
        // approaches that size.
        #[allow(clippy::cast_possible_wrap)]
        map.insert(b"length", Self::Int(leaf.length as i64));

        if let Some(pieces_root) = &leaf.pieces_root {
            map.insert(b"pieces root", Self::Bytes(pieces_root.as_ref()));
        }

        Self::Dict(map)
    }
}

/// A zero-copy parser for raw bencoded data.
///
/// The parser enforces all bencode rules as described by [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding).
/// The parser also provides a defense mechanism against stack overflow attacks by tracking the nesting depth
/// and checking it against the maximum depth. The maximum depth is configurable using [`Parser::with_max_depth`].
///
/// There is also the shorthand function [`parse_torrent`] that creates a [`Torrent`] instance
/// from raw data without going through the intermediate steps.
///
/// [`parse_torrent`]: crate::parse_torrent
///
/// # Examples
///
/// Creation:
///
/// ```
/// # use bitors::Parser;
/// let mut parser = Parser::new(b"l4:cool6:parser2:at4:worke".as_slice());
/// let bencode = parser.parse().unwrap();
///
/// assert_eq!(bencode.encode(), b"l4:cool6:parser2:at4:worke");
/// ```
///
/// Torrent file parsing and creating a [`Torrent`] instance:
///
/// ```no_run
/// # use std::{fs::File, io::Read, error::Error};
/// # use bitors::{Parser, Torrent};
/// # fn main() -> Result<(), Box<dyn Error>> {
/// let mut file = File::open("my_torrent.torrent")?;
/// let mut data = vec![];
/// file.read_to_end(&mut data);
///
/// let mut parser = Parser::new(&data);
/// let torrent = Torrent::try_from(parser.parse()?)?;
///
/// // Do something else...
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Parser<'a> {
    /// The data this parser is parsing.
    data: &'a [u8],
    /// A pointer that tracks the parser's current position in the data.
    cursor: usize,
    /// The maximum nesting depth. Exceeding this depth will lead to [`Error::DepthLimitExceeded`].
    max_depth: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new [`Parser`] instance with the given data and the default maximum nesting depth of 64.
    ///
    /// The maximum nesting depth can be configured using the [`Parser::with_max_depth`] constructor.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self::with_max_depth(data, 64)
    }

    /// Creates a new [`Parser`] instance with the given data and maximum nesting depth.
    ///
    /// The maximum nesting depth provides defense against stack overflow attacks that use malicious
    /// torrent inputs (i.e. `<thousands of 'l's><thousands of 'e's>`).
    ///
    /// # Examples
    ///
    /// ```
    /// # use bitors::{Parser, bencode::Error};
    /// let data: &[u8] = b"llllllllleeeeeeeee"; // A nested list with the depth of 9
    /// let mut parser = Parser::with_max_depth(data, 8);
    /// let result = parser.parse();
    ///
    /// assert!(matches!(result, Err(Error::DepthLimitExceeded)));
    /// ```
    #[must_use]
    pub fn with_max_depth(data: &'a [u8], max_depth: usize) -> Self {
        Self {
            data,
            cursor: 0,
            max_depth,
        }
    }

    /// Parses the provided data and returns the resulting [`Bencode`] element.
    ///
    /// # Errors
    ///
    /// - [`Error::UnexpectedByte`] if the parser encountered an unexpected byte in the data.
    /// - [`Error::UnexpectedEof`] if the EOF (end-of-file) was reached prematurely.
    /// - [`Error::DepthLimitExceeded`] if the maximum nesting depth was exceeded during parsing.
    /// - [`Error::InvalidInteger`] if an integer could not be parsed by Rust.
    /// - [`Error::InvalidBencodeInteger`] if an integer violated the bencode format
    ///   (see [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding)). This happens
    ///   when an integer has a leading zero (`i01e`) or if a negative zero was provided
    ///   (`i-0e`).
    /// - [`Error::NonStringKey`] if a dictionary contained a non-string key (i.e. a key that is not [`Bencode::Bytes`]).
    ///   [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding) requires that all dictionary keys
    ///   be strings.
    /// - [`Error::UnsortedDictKeys`] if a dictionary's keys appeared in a non-sorted order. This is required by
    ///   [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding).
    pub fn parse(&mut self) -> Result<Bencode<'a>, Error> {
        self.parse_internal(1)
    }

    /// Peeks at the current byte in the data. Returns [`Error::UnexpectedEof`] if there is none.
    fn peek(&self) -> Result<u8, Error> {
        self.data
            .get(self.cursor)
            .copied()
            .ok_or(Error::UnexpectedEof)
    }

    /// Peeks at a slice of the given length at the current position in the data.
    /// Returns [`Error::UnexpectedEof`] if there is none.
    fn peek_slice(&self, len: usize) -> Result<&'a [u8], Error> {
        self.data
            .get(self.cursor..self.cursor + len)
            .ok_or(Error::UnexpectedEof)
    }

    /// An internal helper method that tracks the current nesting depth. For more information,
    /// see [`Parser::parse`].
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

    /// Parses an integer at the current position in the data.
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

    /// Extracts a byte string at the current position in the data.
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

    /// Parses a list of other bencoded items at the current position in the data. Increases the
    /// current nesting depth.
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

    /// Parses a dictionary at the current position in the data. Increases the
    /// current nesting depth.
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

/// Errors that can arise while parsing using [`Parser`] or working with [`Bencode`].
#[derive(Debug, Error)]
pub enum Error {
    /// A byte slice in [`Bencode`] cannot be converted to a UTF-8 encoded [`str`].
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// An expected integer could not be parsed by Rust.
    #[error("Integer parsing error: {0}")]
    InvalidInteger(#[from] std::num::ParseIntError),
    /// The raw representation of an integer violated the bencode format described in
    /// [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding). The integer either had
    /// a leading zero (`i01e`) or was a negative zero (`i-0e`).
    #[error("Invalid Bencode integer representation: {0}")]
    InvalidBencodeInteger(String),
    /// An unexpected byte was encountered at a certain position in the data during parsing.
    /// This means the bencode is malformed.
    #[error("Unexpected byte at position {0}: {1}")]
    UnexpectedByte(usize, u8),
    /// The maximum nesting depth limit was exceeded during parsing.
    #[error("Depth limit exceeded")]
    DepthLimitExceeded,
    /// The parser encountered an EOF (end-of-file) prematurely. This means the bencode is malformed.
    #[error("Unexpected EOF")]
    UnexpectedEof,
    /// The keys of a dictionary were not sorted. This is required by
    /// [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding).
    #[error("Unsorted dict keys")]
    UnsortedDictKeys,
    /// A key of a dictionary was not a byte string (i.e. [`Bencode::Bytes`]). This is required by
    /// [BEP 3](https://www.bittorrent.org/beps/bep_0003.html#bencoding).
    #[error("Keys of Bencode dictionaries must be strings")]
    NonStringKey,
    /// A [`Bencode`] accessor method (e.g. [`as_int`](Bencode::as_int)) was called on
    /// a value of a different variant.
    #[error("Wrong Bencode type, expected {expected}")]
    WrongType {
        /// A short description of the expected type (e.g. `"int"` or `"dict"`).
        expected: &'static str,
    },
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
