//! Optional assistant-frame sidecar block codec.
//!
//! The default storage mode remains inline JSONL. `JsonlSessionStorage` can opt
//! into this format explicitly or through `RPI_FRAME_SIDECAR=1`.
//!
//! A sidecar consists of independent blocks. Each block has a fixed header and
//! a newline-delimited JSON payload containing only `assistant_frame` records.
//! A truncated final block is reported as a recoverable tail; completed blocks
//! before it remain readable.

use std::fmt;

#[cfg(feature = "frame-sidecar-zlib")]
use std::io::{Read, Write};

#[cfg(feature = "frame-sidecar-zlib")]
use flate2::read::ZlibDecoder;
#[cfg(feature = "frame-sidecar-zlib")]
use flate2::write::ZlibEncoder;
#[cfg(feature = "frame-sidecar-zlib")]
use flate2::Compression;
use serde_json::Value;

use crate::session::types::LaneRecord;

const MAGIC: &[u8; 8] = b"RPIAFRM\0";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 34;
#[cfg(feature = "frame-sidecar-zlib")]
const COMPRESSED_MAGIC: &[u8; 8] = b"RPIAZLB\0";
#[cfg(feature = "frame-sidecar-zlib")]
const COMPRESSED_VERSION: u16 = 1;
#[cfg(feature = "frame-sidecar-zlib")]
const COMPRESSED_HEADER_LEN: usize = 18;
const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;

/// Result of decoding a sidecar byte stream.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSidecarDecode {
    pub records: Vec<LaneRecord>,
    /// True when the byte stream ended in a partial final block. The caller may
    /// safely retain all decoded records and discard only that tail.
    pub truncated_tail: bool,
}

/// Errors that indicate a complete block is corrupt rather than merely torn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameSidecarError {
    EmptyBlock,
    InvalidMagic,
    UnsupportedVersion(u16),
    BlockTooLarge(u32),
    InvalidLength,
    InvalidRecord(String),
    #[cfg(feature = "frame-sidecar-zlib")]
    Compression(String),
    NonFrameRecord,
    NonMonotonicSequence,
}

impl fmt::Display for FrameSidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBlock => f.write_str("assistant frame sidecar block is empty"),
            Self::InvalidMagic => f.write_str("assistant frame sidecar magic is invalid"),
            Self::UnsupportedVersion(version) => {
                write!(
                    f,
                    "assistant frame sidecar version {version} is unsupported"
                )
            }
            Self::BlockTooLarge(bytes) => {
                write!(
                    f,
                    "assistant frame sidecar block is too large: {bytes} bytes"
                )
            }
            Self::InvalidLength => f.write_str("assistant frame sidecar block length is invalid"),
            Self::InvalidRecord(error) => {
                write!(f, "assistant frame sidecar record is invalid: {error}")
            }
            #[cfg(feature = "frame-sidecar-zlib")]
            Self::Compression(error) => {
                write!(f, "assistant frame sidecar compression failed: {error}")
            }
            Self::NonFrameRecord => {
                f.write_str("assistant frame sidecar contains a non-frame record")
            }
            Self::NonMonotonicSequence => {
                f.write_str("assistant frame sidecar sequences are not increasing")
            }
        }
    }
}

impl std::error::Error for FrameSidecarError {}

/// Encode a reference marker stored in the main JSONL record.
pub fn reference_marker(offset: u64, index: u32) -> Value {
    serde_json::json!({
        "$rpiFrameSidecar": {
            "offset": offset,
            "index": index,
        }
    })
}

/// Read a reference marker from a frame payload.
pub fn parse_reference_marker(value: &Value) -> Option<(u64, u32)> {
    let marker = value.get("$rpiFrameSidecar")?.as_object()?;
    Some((
        marker.get("offset")?.as_u64()?,
        u32::try_from(marker.get("index")?.as_u64()?).ok()?,
    ))
}

/// Decode one complete sidecar block at a byte offset.
pub fn decode_block_at(bytes: &[u8], offset: u64) -> Result<Vec<LaneRecord>, FrameSidecarError> {
    let offset = usize::try_from(offset).map_err(|_| FrameSidecarError::InvalidLength)?;
    if offset >= bytes.len() {
        return Err(FrameSidecarError::InvalidLength);
    }
    #[cfg(feature = "frame-sidecar-zlib")]
    if bytes[offset..].starts_with(COMPRESSED_MAGIC) {
        let decoded = decode_compressed_block(&bytes[offset..])?;
        let block = decode_blocks(&decoded)?;
        return if block.truncated_tail {
            Err(FrameSidecarError::InvalidLength)
        } else {
            Ok(block.records)
        };
    }

    let remaining = bytes.len() - offset;
    if remaining < HEADER_LEN {
        return Err(FrameSidecarError::InvalidLength);
    }
    let payload_len = u32::from_le_bytes([
        bytes[offset + 10],
        bytes[offset + 11],
        bytes[offset + 12],
        bytes[offset + 13],
    ]) as usize;
    let block_end = offset
        .checked_add(HEADER_LEN)
        .and_then(|end| end.checked_add(payload_len))
        .ok_or(FrameSidecarError::InvalidLength)?;
    if block_end > bytes.len() {
        return Err(FrameSidecarError::InvalidLength);
    }
    let block = decode_blocks(&bytes[offset..block_end])?;
    if block.truncated_tail {
        return Err(FrameSidecarError::InvalidLength);
    }
    Ok(block.records)
}
/// Decode one referenced record from a sidecar block.
pub fn decode_record_at(
    bytes: &[u8],
    offset: u64,
    index: u32,
) -> Result<LaneRecord, FrameSidecarError> {
    let decoded = decode_block_at(bytes, offset)?;
    decoded
        .into_iter()
        .nth(index as usize)
        .ok_or(FrameSidecarError::InvalidLength)
}

/// Encode one non-empty block of assistant-frame records.
pub fn encode_block(records: &[LaneRecord]) -> Result<Vec<u8>, FrameSidecarError> {
    if records.is_empty() {
        return Err(FrameSidecarError::EmptyBlock);
    }

    let mut payload = Vec::new();
    let mut first_seq = None;
    let mut last_seq = None;
    let mut previous_seq = None;
    for record in records {
        if !matches!(record, LaneRecord::AssistantFrame(_)) {
            return Err(FrameSidecarError::NonFrameRecord);
        }
        let seq = record.seq();
        if previous_seq.is_some_and(|previous| seq <= previous) {
            return Err(FrameSidecarError::NonMonotonicSequence);
        }
        previous_seq = Some(seq);
        first_seq.get_or_insert(seq);
        last_seq = Some(seq);
        serde_json::to_writer(&mut payload, record)
            .map_err(|error| FrameSidecarError::InvalidRecord(error.to_string()))?;
        payload.push(b'\n');
    }

    if payload.len() > MAX_BLOCK_BYTES || payload.len() > u32::MAX as usize {
        return Err(FrameSidecarError::BlockTooLarge(
            payload.len().min(u32::MAX as usize) as u32,
        ));
    }

    let mut encoded = Vec::with_capacity(HEADER_LEN + payload.len());
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&VERSION.to_le_bytes());
    encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&(records.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&first_seq.expect("non-empty block").to_le_bytes());
    encoded.extend_from_slice(&last_seq.expect("non-empty block").to_le_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

/// Encode one block with a zlib envelope. The envelope keeps the raw block
/// format intact so old raw sidecars remain readable and references still use
/// the same byte offset + record index scheme.
#[cfg(feature = "frame-sidecar-zlib")]
pub fn encode_compressed_block(records: &[LaneRecord]) -> Result<Vec<u8>, FrameSidecarError> {
    let raw = encode_block(records)?;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&raw)
        .map_err(|error| FrameSidecarError::Compression(error.to_string()))?;
    let compressed = encoder
        .finish()
        .map_err(|error| FrameSidecarError::Compression(error.to_string()))?;
    if compressed.len() > u32::MAX as usize || raw.len() > u32::MAX as usize {
        return Err(FrameSidecarError::BlockTooLarge(
            compressed.len().min(u32::MAX as usize) as u32,
        ));
    }
    let mut encoded = Vec::with_capacity(COMPRESSED_HEADER_LEN + compressed.len());
    encoded.extend_from_slice(COMPRESSED_MAGIC);
    encoded.extend_from_slice(&COMPRESSED_VERSION.to_le_bytes());
    encoded.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&compressed);
    Ok(encoded)
}

#[cfg(feature = "frame-sidecar-zlib")]
fn decode_compressed_block(bytes: &[u8]) -> Result<Vec<u8>, FrameSidecarError> {
    if bytes.len() < COMPRESSED_HEADER_LEN {
        return Err(FrameSidecarError::InvalidLength);
    }
    if &bytes[..8] != COMPRESSED_MAGIC {
        return Err(FrameSidecarError::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != COMPRESSED_VERSION {
        return Err(FrameSidecarError::UnsupportedVersion(version));
    }
    let compressed_len = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    let raw_len = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]) as usize;
    if raw_len > MAX_BLOCK_BYTES || compressed_len > MAX_BLOCK_BYTES {
        return Err(FrameSidecarError::InvalidLength);
    }
    let end = COMPRESSED_HEADER_LEN
        .checked_add(compressed_len)
        .ok_or(FrameSidecarError::InvalidLength)?;
    if end > bytes.len() {
        return Err(FrameSidecarError::InvalidLength);
    }
    let mut decoded = Vec::with_capacity(raw_len);
    let mut decoder = ZlibDecoder::new(&bytes[COMPRESSED_HEADER_LEN..end]);
    decoder
        .read_to_end(&mut decoded)
        .map_err(|error| FrameSidecarError::Compression(error.to_string()))?;
    if decoded.len() != raw_len {
        return Err(FrameSidecarError::InvalidLength);
    }
    Ok(decoded)
}

/// Decode all complete blocks from a sidecar byte stream.
///
/// A short final header or payload is a recoverable torn tail. A complete
/// header with malformed JSON is corruption and returns an error instead.
pub fn decode_blocks(bytes: &[u8]) -> Result<FrameSidecarDecode, FrameSidecarError> {
    let mut offset = 0;
    let mut records = Vec::new();
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        #[cfg(feature = "frame-sidecar-zlib")]
        if bytes[offset..].starts_with(COMPRESSED_MAGIC) {
            if remaining < COMPRESSED_HEADER_LEN {
                return Ok(FrameSidecarDecode {
                    records,
                    truncated_tail: true,
                });
            }
            let compressed_len = u32::from_le_bytes([
                bytes[offset + 10],
                bytes[offset + 11],
                bytes[offset + 12],
                bytes[offset + 13],
            ]) as usize;
            let block_end = offset
                .checked_add(COMPRESSED_HEADER_LEN)
                .and_then(|length| length.checked_add(compressed_len))
                .ok_or(FrameSidecarError::InvalidLength)?;
            if block_end > bytes.len() {
                return Ok(FrameSidecarDecode {
                    records,
                    truncated_tail: true,
                });
            }
            let decoded = decode_compressed_block(&bytes[offset..block_end])?;
            let block = decode_blocks(&decoded)?;
            if block.truncated_tail {
                return Err(FrameSidecarError::InvalidLength);
            }
            records.extend(block.records);
            offset = block_end;
            continue;
        }

        if remaining < HEADER_LEN {
            return Ok(FrameSidecarDecode {
                records,
                truncated_tail: true,
            });
        }

        let header = &bytes[offset..offset + HEADER_LEN];
        if &header[..8] != MAGIC {
            return Err(FrameSidecarError::InvalidMagic);
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != VERSION {
            return Err(FrameSidecarError::UnsupportedVersion(version));
        }
        let payload_len = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);
        let record_count =
            u32::from_le_bytes([header[14], header[15], header[16], header[17]]) as usize;
        let first_seq = u64::from_le_bytes(header[18..26].try_into().expect("header width"));
        let last_seq = u64::from_le_bytes(header[26..34].try_into().expect("header width"));
        if payload_len as usize > MAX_BLOCK_BYTES {
            return Err(FrameSidecarError::BlockTooLarge(payload_len));
        }
        if record_count == 0 || first_seq > last_seq {
            return Err(FrameSidecarError::InvalidLength);
        }
        let block_end = HEADER_LEN
            .checked_add(payload_len as usize)
            .and_then(|length| offset.checked_add(length))
            .ok_or(FrameSidecarError::InvalidLength)?;
        if block_end > bytes.len() {
            return Ok(FrameSidecarDecode {
                records,
                truncated_tail: true,
            });
        }

        let payload = &bytes[offset + HEADER_LEN..block_end];
        let mut block_records = Vec::with_capacity(record_count);
        for line in payload
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let value: Value = serde_json::from_slice(line)
                .map_err(|error| FrameSidecarError::InvalidRecord(error.to_string()))?;
            let record: LaneRecord = serde_json::from_value(value)
                .map_err(|error| FrameSidecarError::InvalidRecord(error.to_string()))?;
            if !matches!(record, LaneRecord::AssistantFrame(_)) {
                return Err(FrameSidecarError::NonFrameRecord);
            }
            block_records.push(record);
        }
        if block_records.len() != record_count {
            return Err(FrameSidecarError::InvalidLength);
        }
        let mut previous_seq = None;
        for record in &block_records {
            let seq = record.seq();
            if previous_seq.is_some_and(|previous| seq <= previous) {
                return Err(FrameSidecarError::NonMonotonicSequence);
            }
            previous_seq = Some(seq);
        }
        if block_records.first().expect("count checked").seq() != first_seq
            || block_records.last().expect("count checked").seq() != last_seq
        {
            return Err(FrameSidecarError::InvalidLength);
        }
        records.extend(block_records);
        offset = block_end;
    }

    Ok(FrameSidecarDecode {
        records,
        truncated_tail: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{AssistantFrameOp, AssistantFrameRecord, RecordBase};

    fn frame(seq: u64, op: AssistantFrameOp) -> LaneRecord {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: format!("frame-{seq}"),
                seq,
                lane: "main".to_string(),
                timestamp: seq as i64,
            },
            run_id: "run-1".to_string(),
            stream_index: 0,
            op,
            frame: Some(serde_json::json!({"entryId":"entry-1","text":"hello"})),
        })
    }

    #[test]
    fn round_trips_control_and_append_frames() {
        let records = vec![
            frame(1, AssistantFrameOp::Append),
            frame(2, AssistantFrameOp::CommitStream),
            frame(3, AssistantFrameOp::ClearStream),
        ];
        let encoded = encode_block(&records).unwrap();
        let decoded = decode_blocks(&encoded).unwrap();
        assert_eq!(decoded.records, records);
        assert!(!decoded.truncated_tail);
    }

    #[test]
    fn preserves_completed_blocks_before_torn_tail() {
        let first = encode_block(&[frame(1, AssistantFrameOp::Append)]).unwrap();
        let second = encode_block(&[frame(2, AssistantFrameOp::ClearRun)]).unwrap();
        let mut bytes = first;
        bytes.extend_from_slice(&second[..second.len() - 1]);
        let decoded = decode_blocks(&bytes).unwrap();
        assert_eq!(decoded.records, vec![frame(1, AssistantFrameOp::Append)]);
        assert!(decoded.truncated_tail);
    }

    #[test]
    fn rejects_non_frame_records_and_non_monotonic_sequences() {
        let non_frame =
            LaneRecord::OperationFinished(crate::session::types::OperationFinishedRecord {
                base: RecordBase {
                    id: "finish".to_string(),
                    seq: 1,
                    lane: "main".to_string(),
                    timestamp: 1,
                },
                run_id: "run-1".to_string(),
                outcome: crate::session::types::OperationOutcome::Completed,
                error: None,
            });
        assert_eq!(
            encode_block(&[non_frame]),
            Err(FrameSidecarError::NonFrameRecord)
        );
        assert_eq!(
            encode_block(&[
                frame(2, AssistantFrameOp::Append),
                frame(1, AssistantFrameOp::Append)
            ]),
            Err(FrameSidecarError::NonMonotonicSequence)
        );
    }

    #[cfg(feature = "frame-sidecar-zlib")]
    #[test]
    fn zlib_compression_round_trips_and_reduces_repeated_frame_envelopes() {
        let records: Vec<_> = (1..=128)
            .map(|seq| frame(seq, AssistantFrameOp::Append))
            .collect();
        let inline_bytes: usize = records
            .iter()
            .map(|record| serde_json::to_vec(record).unwrap().len() + 1)
            .sum();
        let raw_block = encode_block(&records).unwrap();
        let compressed = encode_compressed_block(&records).unwrap();
        assert!(compressed.len() < inline_bytes);
        assert_eq!(decode_blocks(&compressed).unwrap().records, records);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(
                decode_record_at(&compressed, 0, index as u32).unwrap(),
                *record
            );
        }

        if std::env::var_os("RPI_FRAME_SIDECAR_AUDIT").is_some_and(|value| value != "0") {
            eprintln!(
                "RPI_FRAME_SIDECAR_AUDIT records={} inline_bytes={} raw_block_bytes={} compressed_block_bytes={} compression_ratio={:.3}",
                records.len(),
                inline_bytes,
                raw_block.len(),
                compressed.len(),
                compressed.len() as f64 / inline_bytes as f64,
            );
        }
    }

    #[cfg(feature = "frame-sidecar-zlib")]
    #[test]
    fn compressed_blocks_preserve_completed_prefix_before_torn_tail() {
        let first = encode_compressed_block(&[frame(1, AssistantFrameOp::Append)]).unwrap();
        let second = encode_compressed_block(&[frame(2, AssistantFrameOp::Append)]).unwrap();
        let mut bytes = first;
        bytes.extend_from_slice(&second[..second.len() - 1]);
        let decoded = decode_blocks(&bytes).unwrap();
        assert_eq!(decoded.records, vec![frame(1, AssistantFrameOp::Append)]);
        assert!(decoded.truncated_tail);
    }
    #[test]
    fn rejects_bad_magic() {
        let mut encoded = encode_block(&[frame(1, AssistantFrameOp::Append)]).unwrap();
        encoded[0] ^= 1;
        assert_eq!(
            decode_blocks(&encoded),
            Err(FrameSidecarError::InvalidMagic)
        );
    }
}
