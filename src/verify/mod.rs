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
//! - `tag_proofs`: every 11-bit type field, every 32-bit raw tag word,
//!   and every `TagType` round-trips through `from_bits` / `into_bits`
//!   without panicking. `AbstractType::from_bits` is total over
//!   `0..8` and rejects everything else.
//! - `crc_proofs`: the table-based `crc::update` agrees with the
//!   bit-by-bit `crc::update_bitwise` for every seed and every
//!   single-byte / two-byte input. Bounded because Kani's symbolic
//!   loop budget is finite; longer inputs are covered by
//!   `tests/property_crc.rs`.
//! - `meta_proofs`: `meta::rev_scmp` agrees with the i32-cast of
//!   `a.wrapping_sub(b)` for every pair, and its sign tracks the
//!   modular ordering of the revision counter.
//! - `commit_proofs`: `Tag::from_bits` is total over `u32` and its
//!   classification (`is_ccrc`, `is_valid`, `body_len`, `dsize`) never
//!   panics. The commit-accept-or-reject sketch will land alongside
//!   the power-loss fuzz harnesses; this module is the foothold.
//!
//! Adding a new harness: pin the specification (which C-reference
//! line or this crate's invariant), state the bound (input range,
//! input length), and let `cargo kani` chew.

pub mod commit_proofs;
pub mod crc_proofs;
pub mod meta_proofs;
pub mod tag_proofs;
