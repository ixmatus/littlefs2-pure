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
//! **v1.0 — API frozen.** The kernel implements the complete v2 surface:
//! mount, format, full path resolution with HardTail chasing, inline and
//! CTZ file read and write, streaming append via the stateful [`File`]
//! handle, directory create / remove / rename, user attributes, atomic
//! cross-directory rename with mount-time gstate recovery, and
//! compact-time inter-pair wear levelling with mount-time orphan
//! recovery. Bit accuracy against the C reference is verified in both
//! directions (Rust reads images written by C, C reads images written
//! by Rust); the round-trip and conformance harnesses gate every CI
//! run.
//!
//! Every public item is covered by the semver contract starting at
//! v1.0; future additive changes ship as 1.x minor releases, and
//! `#[non_exhaustive]` is applied to [`Error`], [`EntryKind`],
//! [`AbstractType`], and [`TagType`] so a new spec-driven variant
//! does not require a major version bump.
//!
//! # Entry points
//!
//! - [`Fs::format`] writes a fresh superblock onto a [`Storage`].
//! - [`Fs::mount`] returns a handle to an existing image.
//! - [`Fs::resolve`] walks an absolute path to its [`ResolvedPath`].
//! - [`Fs::read_at_path`] reads from any file (inline or CTZ) at an offset.
//! - [`Fs::write_to_path`] writes or updates a file, auto-dispatching
//!   inline vs CTZ on size.
//! - [`Fs::append_to_path`] streams new bytes onto the end of a file.
//! - [`Fs::mkdir`], [`Fs::rmdir`], [`Fs::rename`], [`Fs::remove_at_path`]
//!   complete the directory surface.
//!
//! The `INTEGRATION.md` file at the repository root walks through a full
//! mount + write + remount example against a SPI NOR adapter, plus the
//! mount-error matrix and the power-loss recovery envelope.
//!
//! # Crate features
//!
//! - `alloc` enables `Vec`-backed read buffers on richer hosts.
//! - `std` enables `std::error::Error` for the [`Error`] type and `std::io`
//!   adapters around the [`Storage`] trait.
//! - `kani` compiles the formal verification harnesses under
//!   `src/verify/`; off in normal builds.
//!
//! The default feature set is empty, so a downstream `no_std` no-`alloc`
//! consumer pulls in nothing beyond `core`.
//!
//! # The verification posture
//!
//! See `docs/decisions/0003-verification-stacks.md`. Five stacks: unit
//! tests, proptest property tests, golden conformance vectors from the C
//! reference, Kani harnesses, and libFuzzer corpora. Each catches a class
//! of failure the others miss. The conformance vectors are committed
//! binaries produced by the vendored C reference under `tools/gen_vectors/`;
//! the round-trip vectors run our writer through a small C verifier under
//! `tools/verify_image/`. Property tests cover CRC, tag layout, CTZ
//! geometry, and metadata commit round-trip. Kani harnesses cover the
//! load-bearing primitives (tag totality, CRC equivalence, revision
//! comparison wrap, commit-reader panic-freedom).

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Rename the std `alloc` extern crate so it doesn't collide with our
// `crate::alloc` module (the block allocator). Internal lib code that
// needs the heap path uses `core_alloc::*`; downstream consumers see
// our `crate::alloc::*` block-allocator API.
#[cfg(feature = "alloc")]
extern crate alloc as core_alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod alloc;
pub mod block;
pub mod crc;
pub mod ctz;
pub mod dir;
pub mod error;
pub mod file;
pub mod fs;
pub mod gstate;
pub mod meta;
pub mod nor;
pub mod path;
pub mod storage;
pub mod superblock;
pub mod tag;

/// Kani proof harnesses. Compiled and discharged by `cargo kani
/// --features=kani`; ignored by `cargo build` / `cargo test`. Each
/// submodule documents what totality property it discharges and
/// against what specification.
#[cfg(kani)]
pub mod verify;

pub use crate::block::{BlockAddress, BlockPair};
pub use crate::ctz::CtzStruct;
pub use crate::dir::{entries, live_entries, lookup, DirEntry, EntryKind, Resolved};
pub use crate::error::{Error, Result};
pub use crate::file::{File, OpenOptions, SeekFrom};
pub use crate::fs::{Fs, ResolvedPath};
// Low-level metadata plumbing, re-exported only for the conformance and
// adversarial test harnesses. Hidden from the rendered docs and a
// candidate to move to `pub(crate)` in 2.0; see the per-item notes in
// `crate::meta`. The re-export stays so the 1.x semver contract holds.
#[doc(hidden)]
pub use crate::meta::{MetadataPair, MetadataReader, TagEntry};
pub use crate::nor::NorAlignedStorage;
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
