//! Kani proof harnesses.
//!
//! Each harness in this module is a `#[kani::proof]` function
//! discharging a totality or agreement property over the relevant
//! kernel primitive. The proofs are SAT-solved during
//! `cargo kani --features=kani`; in normal builds the module is not
//! even compiled (the parent gates on `#[cfg(kani)]`).
//!
//! # Coverage at this revision
//!
//! - `tag_proofs`: every 11 bit type field and every 32 bit raw tag
//!   word round trips through `from_bits` / `into_bits` without
//!   panicking. `AbstractType::from_bits` is total over `0..8` and
//!   rejects everything else. Every tag word satisfies
//!   `dsize() == 4 + body_len()` with `dsize() <= 1026`, and
//!   `is_ccrc()` holds exactly when `ccrc_chunk()` returns `Some`,
//!   which is the classification totality the metadata reader
//!   depends on.
//! - `crc_proofs`: the table driven `crc::update` agrees with the bit
//!   by bit `crc::update_bitwise` for every seed and every single byte
//!   and two byte input, both agree that the empty slice is the
//!   identity, and `update` is associative over one byte
//!   concatenation. Bounded because Kani's symbolic loop budget is
//!   finite; longer inputs are covered by `tests/property_crc.rs`.
//! - `meta_proofs`: `meta::rev_scmp` is total over every `(a, b)`,
//!   returns zero exactly when `a == b`, is antisymmetric wherever
//!   neither result is `i32::MIN`, and reports `b.wrapping_add(1)` as
//!   newer than `b` across the `u32` wrap. Agreement with
//!   `a.wrapping_sub(b) as i32` is not proven here and would be
//!   vacuous: that expression is the whole body of `rev_scmp`. What
//!   the harnesses pin is that the modular ordering the definition
//!   implies is the one the active block selector needs.
//! - `commit_proofs`: `MetadataReader::new` never panics on arbitrary
//!   block bytes. The call returns rather than panicking for every 32
//!   byte block, it errors on a block too short to hold the four byte
//!   revision header (pinned at three bytes), and it leaves
//!   `committed_end()` within the block for every 16 byte input. Two
//!   of the three harnesses stub `crc::update` to a nondeterministic
//!   `u32`, which strengthens the panic freedom result rather than
//!   weakening it: the reader must survive every accept path and
//!   every reject path the CRC could select. Correctness of the
//!   accept or reject decision is therefore NOT proven here; that is
//!   pinned by the conformance and roundtrip vectors against the C
//!   reference.
//! - `ctz_proofs`: the CTZ skip list geometry math is total over every
//!   `u32` index, offset, and size. `skip_pointers_in_block` and
//!   `content_bytes_in_block` hold at every `block_size` meeting the C
//!   reference's 128 byte floor (`lfs.c:4189`), and one harness shows
//!   that floor is tight rather than conservative.
//!   `block_index_at_offset` and `block_count` are pinned to the
//!   128 byte and 256 byte geometries, because a symbolic divisor
//!   defeats CBMC's refinement; the offset they return always lands in
//!   the block's content region. The floor is enforced rather than
//!   merely assumed since `lfs-cw1`; see `geometry_proofs` and
//!   `crate::geometry`.
//! - `geometry_proofs`: the geometry gate `Fs::mount` and `Fs::format`
//!   apply admits no block size below the 128 byte CTZ floor, and every
//!   geometry it does admit makes the CTZ content capacity subtraction
//!   total over all 2^32 block indices. Together with `ctz_proofs` this
//!   closes the loop: the precondition those harnesses assume is the
//!   one the entry points discharge. `block_size` is symbolic;
//!   `read_size` and `prog_size` are pinned to two grids, for the same
//!   symbolic divisor reason.
//! - `commit_writer_proofs`: `meta::Commit` never writes outside the
//!   caller's buffer, every commit it emits ends in a well formed CCRC
//!   tag, its two bounds checks are exact and leave the cursor
//!   untouched on rejection, and it refuses a caller supplied CCRC.
//!   Bounded to a 24 byte buffer and a 4 byte body, with `crc::update`
//!   stubbed nondeterministically. The writer to reader round trip is
//!   out of Kani's reach at this revision; the module docs record the
//!   measurements and name the stacks that cover it instead.
//!
//! Adding a new harness: pin the specification (which C-reference
//! line or this crate's invariant), state the bound (input range,
//! input length), and let `cargo kani` chew.

pub mod commit_proofs;
pub mod commit_writer_proofs;
pub mod crc_proofs;
pub mod ctz_proofs;
pub mod geometry_proofs;
pub mod meta_proofs;
pub mod tag_proofs;
