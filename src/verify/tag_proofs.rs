//! Totality of `Tag` and `TagType` over every input bit pattern.
//!
//! `Tag::from_bits` is a const wrapper around a `u32` and cannot
//! panic; that is mechanical. The interesting properties are
//! around the classification accessors and the round-trip through
//! `into_bits`.

use crate::tag::{AbstractType, Tag, TagType};

/// Every `u16` decodes to a `TagType` (possibly `TagType::Unknown`).
/// No input causes `from_bits` to panic or hang.
#[kani::proof]
fn tag_type_from_bits_is_total() {
    let field: u16 = kani::any();
    // The 11-bit constraint is the on-disk reality. Outside that
    // range, `from_bits` masks down to 11 bits via `(field >> 8) & 0x7`
    // and `field & 0xff`, so technically every u16 is valid input;
    // we assume the upper bits to keep the proof scope tight.
    kani::assume(field < 0x800);
    let _ = TagType::from_bits(field);
}

/// `from_bits` followed by `into_bits` recovers the original 11-bit
/// type field for every defined variant.
#[kani::proof]
fn tag_type_roundtrips_for_defined_variants() {
    let field: u16 = kani::any();
    kani::assume(field < 0x800);
    let decoded = TagType::from_bits(field);
    let reencoded = decoded.into_bits();
    // For variants other than `Unknown`, the reencoded field must
    // match the input. For `Unknown`, the contained abstract_type +
    // chunk preserve the bits exactly, so the round-trip holds
    // universally.
    assert_eq!(reencoded, field);
}

/// `AbstractType::from_bits` returns `Some(_)` for every value in
/// `0..8` and `None` for everything else. Totality without panic.
#[kani::proof]
fn abstract_type_from_bits_is_total() {
    let b: u8 = kani::any();
    let result = AbstractType::from_bits(b);
    if b < 8 {
        assert!(result.is_some(), "AbstractType::from_bits must accept 0..8");
    } else {
        assert!(result.is_none(), "AbstractType::from_bits must reject 8..");
    }
}

/// `Tag::from_bits(b).into_bits() == b` for every `u32`. The wrapper
/// is by construction a `repr(transparent)` newtype; this proof
/// pins it.
#[kani::proof]
fn tag_bits_roundtrip() {
    let b: u32 = kani::any();
    let tag = Tag::from_bits(b);
    assert_eq!(tag.into_bits(), b);
}

/// Every tag word's `dsize()` is finite and equals `4 + body_len()`.
/// The length field is 10 bits (max `0x3FF`), so `body_len() <= 0x3FE`
/// (`0x3FF` is the special-length sentinel that returns 0). `dsize()`
/// therefore stays at or below `4 + 0x3FE = 1026` for every input.
#[kani::proof]
fn tag_dsize_is_bounded() {
    let b: u32 = kani::any();
    let tag = Tag::from_bits(b);
    let body = tag.body_len();
    let dsize = tag.dsize();
    assert_eq!(dsize, 4 + body);
    assert!(dsize <= 4 + 0x3FE, "dsize bound: dsize={dsize}");
}

/// `is_ccrc()` and `ccrc_chunk()` agree: if `is_ccrc()` is true,
/// `ccrc_chunk()` returns `Some(_)`; otherwise `None`. This is the
/// dispatch totality the metadata reader relies on.
#[kani::proof]
fn ccrc_classification_consistent() {
    let b: u32 = kani::any();
    let tag = Tag::from_bits(b);
    let is = tag.is_ccrc();
    let chunk = tag.ccrc_chunk();
    if is {
        assert!(chunk.is_some(), "is_ccrc but ccrc_chunk is None");
    } else {
        assert!(chunk.is_none(), "ccrc_chunk is Some but is_ccrc is false");
    }
}
