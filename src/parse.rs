use memchr::memchr;
use thiserror::Error;

#[derive(Debug)]
pub enum BencodeValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(Vec<(Vec<u8>, BencodeValue)>),
}

impl BencodeValue {
    pub fn parse(data: &[u8], pos: &mut usize) -> Result<BencodeValue, BencodeError> {
        match data[*pos] {
            b'i' => Self::parse_integer(data, pos),
            b'l' => Self::parse_list(data, pos),
            b'd' => Self::parse_dict(data, pos),
            b'0'..=b'9' => Self::parse_bytes(data, pos),
            b => Err(BencodeError::UnexpectedByte(*pos, b)),
        }
    }

    fn parse_integer(data: &[u8], pos: &mut usize) -> Result<BencodeValue, BencodeError> {
        *pos += 1;
        let end = memchr(b'e', &data[*pos..]).ok_or(BencodeError::UnexpectedEof)?;
        let s = std::str::from_utf8(&data[*pos..*pos + end])?;
        let i = s.parse::<i64>()?;
        *pos += end + 1;
        Ok(Self::Int(i))
    }

    fn parse_bytes(data: &[u8], pos: &mut usize) -> Result<BencodeValue, BencodeError> {
        let colon = memchr(b':', &data[*pos..]).ok_or(BencodeError::UnexpectedEof)?;
        let len = std::str::from_utf8(&data[*pos..*pos + colon])?;
        let len = len.parse::<usize>()?;
        *pos += colon + 1;
        let bytes = data[*pos..*pos + len].to_vec();
        *pos += len;
        Ok(Self::Bytes(bytes))
    }

    fn parse_list(data: &[u8], pos: &mut usize) -> Result<BencodeValue, BencodeError> {
        *pos += 1;
        let mut list = vec![];
        while data[*pos] != b'e' {
            let item = Self::parse(data, pos)?;
            list.push(item);
        }
        *pos += 1;
        Ok(BencodeValue::List(list))
    }

    fn parse_dict(data: &[u8], pos: &mut usize) -> Result<BencodeValue, BencodeError> {
        *pos += 1;
        let mut pairs = vec![];
        while data[*pos] != b'e' {
            let key = match Self::parse(data, pos)? {
                BencodeValue::Bytes(b) => b,
                _ => return Err(BencodeError::NonStringKey),
            };
            let value = Self::parse(data, pos)?;
            pairs.push((key, value));
        }
        *pos += 1;
        Ok(BencodeValue::Dict(pairs))
    }
}

#[derive(Debug, Error)]
pub enum BencodeError {
    #[error("UTF-8 error: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("Integer parsing error: {0}")]
    InvalidInteger(#[from] std::num::ParseIntError),
    #[error("Unexpected byte at position {0}: {1}")]
    UnexpectedByte(usize, u8),
    #[error("Unexpected EOF")]
    UnexpectedEof,
    #[error("Keys of bencode dictionaries must be strings")]
    NonStringKey,
}
