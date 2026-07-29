//! Totality of the CTZ skip-list geometry math.
//!
//! [`crate::ctz`] translates a logical file offset into a
//! `(block index, offset within block)` pair and counts the blocks a
//! file of a given size occupies. The `u32` it translates comes off
//! disk: a `CtzStruct` body is eight bytes of image content, and
//! [`crate::ctz::read_ctz_at`] hands `ctz.size` to
//! [`block_count`](crate::ctz::block_count) before any bound has been
//! applied to it. Every one of the 2^32 values therefore has to
//! produce an answer rather than an arithmetic fault. The functions do
//! unchecked subtraction, unchecked multiplication, and two divisions,
//! which is exactly the shape that aborts in a debug build and wraps
//! silently in a release one.
//!
//! # What is symbolic and what is bounded
//!
//! `index`, `offset`, and `size` are fully symbolic `u32` with no
//! assumption at all: that is the domain the on-disk bytes span.
//!
//! `block_size` is device geometry rather than image content, and the
//! C reference asserts a floor on it at mount time:
//!
//! ```text
//! // check that the block size is large enough to fit all ctz pointers
//! LFS_ASSERT(lfs->cfg->block_size >= 128);
//! ```
//!
//! (`tools/gen_vectors/littlefs/lfs.c:4189`, the vendored oracle at
//! the pinned upstream revision.) The two harnesses that only need the
//! floor keep `block_size` symbolic over `128..=u32::MAX`. The two
//! that call [`block_index_at_offset`](crate::ctz::block_index_at_offset)
//! pin it to a concrete geometry instead, because that function
//! divides by `block_size - 8` twice and CBMC's bit-vector refinement
//! does not converge on a symbolic 32-bit divisor: the symbolic-divisor
//! form ran past 600 seconds against CI's 360-second per-harness
//! budget, and a power-of-two-family form (`1 << e`, `e in 7..=16`)
//! ran past 500 seconds. Two geometries are pinned instead:
//!
//! - `128`, the floor itself, the worst case for header-to-block ratio
//!   (a 128-byte block can be entirely skip-pointer header).
//! - `256`, the geometry every integration suite, conformance vector,
//!   and round-trip vector in this repository uses.
//!
//! Both discharge in about 35 seconds. Other geometries are covered by
//! `tests/property_ctz.rs` (randomized, cross-checked against an
//! independent walk) rather than exhaustively; that is a real bound on
//! the claim, and it is the reason the property test stays.
//!
//! # What these proofs do not claim
//!
//! Panic-freedom and the structural bounds below, not agreement with
//! `lfs_ctz_index`. Bit-for-bit agreement with the C reference is
//! pinned by `tests/property_ctz.rs` and by the conformance vectors,
//! which read real C-written CTZ chains. A harness asserting the
//! formula against a Rust copy of the same formula would prove
//! nothing.

use crate::ctz::{
    block_count, block_index_at_offset, content_bytes_in_block, skip_pointers_in_block,
};

/// The floor the C reference asserts on `block_size` at mount
/// (`lfs.c:4189`). Also the exact worst-case skip-pointer header
/// size: `4 * (ctz(0x8000_0000) + 1) = 4 * 32 = 128`.
const BLOCK_SIZE_FLOOR: u32 = 128;

/// The geometry every integration, conformance, and round-trip suite
/// in this repository runs at.
const TEST_BLOCK_SIZE: u32 = 256;

/// `skip_pointers_in_block` is total over every `u32` index: the
/// `ctz(index) + 1` never overflows, and the count never exceeds 32
/// (the width of the index word). The `4 * count` header therefore
/// never exceeds [`BLOCK_SIZE_FLOOR`] bytes, which is what makes that
/// floor sufficient for [`content_bytes_in_block`].
#[kani::proof]
fn ctz_skip_pointers_in_block_is_total() {
    let index: u32 = kani::any();
    let count = skip_pointers_in_block(index);
    assert!(count <= 32, "skip-pointer count exceeds the index width");
    assert_eq!(count == 0, index == 0, "only block 0 has no skip-pointer header");
    assert!(4 * count <= BLOCK_SIZE_FLOOR, "header outgrows the block-size floor");
}

/// `content_bytes_in_block` never underflows, for any index, at any
/// `block_size` meeting the C reference's floor. Its result is a real
/// byte count: at most the whole block, at least the block minus the
/// widest possible skip-pointer header.
#[kani::proof]
fn ctz_content_bytes_in_block_no_underflow() {
    let index: u32 = kani::any();
    let block_size: u32 = kani::any();
    // Precondition: the geometry floor the C reference asserts at
    // mount time (`lfs.c:4189`). Below it the header does not fit and
    // the subtraction inside the function underflows; see
    // `ctz_content_bytes_in_block_underflows_below_the_floor`.
    kani::assume(block_size >= BLOCK_SIZE_FLOOR);
    let content = content_bytes_in_block(index, block_size);
    assert!(content <= block_size, "content cannot exceed the block");
    assert!(content >= block_size - BLOCK_SIZE_FLOOR, "header larger than the worst case");
}

/// The floor is tight, not conservative. Block index `0x8000_0000`
/// has `ctz = 31`, so 32 skip pointers, so a 128-byte header; for
/// every `block_size` below 128 the subtraction in
/// `content_bytes_in_block` underflows on that index. Every execution
/// of this harness faults, which is what `#[kani::should_panic]`
/// asserts.
///
/// The failure mode this pins is worse than the abort it shows. Kani
/// checks arithmetic overflow unconditionally, but a release build
/// with `overflow-checks = false` wraps instead: the caller gets a
/// content size near `u32::MAX` and computes read extents from it. The
/// precondition is therefore load-bearing for more than panic
/// freedom, and `block_size` has to be validated before it reaches
/// this module.
///
/// # Where the precondition is discharged
///
/// The function itself is still partial, so this harness still holds
/// and is deliberately kept. What changed with `lfs-cw1` is where the
/// boundary sits: the floor is now enforced rather than assumed.
/// [`crate::Fs::mount`] and [`crate::Fs::format`] name
/// [`crate::geometry::Geometry::CHECK`], so a sub floor `Storage` is a
/// compile error at either entry point and no `Fs` handle over such a
/// device can exist; [`crate::ctz::read_ctz_at`], the one public reader
/// that computes a per block capacity from a raw `Storage`, reports
/// [`crate::Error::GeometryMismatch`] before the subtraction. The
/// companion harnesses in `crate::verify::geometry_proofs` prove that
/// gate admits nothing below 128.
///
/// The partiality this harness pins therefore describes the raw
/// function, not a reachable state of the kernel. Keep it that way: if
/// a future change makes the function itself total (saturating, or
/// returning a `Result`, or taking a validated geometry newtype), this
/// harness fails and must be deliberately retired. That is the
/// intended signal, not a regression.
#[kani::proof]
#[kani::should_panic]
fn ctz_content_bytes_in_block_underflows_below_the_floor() {
    let block_size: u32 = kani::any();
    kani::assume(block_size < BLOCK_SIZE_FLOOR);
    // 0x8000_0000 is the index whose header is the full 128 bytes, so
    // it witnesses the failure for every block size below the floor at
    // once.
    let _ = content_bytes_in_block(0x8000_0000, block_size);
}

/// `block_index_at_offset` is total over every `u32` offset at the
/// floor geometry: the leading `block_size - 8` does not underflow,
/// neither division divides by zero, the `b * i` multiplication does
/// not overflow, and neither correction subtraction underflows.
///
/// The structural postcondition is the one the read path depends on:
/// the returned offset lands in the block's *content* region, at or
/// past that block's skip-pointer header and strictly inside the
/// block. Every CTZ read issues
/// `storage.read(block, returned_offset, ..)`, so an offset past the
/// block end would run off the block, and an offset inside the header
/// would return skip-pointer bytes as file content. A conforming
/// `Storage` rejects the first; nothing but this arithmetic prevents
/// the second.
///
/// 128 is the worst case for that property: at the floor the header
/// can be the entire block, so the content region can be empty.
#[kani::proof]
fn ctz_block_index_at_offset_is_total_at_the_floor() {
    let offset: u32 = kani::any();
    let (index, abs_off) = block_index_at_offset(offset, BLOCK_SIZE_FLOOR);
    assert!(abs_off < BLOCK_SIZE_FLOOR, "returned offset lands outside the block");
    assert!(
        abs_off >= 4 * skip_pointers_in_block(index),
        "returned offset lands inside the skip-pointer header"
    );
}

/// Same property at the 256-byte geometry the repository's suites run
/// at, so the proof covers the geometry the conformance and
/// round-trip vectors were generated against as well as the floor.
#[kani::proof]
fn ctz_block_index_at_offset_is_total_at_the_test_geometry() {
    let offset: u32 = kani::any();
    let (index, abs_off) = block_index_at_offset(offset, TEST_BLOCK_SIZE);
    assert!(abs_off < TEST_BLOCK_SIZE, "returned offset lands outside the block");
    assert!(
        abs_off >= 4 * skip_pointers_in_block(index),
        "returned offset lands inside the skip-pointer header"
    );
}

/// `block_count` is total over every `u32` size at the floor
/// geometry: the `size - 1` does not underflow (the zero case returns
/// early), the delegated `block_index_at_offset` does not fault, and
/// the `+ 1` does not overflow.
///
/// Postconditions: a file occupies zero blocks exactly when it is
/// empty, and a chain is never longer than the file it holds (each
/// block carries at least one content byte). The read path branches on
/// the first and sizes its address scratch from the second.
#[kani::proof]
fn ctz_block_count_is_total_at_the_floor() {
    let size: u32 = kani::any();
    let count = block_count(size, BLOCK_SIZE_FLOOR);
    assert_eq!(count == 0, size == 0, "only the empty file occupies no blocks");
    assert!(count <= size, "chain longer than the file it holds");
}
