use std::{
    collections::BTreeMap,
    io::{self, Write},
};

use thiserror::Error;

use crate::torrent::{self, FileInfo, FileMode, Info, Torrent};

/// Represents a parsed Bencode value.
///
/// Bencode is the encoding format used by the BitTorrent protocol.
/// It supports four data types: integers, byte strings, lists, and dictionaries.
///
/// The lifetime parameter `'a` ties byte slices and dictionary keys back to the
/// original input buffer, allowing zero-copy parsing — no data is copied out of
/// the source slice during parsing.
///
/// # Examples
///
/// Parsing a Bencode integer:
/// ```
/// use bitors::bencode::Parser;
///
/// let mut parser = Parser::new(b"i42e");
/// let value = parser.parse().unwrap();
/// assert_eq!(value.as_int().unwrap(), 42);
/// ```
///
/// Encoding a value to bytes:
/// ```
/// use bitors::bencode::Bencode;
///
/// let value = Bencode::Int(42);
/// assert_eq!(value.encode(), b"i42e");
/// ```
#[derive(Debug)]
pub enum Bencode<'a> {
    /// A 64-bit signed integer.
    Int(i64),
    /// A string of raw bytes, borrowed from the source buffer.
    Bytes(&'a [u8]),
    /// An ordered list of Bencode values.
    List(Vec<Bencode<'a>>),
    /// A dictionary mapping byte-string keys to Bencode values.
    ///
    /// The Bencode specification requires dictionary keys to be lexicographically
    /// sorted, so [`BTreeMap`] is used to uphold that invariant automatically
    /// during encoding.
    Dict(BTreeMap<&'a [u8], Bencode<'a>>),
}

impl<'a> Bencode<'a> {
    /// Attempts to interpret this value as a [`Torrent`].
    ///
    /// This is a convenience wrapper around the [`TryInto`] implementation for
    /// `Torrent`. The value must be a `Bencode::Dict` whose structure matches the
    /// BitTorrent metainfo file format.
    ///
    /// # Errors
    /// Returns a [`torrent::Error`] if the value cannot be interpreted as a
    /// valid `Torrent`.
    pub fn try_to_torrent(&self) -> Result<Torrent<'_>, torrent::Error> {
        self.try_into()
    }

    /// Attempts to interpret this value as an [`Info`] dictionary.
    ///
    /// This is a convenience wrapper around the [`TryInto`] implementation for
    /// `Info`. The value must be a `Bencode::Dict` containing the required
    /// info-dictionary fields.
    ///
    /// # Errors
    /// Returns a [`torrent::Error`] if the value cannot be interpreted as a
    /// valid `Info`.
    pub fn try_to_info(&self) -> Result<Info<'_>, torrent::Error> {
        self.try_into()
    }

    /// Attempts to interpret this value as a [`FileInfo`] entry.
    ///
    /// This is a convenience wrapper around the [`TryInto`] implementation for
    /// `FileInfo`. The value must be a `Bencode::Dict` containing the required
    /// file-entry fields.
    ///
    /// # Errors
    /// Returns a [`torrent::Error`] if the value cannot be interpreted as a
    /// valid `FileInfo`.
    pub fn try_to_file_info(&self) -> Result<FileInfo<'_>, torrent::Error> {
        self.try_into()
    }

    /// Attempts to unwrap the value as an integer.
    ///
    /// # Errors
    /// Returns [`Error::WrongType`] if the variant is not `Bencode::Int`.
    pub fn as_int(&self) -> Result<i64, Error> {
        match self {
            Self::Int(i) => Ok(*i),
            _ => Err(Error::WrongType { expected: "int" }),
        }
    }

    /// Attempts to unwrap the value as a byte slice.
    ///
    /// # Errors
    /// Returns [`Error::WrongType`] if the variant is not `Bencode::Bytes`.
    pub fn as_bytes(&self) -> Result<&[u8], Error> {
        match self {
            Self::Bytes(b) => Ok(b),
            _ => Err(Error::WrongType { expected: "bytes" }),
        }
    }

    /// Attempts to unwrap the value as a list.
    ///
    /// # Errors
    /// Returns [`Error::WrongType`] if the variant is not `Bencode::List`.
    pub fn as_list(&self) -> Result<&[Bencode<'a>], Error> {
        match self {
            Self::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    /// Attempts to unwrap the value as a dictionary.
    ///
    /// # Errors
    /// Returns [`Error::WrongType`] if the variant is not `Bencode::Dict`.
    pub fn as_dict(&self) -> Result<&BTreeMap<&[u8], Bencode<'a>>, Error> {
        match self {
            Self::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    /// Attempts to unwrap the value as a UTF-8 string.
    ///
    /// # Errors
    /// Returns [`Error::WrongType`] if the variant is not `Bencode::Bytes`,
    /// or [`Error::InvalidUtf8`] if the bytes are not valid UTF-8.
    pub fn as_str(&self) -> Result<&str, Error> {
        let bytes = self.as_bytes()?;
        Ok(std::str::from_utf8(bytes)?)
    }

    /// Encodes this value into a freshly allocated byte vector.
    ///
    /// The buffer is pre-sized using [`encoded_len`](Self::encoded_len) to avoid
    /// reallocations. Prefer [`encode_extend`](Self::encode_extend) when appending
    /// into an existing buffer, or [`encode_to_writer`](Self::encode_to_writer)
    /// when writing directly to an I/O sink.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.encoded_len());
        self.encode_to_writer(&mut buf)
            .expect("Writing to Vec should not fail");
        buf
    }

    /// Encodes this value and appends the result to an existing byte vector.
    ///
    /// Reserves exactly [`encoded_len`](Self::encoded_len) additional bytes in
    /// `buf` before writing, so no further reallocations occur. Useful when
    /// building up a larger buffer incrementally from multiple values.
    pub fn encode_extend(&self, buf: &mut Vec<u8>) {
        buf.reserve_exact(self.encoded_len());
        self.encode_to_writer(buf)
            .expect("Writing to Vec should not fail");
    }

    /// Encodes this value and writes the result to `writer`.
    ///
    /// This is the most general encoding entry point — it works with any type
    /// that implements [`Write`], including files, network streams, and
    /// in-memory buffers. All other encoding methods delegate here.
    ///
    /// # Errors
    /// Propagates any [`io::Error`] returned by `writer`.
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

    /// Returns the exact number of bytes this value will occupy when encoded.
    ///
    /// Used internally to pre-allocate buffers in [`encode`](Self::encode) and
    /// [`encode_extend`](Self::encode_extend), avoiding reallocations. The
    /// calculation is exact: calling `encode().len()` will always equal this
    /// value.
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

/// Returns the encoded byte length of a Bencode integer (`i<n>e`).
///
/// Accounts for the `i` prefix, the `e` suffix, an optional leading `-` for
/// negative values, and the number of decimal digits in the absolute value.
fn encoded_int_len(i: i64) -> usize {
    if i == 0 {
        // i0e
        3
    } else if i < 0 {
        // i-<abs>e
        3 + (1 + i.unsigned_abs().ilog10() as usize)
    } else {
        // i<num>e
        2 + (1 + i.ilog10() as usize)
    }
}

/// Returns the encoded byte length of a Bencode byte string (`<len>:<bytes>`).
///
/// Accounts for the decimal digit count of `byte_len`, the `:` separator, and
/// the payload bytes themselves.
fn encoded_bytes_len(byte_len: usize) -> usize {
    let len_str_len = if byte_len == 0 {
        1
    } else {
        1 + byte_len.ilog10() as usize
    };

    // <len>:<bytes>
    len_str_len + byte_len + 1
}

/// Converts a [`Torrent`] reference into its Bencode representation.
///
/// Only fields that are `Some` are included in the output dictionary; absent
/// optional fields are omitted entirely, as required by the BitTorrent
/// metainfo specification.
///
/// The resulting `Bencode` borrows data from `torrent` for the lifetime `'a`,
/// so no string or byte data is copied.
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

        if let Some(creation_date) = torrent.creation_date {
            map.insert(b"creation date", Self::Int(creation_date as i64));
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

        Self::Dict(map)
    }
}

/// Converts an [`Info`] reference into its Bencode representation.
///
/// Encodes the mandatory fields (`name`, `piece length`, `pieces`) and then
/// branches on [`FileMode`] to emit either a single-file `length` field or a
/// multi-file `files` list.  The `private` flag is only emitted when `true`,
/// and `md5sum` is only emitted when present, both per the specification.
///
/// The resulting `Bencode` borrows all string and byte data from `info`.
impl<'a> From<&'a Info<'a>> for Bencode<'a> {
    fn from(info: &'a Info<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

        map.insert(b"name", Self::Bytes(info.name.as_bytes()));
        map.insert(b"piece length", Self::Int(info.piece_length as i64));
        map.insert(b"pieces", Self::Bytes(info.pieces.as_flattened()));

        if info.private {
            map.insert(b"private", Self::Int(1));
        }

        match &info.file_mode {
            FileMode::Single { length, md5sum } => {
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

        Self::Dict(map)
    }
}

/// Converts a [`FileInfo`] reference into its Bencode representation.
///
/// Encodes the file's byte length and its path components as a Bencode list of
/// byte strings. The optional `md5sum` is included only when present.
///
/// The resulting `Bencode` borrows all string data from `file_info`.
impl<'a> From<&'a FileInfo<'a>> for Bencode<'a> {
    fn from(file_info: &'a FileInfo<'a>) -> Self {
        let mut map: BTreeMap<&[u8], Bencode<'_>> = BTreeMap::new();

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

/// A zero-copy parser for Bencode-encoded byte slices.
///
/// `Parser` holds a reference to the source buffer and advances an internal
/// cursor as it consumes input. All [`Bytes`](Bencode::Bytes) values and
/// [`Dict`](Bencode::Dict) keys returned by [`parse`](Parser::parse) borrow
/// directly from that buffer — nothing is copied.
///
/// Recursive structures (lists and dicts) are subject to a configurable depth
/// limit to guard against stack overflow on adversarial or malformed input.
/// The default limit is **64 levels**; use [`with_max_depth`](Parser::with_max_depth)
/// to override it.
///
/// # Example
///
/// ```
/// use bitors::bencode::Parser;
///
/// let mut parser = Parser::new(b"d3:fooi42ee");
/// let value = parser.parse().unwrap();
/// let dict = value.as_dict().unwrap();
/// assert_eq!(dict[b"foo".as_ref()].as_int().unwrap(), 42);
/// ```
#[derive(Debug)]
pub struct Parser<'a> {
    /// The raw input buffer being parsed.
    data: &'a [u8],
    /// Offset of the next byte to be examined.
    cursor: usize,
    /// Maximum nesting depth before [`Error::DepthLimitExceeded`] is returned.
    max_depth: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new `Parser` with the default maximum nesting depth of 64.
    ///
    /// Use [`with_max_depth`](Self::with_max_depth) if you need a different limit.
    pub fn new(data: &'a [u8]) -> Self {
        Self::with_max_depth(data, 64)
    }

    /// Creates a new `Parser` with an explicit maximum nesting depth.
    ///
    /// Nesting depth is incremented each time the parser enters a list or
    /// dictionary, and checked before recursing further. Setting `max_depth`
    /// to `0` prevents any list or dictionary from being parsed.
    pub fn with_max_depth(data: &'a [u8], max_depth: usize) -> Self {
        Self {
            data,
            cursor: 0,
            max_depth,
        }
    }

    /// Parses a single Bencode element starting at the current cursor position.
    ///
    /// After a successful call the cursor is advanced past the consumed bytes,
    /// so successive calls to `parse` can be used to read a stream of
    /// top-level values from the same buffer.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if the data is malformed, unexpectedly truncated,
    /// or exceeds the configured depth limit.
    pub fn parse(&mut self) -> Result<Bencode<'a>, Error> {
        self.parse_internal(0)
    }

    /// Returns a raw slice of the input data from `start` up to (but not
    /// including) the current cursor position.
    ///
    /// Intended for callers that need access to the exact bytes consumed by a
    /// preceding [`parse`](Self::parse) call — for example, to hash the
    /// `info` dictionary of a torrent file without re-encoding it.
    ///
    /// # Panics
    ///
    /// Panics if `start` is greater than the current cursor position, or if
    /// either index is out of bounds for the source buffer.
    pub fn raw_span(&self, start: usize) -> &'a [u8] {
        &self.data[start..self.cursor]
    }

    /// Returns the current byte offset of the parser's cursor within the
    /// source buffer.
    ///
    /// Useful in combination with [`raw_span`](Self::raw_span) to capture
    /// the start position before a `parse` call and then retrieve the exact
    /// bytes it consumed.
    pub fn position(&self) -> usize {
        self.cursor
    }

    /// Peeks at the next byte without advancing the cursor.
    fn peek(&self) -> Result<u8, Error> {
        self.data
            .get(self.cursor)
            .copied()
            .ok_or(Error::UnexpectedEof)
    }

    /// Peeks at a slice of `len` bytes starting at the cursor without advancing it.
    fn peek_slice(&self, len: usize) -> Result<&'a [u8], Error> {
        self.data
            .get(self.cursor..self.cursor + len)
            .ok_or(Error::UnexpectedEof)
    }

    /// Internal recursive entry point shared by [`parse`](Self::parse),
    /// `parse_list`, and `parse_dict`.
    ///
    /// `depth` is the current nesting level; it is compared against
    /// `max_depth` before recursing into any nested structure.
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

    /// Parses a Bencode integer (`i<number>e`).
    ///
    /// Rejects negative zero (`i-0e`) and any representation with leading zeros
    /// (e.g. `i03e`), as required by the Bencode specification.
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

        let i = s.parse::<i64>()?;
        self.cursor += end + 1;

        Ok(Bencode::Int(i))
    }

    /// Parses a Bencode byte string (`<length>:<contents>`).
    ///
    /// The length prefix must not have leading zeros. The returned
    /// [`Bencode::Bytes`] variant borrows its data directly from the source
    /// buffer.
    fn parse_bytes(&mut self) -> Result<Bencode<'a>, Error> {
        let colon = self.data[self.cursor..]
            .iter()
            .position(|&b| b == b':')
            .ok_or(Error::UnexpectedEof)?;
        let len_str = std::str::from_utf8(&self.data[self.cursor..self.cursor + colon])?;

        if len_str.starts_with('0') && len_str.len() > 1 {
            return Err(Error::InvalidBencodeInteger(len_str.to_string()));
        }

        let len = len_str.parse::<usize>()?;
        self.cursor += colon + 1;
        let bytes = self.peek_slice(len)?;
        self.cursor += len;

        Ok(Bencode::Bytes(bytes))
    }

    /// Parses a Bencode list (`l<contents>e`).
    ///
    /// Elements are parsed recursively at `depth + 1`. Parsing stops when the
    /// `e` terminator is found.
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

    /// Parses a Bencode dictionary (`d<contents>e`).
    ///
    /// Keys must be byte strings and must appear in strict lexicographic
    /// ascending order; duplicate or out-of-order keys are rejected with
    /// [`Error::UnsortedDictKeys`]. Values are parsed recursively at
    /// `depth + 1`.
    fn parse_dict(&mut self, depth: usize) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let mut map = BTreeMap::new();
        let mut last_key = None;

        while self.peek()? != b'e' {
            let key = match self.parse_internal(depth + 1)? {
                Bencode::Bytes(b) => b,
                _ => return Err(Error::NonStringKey),
            };

            if let Some(prev) = last_key {
                if key <= prev {
                    return Err(Error::UnsortedDictKeys);
                }
            }

            last_key = Some(key);

            let value = self.parse_internal(depth + 1)?;
            map.insert(key, value);
        }
        self.cursor += 1;

        Ok(Bencode::Dict(map))
    }
}

/// Errors that can occur during Bencode parsing or type coercion.
#[derive(Debug, Error)]
pub enum Error {
    /// The data contained bytes that are not valid UTF-8 where a UTF-8 string
    /// was expected (e.g. inside an integer token or when calling
    /// [`Bencode::as_str`]).
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// An integer token was syntactically valid UTF-8 but could not be parsed
    /// as an `i64` (e.g. the value is out of range).
    #[error("Integer parsing error: {0}")]
    InvalidInteger(#[from] std::num::ParseIntError),
    /// An integer token violated Bencode formatting rules: leading zeros
    /// (e.g. `i03e`) and negative zero (`i-0e`) are both forbidden.
    #[error("Invalid Bencode integer representation: {0}")]
    InvalidBencodeInteger(String),
    /// An unrecognised byte was encountered at the given offset. The second
    /// field is the unexpected byte value.
    #[error("Unexpected byte at position {0}: {1}")]
    UnexpectedByte(usize, u8),
    /// The input contained more nesting levels than the parser's configured
    /// `max_depth`. See [`Parser::with_max_depth`].
    #[error("Depth limit exceeded")]
    DepthLimitExceeded,
    /// The input ended before a complete Bencode value could be read.
    #[error("Unexpected EOF")]
    UnexpectedEof,
    /// A dictionary's keys were not in strict lexicographic ascending order,
    /// violating the Bencode specification.
    #[error("Unsorted dict keys")]
    UnsortedDictKeys,
    /// A dictionary key was not a byte string. The Bencode specification
    /// requires all dictionary keys to be byte strings.
    #[error("Keys of Bencode dictionaries must be strings")]
    NonStringKey,
    /// A [`Bencode`] accessor method (e.g. [`as_int`](Bencode::as_int)) was
    /// called on a value of a different variant.
    #[error("Wrong Bencode type, expected {expected}")]
    WrongType {
        /// A short description of the expected type (e.g. `"int"`, `"bytes"`).
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
