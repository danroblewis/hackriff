//! Reference stream reader (docs/stream-contract.md §8): reads the header, then records.
//! Used by `hk stream-tail`, the dummy plugin and the contract tests.

use std::io::{self, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;

use hk_model::ContentClass;

use super::frame::{FrameDecoder, FrameError, HEADER_MAX_LEN};
use super::header::{HeaderError, StreamHeader, StreamKind};
use super::record::{
    BINARY_RECORD_HEADER_LEN, BinaryData, BinaryRecordHeader, BinaryRecordType, DropMarker,
    MessageEnvelope, Record, RecordFlags,
};
use hk_model::Timestamp;

/// Reader errors.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Transport error.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Framing error (e.g. oversize frame).
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Bad header.
    #[error(transparent)]
    Header(#[from] HeaderError),
    /// Malformed record.
    #[error("malformed record: {0}")]
    Record(String),
    /// The stream ended inside a frame, or before a header.
    #[error("stream truncated")]
    Truncated,
}

/// Reads a stream from any byte source.
pub struct StreamReader<R> {
    r: R,
    dec: FrameDecoder,
    header: Option<StreamHeader>,
    eof: bool,
}

impl StreamReader<UnixStream> {
    /// Connects to a Unix-domain-socket listener.
    pub fn connect_uds(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self::new(UnixStream::connect(path)?))
    }
}

impl StreamReader<TcpStream> {
    /// Connects to a TCP listener.
    pub fn connect_tcp(addr: impl ToSocketAddrs) -> io::Result<Self> {
        Ok(Self::new(TcpStream::connect(addr)?))
    }
}

impl<R: Read> StreamReader<R> {
    /// Wraps a byte source.
    pub fn new(r: R) -> Self {
        Self {
            r,
            dec: FrameDecoder::new(HEADER_MAX_LEN),
            header: None,
            eof: false,
        }
    }

    /// The header, once read.
    pub fn header(&self) -> Option<&StreamHeader> {
        self.header.as_ref()
    }

    /// The byte source.
    pub fn get_ref(&self) -> &R {
        &self.r
    }

    fn next_frame(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        loop {
            if let Some(f) = self.dec.next_frame()? {
                return Ok(Some(f.to_vec()));
            }
            if self.eof {
                return if self.dec.buffered() > 0 {
                    Err(ClientError::Truncated)
                } else {
                    Ok(None)
                };
            }
            if self.dec.read_from(&mut self.r)? == 0 {
                self.eof = true;
            }
        }
    }

    /// Reads and validates the header (once).
    pub fn read_header(&mut self) -> Result<&StreamHeader, ClientError> {
        if self.header.is_none() {
            let frame = self.next_frame()?.ok_or(ClientError::Truncated)?;
            let header = StreamHeader::from_json_bytes(&frame)?;
            self.dec.set_max_frame_len(header.max_frame_len);
            self.header = Some(header);
        }
        Ok(self.header.as_ref().expect("just set"))
    }

    /// The next record; `Ok(None)` at a clean end of stream.
    pub fn next_record(&mut self) -> Result<Option<Record>, ClientError> {
        let kind = self.read_header()?.kind;
        match self.next_frame()? {
            None => Ok(None),
            Some(frame) => parse_record(kind, &frame).map(Some),
        }
    }
}

/// Parses one record frame of a stream of `kind`.
pub fn parse_record(kind: StreamKind, frame: &[u8]) -> Result<Record, ClientError> {
    if kind.is_binary() {
        parse_binary(frame)
    } else {
        parse_message(frame)
    }
}

fn parse_message(frame: &[u8]) -> Result<Record, ClientError> {
    let value: serde_json::Value =
        serde_json::from_slice(frame).map_err(|e| ClientError::Record(e.to_string()))?;
    let u64_field = |name: &str| {
        value[name]
            .as_u64()
            .ok_or_else(|| ClientError::Record(format!("missing {name}")))
    };
    match value["type"].as_str() {
        Some("message") => Ok(Record::Message(MessageEnvelope {
            seq: u64_field("seq")?,
            content_class: ContentClass::parse_fail_closed(value["content_class"].as_str()),
            gated: value["gated"].as_bool().unwrap_or(false),
            value,
        })),
        Some("dropped") => Ok(Record::Dropped(DropMarker {
            first_seq: u64_field("first_seq")?,
            count: u64_field("count")?,
            t: Timestamp::from_unix_nanos(value["t"].as_i64().unwrap_or(0)),
            sample_index: 0,
        })),
        _ => Ok(Record::Unknown(frame.to_vec())),
    }
}

fn parse_binary(frame: &[u8]) -> Result<Record, ClientError> {
    let header = BinaryRecordHeader::decode(frame)
        .ok_or_else(|| ClientError::Record(format!("{}-byte binary record", frame.len())))?;
    let payload = &frame[BINARY_RECORD_HEADER_LEN..];
    if header.record_type == BinaryRecordType::Data as u8 {
        let gated = header.flags.contains(RecordFlags::GATED);
        if gated && !payload.is_empty() {
            return Err(ClientError::Record(
                "gated record carries payload bytes".into(),
            ));
        }
        if !gated && payload.len() != header.payload_len as usize {
            return Err(ClientError::Record(format!(
                "payload_len {} but {} bytes",
                header.payload_len,
                payload.len()
            )));
        }
        Ok(Record::Binary(BinaryData {
            header,
            payload: payload.to_vec(),
        }))
    } else if header.record_type == BinaryRecordType::Dropped as u8 {
        let count = payload
            .get(..8)
            .map(|b| u64::from_le_bytes(b.try_into().expect("8")))
            .ok_or_else(|| ClientError::Record("short drop marker".into()))?;
        Ok(Record::Dropped(DropMarker {
            first_seq: header.seq,
            count,
            t: header.t,
            sample_index: header.sample_index,
        }))
    } else {
        Ok(Record::Unknown(frame.to_vec()))
    }
}
