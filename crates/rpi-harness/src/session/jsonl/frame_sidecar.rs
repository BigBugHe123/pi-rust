//! Optional assistant-frame sidecar block codec.
//!
//! This module is deliberately not wired into the default JSONL storage path
//! yet. It defines the physical block format we can use in a later migration
//! while keeping the logical [`LaneRecord`] stream unchanged.
//!
//! A sidecar consists of independent blocks. Each block has a fixed header and
//! a newline-delimited JSON payload containing only `assistant_frame` records.
//! A truncated final block is reported as a recoverable tail; completed blocks
//! before it remain readable.

use std::fmt;

use serde_json::Value;

use crate::session::types::LaneRecord;

const MAGIC: &[u8; 8] = b"RPIAFRM\0";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 34;
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

/// Decode all complete blocks from a sidecar byte stream.
///
/// A short final header or payload is a recoverable torn tail. A complete
/// header with malformed JSON is corruption and returns an error instead.
pub fn decode_blocks(bytes: &[u8]) -> Result<FrameSidecarDecode, FrameSidecarError> {
    let mut offset = 0;
    let mut records = Vec::new();
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
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
