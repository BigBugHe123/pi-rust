//! Zero-copy archived payloads for schema-stable hot paths.
//!
//! Unlike the serde-based [`crate::binary`] codec, this module does not
//! reconstruct an owned Rust value on the read side. A validated archive is
//! borrowed directly from the aligned input buffer and its fields can be read
//! through `rkyv`'s archived representation.
//!
//! The format is intentionally separate from the JSON/CBOR compatibility path:
//! archived layouts are Rust-type contracts, not a general interchange format.
//! Use a versioned wrapper type when the bytes cross a process or plugin
//! boundary, and keep the archive buffer alive for as long as the borrowed view.

use std::fmt;

use rkyv::ser::serializers::AllocSerializer;
use rkyv::validation::validators::DefaultValidator;
use rkyv::{Archive, CheckBytes, Serialize};

/// Scratch space reserved by the convenience encoder.
pub const DEFAULT_SCRATCH_BYTES: usize = 256;

/// Maximum encoded archive size (16 MiB by default).
pub const DEFAULT_MAX_ARCHIVE_LENGTH: usize = 16 * 1024 * 1024;

/// Bounds for zero-copy archive operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroCopyOptions {
    pub max_archive_length: usize,
}

impl Default for ZeroCopyOptions {
    fn default() -> Self {
        Self {
            max_archive_length: DEFAULT_MAX_ARCHIVE_LENGTH,
        }
    }
}

/// Error returned by the zero-copy archive helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZeroCopyError {
    Encode(String),
    InvalidArchive(String),
    ArchiveTooLarge { actual: usize, maximum: usize },
}

impl fmt::Display for ZeroCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(message) => write!(f, "zero-copy archive encode failed: {message}"),
            Self::InvalidArchive(message) => {
                write!(f, "zero-copy archive validation failed: {message}")
            }
            Self::ArchiveTooLarge { actual, maximum } => write!(
                f,
                "zero-copy archive is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
        }
    }
}

impl std::error::Error for ZeroCopyError {}

/// An aligned archive buffer. Keep this allocation alive while using a view
/// returned by [`view`].
pub type ArchiveBytes = rkyv::AlignedVec;

/// Encode a value into an aligned archive without a serde intermediate.
///
/// The returned [`ArchiveBytes`] can be borrowed directly by [`view`].
pub fn encode<T>(value: &T, options: &ZeroCopyOptions) -> Result<ArchiveBytes, ZeroCopyError>
where
    T: Serialize<AllocSerializer<DEFAULT_SCRATCH_BYTES>>,
{
    let bytes = rkyv::to_bytes::<_, DEFAULT_SCRATCH_BYTES>(value)
        .map_err(|error| ZeroCopyError::Encode(format!("{error:?}")))?;
    check_length(bytes.len(), options)?;
    Ok(bytes)
}

/// Validate an archive and return a borrowed typed view.
///
/// The aligned buffer is kept in the API type so safe callers cannot
/// accidentally pass a frame slice whose address is not aligned for the
/// archived root. Validation happens once at the input boundary. Accessing
/// fields through the returned archived value does not allocate or deserialize
/// the original type.
pub fn view<'a, T>(
    bytes: &'a ArchiveBytes,
    options: &ZeroCopyOptions,
) -> Result<&'a rkyv::Archived<T>, ZeroCopyError>
where
    T: Archive,
    rkyv::Archived<T>: CheckBytes<DefaultValidator<'a>>,
{
    check_length(bytes.len(), options)?;
    rkyv::check_archived_root::<T>(bytes)
        .map_err(|error| ZeroCopyError::InvalidArchive(format!("{error:?}")))
}

/// Borrow an already validated archive without repeating validation.
///
/// # Safety
/// The caller must guarantee that `bytes` is an aligned, valid archive for
/// `T`, and that it remains unchanged and alive for the returned lifetime.
/// Prefer [`view`] for archives held in [`ArchiveBytes`].
pub unsafe fn view_unchecked<'a, T>(
    bytes: &'a [u8],
    options: &ZeroCopyOptions,
) -> Result<&'a rkyv::Archived<T>, ZeroCopyError>
where
    T: Archive,
{
    check_length(bytes.len(), options)?;
    // SAFETY: the caller guarantees archive validity, alignment, and lifetime.
    Ok(unsafe { rkyv::archived_root::<T>(bytes) })
}

fn check_length(length: usize, options: &ZeroCopyOptions) -> Result<(), ZeroCopyError> {
    if length == 0 {
        return Err(ZeroCopyError::InvalidArchive("archive is empty".into()));
    }
    if length > options.max_archive_length {
        return Err(ZeroCopyError::ArchiveTooLarge {
            actual: length,
            maximum: options.max_archive_length,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::{Archive, Deserialize, Serialize};

    #[derive(Debug, PartialEq, Archive, Serialize, Deserialize)]
    #[archive(check_bytes)]
    struct TestPayload {
        id: u64,
        name: String,
        values: Vec<u32>,
    }

    #[test]
    fn validated_view_reads_fields_without_deserializing() {
        let value = TestPayload {
            id: 42,
            name: "zero-copy".into(),
            values: vec![1, 2, 3],
        };
        let bytes = encode(&value, &ZeroCopyOptions::default()).unwrap();
        let archived = view::<TestPayload>(&bytes, &ZeroCopyOptions::default()).unwrap();
        assert_eq!(archived.id, 42);
        assert_eq!(&*archived.name, "zero-copy");
        assert_eq!(archived.values.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn rejects_oversized_archives_before_validation() {
        let value = TestPayload {
            id: 42,
            name: "too large".into(),
            values: vec![1, 2, 3],
        };
        let bytes = encode(&value, &ZeroCopyOptions::default()).unwrap();
        let options = ZeroCopyOptions {
            max_archive_length: bytes.len() - 1,
        };
        assert!(matches!(
            view::<TestPayload>(&bytes, &options),
            Err(ZeroCopyError::ArchiveTooLarge { .. })
        ));
    }
}
