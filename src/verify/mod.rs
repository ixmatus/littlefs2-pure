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
//!
//! Adding a new harness: pin the specification (which C-reference
//! line or this crate's invariant), state the bound (input range,
//! input length), and let `cargo kani` chew.

pub mod commit_proofs;
pub mod crc_proofs;
pub mod meta_proofs;
pub mod tag_proofs;
