//! Error type for the crate.
//!
//! The variants stay flat and exhaustive on purpose: a caller pattern matching
//! on `Error` should never need a wildcard for "future variants". When a new
//! failure mode appears, add a variant and bump the minor version.

use core::fmt;

/// A specialized [`Result`](core::result::Result) returning [`Error`].
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Every way an operation in `littlefs2-pure` can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The underlying `Storage` reported an I/O failure. The originating
    /// storage error is downcast erased here; callers that need the underlying
    /// type should wrap their storage and surface it before returning.
    Io,

    /// The on disk magic did not match `"littlefs"`. Either the image is not a
    /// LittleFS filesystem at all, or the offsets being probed are wrong.
    NotLittleFs,

    /// The on disk format version is newer than this crate knows how to read.
    /// The contained value is the version word from the superblock, decoded as
    /// `(major << 16) | minor`.
    UnsupportedVersion(u32),

    /// A CRC check failed on a metadata block or commit.
    CrcMismatch,

    /// A tag's bit pattern was not a valid LittleFS tag (reserved bits set,
    /// length field out of bounds, or similar).
    InvalidTag,

    /// A path component exceeded [`crate::NAME_MAX`] bytes, or contained a
    /// disallowed byte (currently only the path separator `/`).
    InvalidPath,

    /// A read returned fewer bytes than requested. Used when an operation must
    /// read a full structure and the storage returned a short read.
    ShortRead,

    /// A name component or struct length exceeded the geometry limit
    /// negotiated at format time.
    OutOfRange,

    /// The filesystem is read only (mounted that way, or the underlying
    /// storage does not implement `program` and `erase`).
    ReadOnly,

    /// A directory or file with the requested name does not exist.
    NotFound,

    /// A directory or file with the requested name already exists.
    AlreadyExists,

    /// Storage geometry (block size, block count, alignment) does not match
    /// what the on disk superblock advertises.
    GeometryMismatch,

    /// The filesystem is corrupt in a way the reader can detect but cannot
    /// repair. Logged with as much context as possible; users should treat
    /// this as a fatal mount error.
    Corrupt,

    /// Attempted to remove a directory that still has live entries.
    /// Caller must remove the contents first (or use a recursive
    /// helper if one becomes available).
    NotEmpty,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Io => "storage I/O error",
            Self::NotLittleFs => "not a LittleFS v2 image",
            Self::UnsupportedVersion(_) => "unsupported on disk format version",
            Self::CrcMismatch => "CRC check failed",
            Self::InvalidTag => "invalid metadata tag",
            Self::InvalidPath => "invalid path component",
            Self::ShortRead => "storage returned a short read",
            Self::OutOfRange => "value exceeds the negotiated geometry limit",
            Self::ReadOnly => "filesystem is mounted read only",
            Self::NotFound => "no such file or directory",
            Self::AlreadyExists => "name already exists",
            Self::GeometryMismatch => "storage geometry does not match the superblock",
            Self::Corrupt => "filesystem is corrupt",
            Self::NotEmpty => "directory is not empty",
        };
        f.write_str(s)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}
