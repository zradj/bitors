use std::collections::BTreeMap;

use thiserror::Error;

#[derive(Debug)]
pub enum Bencode<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Bencode<'a>>),
    Dict(BTreeMap<&'a [u8], Bencode<'a>>),
}

impl<'a> Bencode<'a> {
    pub fn as_int(&self) -> Result<i64, Error> {
        match self {
            Bencode::Int(i) => Ok(*i),
            _ => Err(Error::WrongType { expected: "int" }),
        }
    }

    pub fn as_bytes(&self) -> Result<&'a [u8], Error> {
        match self {
            Bencode::Bytes(b) => Ok(b),
            _ => Err(Error::WrongType { expected: "bytes" }),
        }
    }

    pub fn as_list(&self) -> Result<&Vec<Bencode<'a>>, Error> {
        match self {
            Bencode::List(l) => Ok(l),
            _ => Err(Error::WrongType { expected: "list" }),
        }
    }

    pub fn as_dict(&self) -> Result<&BTreeMap<&'a [u8], Bencode<'a>>, Error> {
        match self {
            Bencode::Dict(d) => Ok(d),
            _ => Err(Error::WrongType { expected: "dict" }),
        }
    }
}

#[derive(Debug)]
pub struct Parser<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }

    pub fn parse(&mut self) -> Result<Bencode<'a>, Error> {
        match self.peek()? {
            b'i' => self.parse_integer(),
            b'l' => self.parse_list(),
            b'd' => self.parse_dict(),
            b'0'..=b'9' => self.parse_bytes(),
            b => Err(Error::UnexpectedByte(self.cursor, b)),
        }
    }

    pub fn raw_span(&self, start: usize) -> &'a [u8] {
        &self.data[start..self.cursor]
    }

    pub fn position(&self) -> usize {
        self.cursor
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
    #[error("Unexpected EOF")]
    UnexpectedEof,
    #[error("Keys of Bencode dictionaries must be strings")]
    NonStringKey,
    #[error("Wrong Bencode type, expected {expected}")]
    WrongType { expected: &'static str },
}
