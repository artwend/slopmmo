//! Minimal bounds-checked little-endian reader/writer.
//!
//! Hand-rolled on purpose: the packet layout is part of the design, and a
//! versioned binary format with a size ceiling is cheaper to reason about than
//! a generic serde codec at 30 Hz per player.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Not enough bytes left for the requested field.
    Truncated,
    /// A length prefix declares more entries than the receiver accepts.
    TooManyEntries { requested: usize, limit: usize },
    /// Trailing bytes after a complete message.
    Trailing { extra: usize },
    /// Unknown discriminator.
    UnknownKind(u8),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated packet"),
            Self::TooManyEntries { requested, limit } => {
                write!(f, "packet declares {requested} entries, limit {limit}")
            }
            Self::Trailing { extra } => write!(f, "{extra} trailing bytes"),
            Self::UnknownKind(k) => write!(f, "unknown message kind {k}"),
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::with_capacity(128)
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let end = self.pos + N;
        let slice = self.buf.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take::<1>()?[0])
    }

    pub fn i8(&mut self) -> Result<i8, WireError> {
        Ok(self.u8()? as i8)
    }

    pub fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take::<2>()?))
    }

    pub fn i16(&mut self) -> Result<i16, WireError> {
        Ok(i16::from_le_bytes(self.take::<2>()?))
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }

    pub fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.take::<4>()?))
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }

    /// Reads a count and rejects it against a caller-supplied ceiling before
    /// allocating, so a hostile packet cannot drive allocation.
    pub fn count(&mut self, limit: usize) -> Result<usize, WireError> {
        let n = self.u8()? as usize;
        if n > limit {
            return Err(WireError::TooManyEntries {
                requested: n,
                limit,
            });
        }
        Ok(n)
    }

    pub fn ensure_end(&self) -> Result<(), WireError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(WireError::Trailing {
                extra: self.remaining(),
            })
        }
    }
}
