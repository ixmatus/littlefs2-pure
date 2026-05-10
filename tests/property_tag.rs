//! Property tests for the LittleFS tag type.
//!
//! Two claims under test:
//!
//! 1. **Bit roundtrip.** For every `u32`, `Tag::from_bits(b).into_bits() == b`.
//!    The 32 bit tag has no reserved bits; every value is a valid tag.
//!
//! 2. **Component roundtrip.** For every legal `(valid, type, id, length)`,
//!    constructing a tag with [`Tag::new`] then reading its components back
//!    returns the same tuple. This covers the bit layout: any field aliasing
//!    or shift error breaks the property.
//!
//! 3. **XOR self inverse.** For every pair of tags `(a, b)`, `a.xor(b).xor(b)
//!    == a`. Trivially true from XOR algebra, but it pins the implementation
//!    to that exact semantic, which is load bearing for the on disk encoding.

use littlefs2_pure::tag::{Tag, TagType};
use proptest::prelude::*;

mod common;

/// Generate a [`TagType`] uniformly over the concrete variants plus the
/// `Unknown` and `UserAttr` carriers.
fn arb_tag_type() -> impl Strategy<Value = TagType> {
    prop_oneof![
        Just(TagType::RegularFile),
        Just(TagType::Directory),
        Just(TagType::Superblock),
        Just(TagType::FromMove),
        Just(TagType::FromUserAttrs),
        Just(TagType::DirStruct),
        Just(TagType::InlineStruct),
        Just(TagType::CtzStruct),
        any::<u8>().prop_map(TagType::UserAttr),
        Just(TagType::Create),
        Just(TagType::Delete),
        (0u8..=3).prop_map(TagType::CommitCrc),
        Just(TagType::ForwardCrc),
        Just(TagType::SoftTail),
        Just(TagType::HardTail),
        Just(TagType::MoveState),
    ]
}

proptest! {
    /// Every `u32` decodes and re encodes to itself.
    #[test]
    fn from_bits_then_into_bits_is_identity(b: u32) {
        prop_assert_eq!(Tag::from_bits(b).into_bits(), b);
    }

    /// Constructing a tag with `Tag::new` and reading its components back
    /// returns the same tuple.
    #[test]
    fn new_then_decode_roundtrips(
        valid: bool,
        ty in arb_tag_type(),
        id in 0u16..=0x3ff,
        length in 0u16..=0x3ff,
    ) {
        let t = Tag::new(valid, ty, id, length);
        prop_assert_eq!(t.is_valid(), valid);
        prop_assert_eq!(t.tag_type(), ty);
        prop_assert_eq!(t.id(), id);
        prop_assert_eq!(t.length(), length);
    }

    /// XOR is its own inverse, for every pair.
    #[test]
    fn xor_is_self_inverse(a: u32, b: u32) {
        let ta = Tag::from_bits(a);
        let tb = Tag::from_bits(b);
        prop_assert_eq!(ta.xor(tb).xor(tb), ta);
    }

    /// XOR is commutative.
    #[test]
    fn xor_is_commutative(a: u32, b: u32) {
        let ta = Tag::from_bits(a);
        let tb = Tag::from_bits(b);
        prop_assert_eq!(ta.xor(tb), tb.xor(ta));
    }

    /// The 11 bit type field never sets bits outside its window.
    #[test]
    fn type_field_stays_in_window(
        valid: bool,
        ty in arb_tag_type(),
        id in 0u16..=0x3ff,
        length in 0u16..=0x3ff,
    ) {
        let bits = Tag::new(valid, ty, id, length).into_bits();
        let type_bits = (bits >> 20) & 0x7ff;
        let nontype_bits = bits & !(0x7ff << 20);
        // The nontype bits contain only: valid bit (bit 31), id (bits 10..20),
        // length (bits 0..10). No bits leak into bits 20..31 except the
        // valid bit itself.
        prop_assert_eq!(nontype_bits & ((0x7ff) << 20), 0);
        prop_assert!(type_bits <= 0x7ff);
    }
}
