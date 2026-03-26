use std::collections::BTreeMap;

use memchr::memchr;
use thiserror::Error;

#[derive(Debug)]
pub enum BencodeValue<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<BencodeValue<'a>>),
    Dict(BTreeMap<&'a [u8], BencodeValue<'a>>),
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

    pub fn parse(&mut self) -> Result<BencodeValue<'a>, BencodeError> {
        match self.peek()? {
            b'i' => self.parse_integer(),
            b'l' => self.parse_list(),
            b'd' => self.parse_dict(),
            b'0'..=b'9' => self.parse_bytes(),
            b => Err(BencodeError::UnexpectedByte(self.cursor, b)),
        }
    }

    pub fn raw_span(&self, start: usize) -> &'a [u8] {
        &self.data[start..self.cursor]
    }

    pub fn position(&self) -> usize {
        self.cursor
    }

    fn peek(&self) -> Result<u8, BencodeError> {
        self.data
            .get(self.cursor)
            .copied()
            .ok_or(BencodeError::UnexpectedEof)
    }

    fn peek_slice(&self, len: usize) -> Result<&'a [u8], BencodeError> {
        self.data
            .get(self.cursor..self.cursor + len)
            .ok_or(BencodeError::UnexpectedEof)
    }

    fn parse_integer(&mut self) -> Result<BencodeValue<'a>, BencodeError> {
        self.cursor += 1;
        let end = memchr(b'e', &self.data[self.cursor..]).ok_or(BencodeError::UnexpectedEof)?;
        let s = std::str::from_utf8(&self.data[self.cursor..self.cursor + end])?;

        if s.starts_with("-0") || (s.starts_with('0') && s.len() > 1) {
            return Err(BencodeError::InvalidBencodeInteger(s.to_string()));
        }

        let i = s.parse::<i64>()?;
        self.cursor += end + 1;
        Ok(BencodeValue::Int(i))
    }

    fn parse_bytes(&mut self) -> Result<BencodeValue<'a>, BencodeError> {
        let colon = memchr(b':', &self.data[self.cursor..]).ok_or(BencodeError::UnexpectedEof)?;
        let len_str = std::str::from_utf8(&self.data[self.cursor..self.cursor + colon])?;
        let len = len_str.parse::<usize>()?;
        self.cursor += colon + 1;
        let bytes = self.peek_slice(len)?;
        self.cursor += len;
        Ok(BencodeValue::Bytes(bytes))
    }

    fn parse_list(&mut self) -> Result<BencodeValue<'a>, BencodeError> {
        self.cursor += 1;
        let mut list = vec![];
        while self.peek()? != b'e' {
            let item = self.parse()?;
            list.push(item);
        }
        self.cursor += 1;
        Ok(BencodeValue::List(list))
    }

    fn parse_dict(&mut self) -> Result<BencodeValue<'a>, BencodeError> {
        self.cursor += 1;
        let mut map = BTreeMap::new();
        while self.peek()? != b'e' {
            let key = match self.parse()? {
                BencodeValue::Bytes(b) => b,
                _ => return Err(BencodeError::NonStringKey),
            };
            let value = self.parse()?;
            map.insert(key, value);
        }
        self.cursor += 1;
        Ok(BencodeValue::Dict(map))
    }
}

#[derive(Debug, Error)]
pub enum BencodeError {
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
}
