use std::collections::BTreeMap;

use thiserror::Error;

/// Represents a parsed Bencode value.
///
/// Bencode is the encoding format used by the BitTorrent protocol. 
/// It supports four data types: integers, byte strings, lists, and dictionaries.
#[derive(Debug)]
pub enum Bencode<'a> {
    /// A 64-bit integer.
    Int(i64),
    /// A string of raw bytes.
    Bytes(&'a [u8]),
    /// A list of other Bencode values.
    List(Vec<Bencode<'a>>),
    /// A dictionary mapping byte-string keys to Bencode values.
    /// Keys are always lexicographically sorted in Bencode, hence the use of `BTreeMap`.
    Dict(BTreeMap<&'a [u8], Bencode<'a>>),
}

impl<'a> Bencode<'a> {
    /// Attempts to unwrap the value as an integer.
    ///
    /// # Errors
    /// Returns `Error::WrongType` if the variant is not `Bencode::Int`.
    pub fn as_int(&self) -> Result<i64, Error> {
        match self {
            Bencode::Int(i) => Ok(*i),
            _ => Err(Error::WrongType { expected: "int" }),
        }
    }

    /// Attempts to unwrap the value as a byte slice.
    ///
    /// # Errors
    /// Returns `Error::WrongType` if the variant is not `Bencode::Bytes`.
    pub fn as_bytes(&self) -> Result<&[u8], Error> {
        match self {
            Bencode::Bytes(b) => Ok(b),
            _ => Err(Error::WrongType { expected: "bytes" }),
        }
    }

    /// Attempts to unwrap the value as a list.
    ///
    /// # Errors
    /// Returns `Error::WrongType` if the variant is not `Bencode::List`.
    pub fn as_list(&self) -> Result<&Vec<Bencode<'a>>, Error> {
        match self {
            Bencode::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    /// Attempts to unwrap the value as a dictionary.
    ///
    /// # Errors
    /// Returns `Error::WrongType` if the variant is not `Bencode::Dict`.
    pub fn as_dict(&self) -> Result<&BTreeMap<&[u8], Bencode<'a>>, Error> {
        match self {
            Bencode::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }

    /// Attempts to unwrap the value as a UTF-8 string.
    ///
    /// # Errors
    /// Returns `Error::WrongType` if the variant is not `Bencode::Bytes`, 
    /// or `Error::InvalidUtf8` if the bytes are not valid UTF-8.
    pub fn as_str(&self) -> Result<&str, Error> {
        let bytes = self.as_bytes()?;
        Ok(std::str::from_utf8(bytes)?)
    }
}

/// A parser for reading Bencode-encoded byte slices.
///
/// Holds a reference to the underlying byte slice and maintains a cursor 
/// to track the current parsing position.
#[derive(Debug)]
pub struct Parser<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new `Parser` instance to read from the provided byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }

    /// Parses a single Bencode element from the current cursor position.
    ///
    /// This method recursively parses complex structures like lists and dictionaries.
    ///
    /// # Errors
    /// Returns an `Error` if the data is malformed, unexpectedly truncated, or invalid.
    pub fn parse(&mut self) -> Result<Bencode<'a>, Error> {
        match self.peek()? {
            b'i' => self.parse_integer(),
            b'l' => self.parse_list(),
            b'd' => self.parse_dict(),
            b'0'..=b'9' => self.parse_bytes(),
            b => Err(Error::UnexpectedByte(self.cursor, b)),
        }
    }

    /// Returns a raw slice of the data from the `start` index up to the current cursor position.
    pub fn raw_span(&self, start: usize) -> &'a [u8] {
        &self.data[start..self.cursor]
    }

    /// Returns the current position of the parser's cursor.
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

    /// Peeks at a slice of the given length without advancing the cursor.
    fn peek_slice(&self, len: usize) -> Result<&'a [u8], Error> {
        self.data
            .get(self.cursor..self.cursor + len)
            .ok_or(Error::UnexpectedEof)
    }

    /// Parses a Bencode integer (format: `i<number>e`).
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

    /// Parses a Bencode byte string (format: `<length>:<contents>`).
    fn parse_bytes(&mut self) -> Result<Bencode<'a>, Error> {
        let colon = self.data[self.cursor..]
            .iter()
            .position(|&b| b == b':')
            .ok_or(Error::UnexpectedEof)?;
        let len_str = std::str::from_utf8(&self.data[self.cursor..self.cursor + colon])?;
        let len = len_str.parse::<usize>()?;
        self.cursor += colon + 1;
        let bytes = self.peek_slice(len)?;
        self.cursor += len;
        Ok(Bencode::Bytes(bytes))
    }

    /// Parses a Bencode list (format: `l<contents>e`).
    fn parse_list(&mut self) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let mut list = vec![];
        while self.peek()? != b'e' {
            let item = self.parse()?;
            list.push(item);
        }
        self.cursor += 1;
        Ok(Bencode::List(list))
    }

    /// Parses a Bencode dictionary (format: `d<contents>e`).
    fn parse_dict(&mut self) -> Result<Bencode<'a>, Error> {
        self.cursor += 1;
        let mut map = BTreeMap::new();
        while self.peek()? != b'e' {
            let key = match self.parse()? {
                Bencode::Bytes(b) => b,
                _ => return Err(Error::NonStringKey),
            };
            let value = self.parse()?;
            map.insert(key, value);
        }
        self.cursor += 1;
        Ok(Bencode::Dict(map))
    }
}

/// Errors that can occur during Bencode parsing.
#[derive(Debug, Error)]
pub enum Error {
    /// Occurs when expected UTF-8 data is invalid.
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    /// Occurs when standard integer parsing fails.
    #[error("Integer parsing error: {0}")]
    InvalidInteger(#[from] std::num::ParseIntError),
    /// Occurs when an integer violates Bencode formatting rules (e.g., leading zeros or negative zero).
    #[error("Invalid Bencode integer representation: {0}")]
    InvalidBencodeInteger(String),
    /// Occurs when an unexpected byte is encountered during parsing.
    #[error("Unexpected byte at position {0}: {1}")]
    UnexpectedByte(usize, u8),
    /// Occurs when the data ends before parsing is complete.
    #[error("Unexpected EOF")]
    UnexpectedEof,
    /// Occurs when a dictionary key is not a byte string.
    #[error("Keys of Bencode dictionaries must be strings")]
    NonStringKey,
    /// Occurs when unwrapping a `Bencode` value into the wrong type.
    #[error("Wrong Bencode type, expected {expected}")]
    WrongType {
        /// The type that was expected.
        expected: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_integer() {
        assert_eq!(Parser::new(b"i42e").parse().unwrap().as_int().unwrap(), 42);
        assert_eq!(Parser::new(b"i-42e").parse().unwrap().as_int().unwrap(), -42);
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
}
