//! The geometry gate is strong enough for the arithmetic behind it.
//!
//! [`crate::geometry::fault_for`] is the predicate
//! [`crate::Fs::mount`] and [`crate::Fs::format`] enforce, at compile
//! time through [`crate::geometry::Geometry::CHECK`] and at runtime
//! through [`crate::geometry::validate`]. `ctz_proofs` proves the CTZ
//! capacity arithmetic total for every `block_size` at or above the
//! 128 byte floor, and proves that floor tight: every value below it
//! faults. These harnesses close the remaining link, that the gate
//! admits nothing below the floor, so the assumption `ctz_proofs`
//! makes is one the gate discharges.
//!
//! # What is symbolic and what is bounded
//!
//! `block_size` is fully symbolic `usize`. `read_size` and `prog_size`
//! are pinned to concrete values, because `fault_for` takes two
//! remainders against them and CBMC's bit vector refinement does not
//! converge on a symbolic 64 bit divisor; `ctz_proofs` records the same
//! limitation for the 32 bit divisor in `block_index_at_offset`. Two
//! pairs are pinned:
//!
//! - `(1, 1)`, the finest grid, where relations 1, 2, 3, and 5 hold for
//!   every `block_size`, so the floor is the only relation that can
//!   reject and the claim is about the floor alone.
//! - `(16, 16)`, the geometry every integration, conformance, and round
//!   trip suite in this repository runs at.
//!
//! Other grids are covered by the fault table in
//! `src/geometry.rs`'s unit tests and by `tests/geometry_floor.rs`.

use crate::ctz::content_bytes_in_block;
use crate::geometry::{fault_for, GeometryFault, BLOCK_SIZE_MIN};

/// No block size below the floor is admitted, at either pinned grid,
/// and the reason reported is the floor rather than an incidental
/// earlier relation.
#[kani::proof]
fn geometry_rejects_every_sub_floor_block_size() {
    let block_size: usize = kani::any();
    kani::assume(block_size < BLOCK_SIZE_MIN);
    // Non vacuity: the assumed set is not empty and reaches the
    // boundary from below.
    kani::cover!(block_size == BLOCK_SIZE_MIN - 1);
    kani::cover!(block_size == 0);
    assert_eq!(
        fault_for(1, 1, block_size, 2),
        Some(GeometryFault::BlockSizeBelowFloor),
        "a sub floor block size passed the unit grid gate"
    );
    assert_eq!(
        fault_for(16, 16, block_size, 2),
        Some(GeometryFault::BlockSizeBelowFloor),
        "a sub floor block size passed the 16 byte grid gate"
    );
}

/// Every geometry the gate admits makes the CTZ content capacity
/// subtraction total, for every one of the 2^32 block indices a corrupt
/// `CtzStruct` can name. This is the property the gate exists for: the
/// partiality `ctz_content_bytes_in_block_underflows_below_the_floor`
/// pins is unreachable behind an admitted geometry.
#[kani::proof]
fn an_admitted_geometry_makes_the_ctz_capacity_total() {
    let block_size: usize = kani::any();
    let index: u32 = kani::any();
    kani::assume(fault_for(1, 1, block_size, 2).is_none());
    // Non vacuity: an assumption no input satisfies would discharge
    // every assertion below for free. These cover checks fail unless
    // the admitted set really contains the floor itself and something
    // above it.
    kani::cover!(block_size == BLOCK_SIZE_MIN);
    kani::cover!(block_size > BLOCK_SIZE_MIN);
    // The word ceiling relation is what makes this cast lossless.
    let content = content_bytes_in_block(index, block_size as u32);
    assert!(content <= block_size as u32, "content cannot exceed the block");
}
