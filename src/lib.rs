//! Pure Rust implementation of the LittleFS v2 on disk format.
//!
//! This crate targets embedded systems first: `no_std`, no global allocator,
//! safe Rust only, no FFI to the C reference. The on disk bytes match what
//! the C reference would write; the API shape follows Rust idiom rather than
//! the C reference's signatures. See `docs/decisions/0001-pure-rust-no-ffi.md`
//! and `docs/decisions/0002-spec-as-oracle.md` for the design reasoning.
//!
//! # Status
//!
//! `0.1.0` is the foundation layer. It contains the bit accurate primitives
//! (CRC, tag, block address, path, storage trait) and the verification
//! scaffolding. It does not yet mount, read, or write a filesystem. Track
//! `KNOWN_ISSUES.md` for the path to v1.0.
//!
//! # Crate features
//!
//! - `alloc` enables `Vec` based read buffers and owned path types.
//! - `std` enables `std::error::Error` for the [`Error`] type and `std::io`
//!   adapters around the [`Storage`] trait.
//! - `kani` compiles the formal verification harnesses under
//!   `src/verify/`; off in normal builds.
//!
//! # The verification posture
//!
//! See `docs/decisions/0003-verification-stacks.md`. Five stacks: unit tests,
//! proptest property tests, golden conformance vectors from the C reference,
//! Kani harnesses, and libFuzzer corpora. Each catches a class of failure the
//! others miss.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod block;
pub mod crc;
pub mod dir;
pub mod error;
pub mod fs;
pub mod meta;
pub mod path;
pub mod storage;
pub mod superblock;
pub mod tag;

// `src/verify/` (Kani harnesses) is added in Phase 3 alongside the commit
// reader. The `kani` feature exists in Cargo.toml so downstream crates can
// already depend on the feature flag, but the module body is not yet
// authored. See `docs/PLAN.md`.

pub use crate::block::{BlockAddress, BlockPair};
pub use crate::dir::{entries, lookup, DirEntry, EntryKind, Resolved};
pub use crate::error::{Error, Result};
pub use crate::fs::Fs;
pub use crate::meta::{MetadataPair, MetadataReader, TagEntry};
pub use crate::path::Path;
pub use crate::storage::Storage;
pub use crate::superblock::Superblock;
pub use crate::tag::{AbstractType, Tag, TagType};

/// The LittleFS v2 on disk format version this crate targets.
///
/// Encoded as `(major << 16) | minor`. The current value is `2.1`, matching
/// the upstream C reference at the time of writing.
pub const DISK_VERSION: u32 = 0x0002_0001;

/// The magic string at the head of every LittleFS superblock.
pub const MAGIC: &[u8; 8] = b"littlefs";

/// The fixed location of the root metadata pair. Always blocks `(0, 1)`.
pub const ROOT_BLOCK_PAIR: BlockPair = BlockPair::new(BlockAddress::new(0), BlockAddress::new(1));

/// The upper bound on path component length. Matches `LFS2_NAME_MAX` in the C
/// reference; the spec allows implementations to lower this but not to raise
/// it above 1022.
pub const NAME_MAX: usize = 255;
