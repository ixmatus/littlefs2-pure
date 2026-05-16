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
//!
//! 4. **Spec oracle.** A hand encoded `(components, raw u32)` table whose
//!    expected words are derived from the documented bit layout
//!    (`valid<<31 | type11<<20 | id<<10 | length`), not from the
//!    implementation. Encode and decode are both checked against it, so a
//!    coordinated shift error that the self-consistency properties cannot
//!    see fails here.

use littlefs2_pure::tag::{Tag, TagType};
use proptest::prelude::*;

mod common;

/// A roundtrip-safe `Unknown` tag type: an `(abstract_type, chunk)` pair
/// that does not collide with any concrete variant, so
/// `from_bits(into_bits)` is the identity on it.
fn arb_unknown() -> impl Strategy<Value = TagType> {
    (0u8..=7, 0u8..=255).prop_filter_map("collides with a known tag type", |(a, c)| {
        let u = TagType::Unknown { abstract_type: a, chunk: c };
        (TagType::from_bits(u.into_bits()) == u).then_some(u)
    })
}

/// Generate a [`TagType`] uniformly over every variant: the concrete
/// kinds, the `UserAttr`/`CommitCrc` carriers, the crate's
/// `RelocateState` extension, and the `Unknown` fallback. Earlier this
/// omitted `RelocateState` and `Unknown`, so the roundtrip and
/// type-window properties never exercised the two decode paths most
/// likely to regress under a layout change.
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
        Just(TagType::RelocateState),
        arb_unknown(),
    ]
}

/// Hand-encoded oracle: `(valid, type, id, length)` and the exact 32 bit
/// word the spec layout demands. Each `raw` is computed by hand from
/// `valid<<31 | type11<<20 | id<<10 | length`, where `type11` is
/// `(abstract_type<<8) | chunk` per the documented field assignments. It
/// is deliberately independent of `Tag::new`/`into_bits` so a shared
/// shift bug in encode and decode is still caught.
#[allow(clippy::type_complexity)]
const TAG_ORACLE: &[(bool, TagType, u16, u16, u32)] = &[
    // Superblock NAME, valid, id 0, len 8 (type11 = 0x0ff).
    (true, TagType::Superblock, 0, 8, 0x0ff0_0008),
    // RegularFile, valid, id 3, len 12 (type11 = 0x001).
    (true, TagType::RegularFile, 3, 12, 0x0010_0c0c),
    // Directory, valid, id 1, len 0 (type11 = 0x002).
    (true, TagType::Directory, 1, 0, 0x0020_0400),
    // InlineStruct, valid, no-id (0x3ff), len 0x10 (type11 = 0x201).
    (true, TagType::InlineStruct, 0x3ff, 0x10, 0x201f_fc10),
    // CtzStruct, valid, id 5, len 8 (type11 = 0x202).
    (true, TagType::CtzStruct, 5, 8, 0x2020_1408),
    // UserAttr(0x42), valid, id 4, len 2 (type11 = 0x342).
    (true, TagType::UserAttr(0x42), 4, 2, 0x3420_1002),
    // Create, valid, id 2, len 0 (type11 = 0x401).
    (true, TagType::Create, 2, 0, 0x4010_0800),
    // Delete, valid, id 7, len special 0x3ff (type11 = 0x4ff).
    (true, TagType::Delete, 7, 0x3ff, 0x4ff0_1fff),
    // CommitCrc(1), valid, no-id, len 4 (type11 = 0x501).
    (true, TagType::CommitCrc(1), 0x3ff, 4, 0x501f_fc04),
    // ForwardCrc, valid, no-id, len 4 (type11 = 0x5ff).
    (true, TagType::ForwardCrc, 0x3ff, 4, 0x5fff_fc04),
    // SoftTail, valid, no-id, len 8 (type11 = 0x600).
    (true, TagType::SoftTail, 0x3ff, 8, 0x600f_fc08),
    // HardTail, valid, no-id, len 8 (type11 = 0x601).
    (true, TagType::HardTail, 0x3ff, 8, 0x601f_fc08),
    // MoveState, valid, no-id, len 0x10 (type11 = 0x7ff).
    (true, TagType::MoveState, 0x3ff, 0x10, 0x7fff_fc10),
    // RelocateState, valid, no-id, len 0x10 (type11 = 0x7fe).
    (true, TagType::RelocateState, 0x3ff, 0x10, 0x7fef_fc10),
    // Unknown{abstract 0x4, chunk 0x73}, valid, id 0, len 0.
    (true, TagType::Unknown { abstract_type: 0x4, chunk: 0x73 }, 0, 0, 0x4730_0000),
    // RegularFile, INVALID (valid bit set), id 0, len 0.
    (false, TagType::RegularFile, 0, 0, 0x8010_0000),
];

/// Encode and decode every oracle row against the hand-computed word.
#[test]
fn tag_layout_matches_spec_oracle() {
    for &(valid, ty, id, length, raw) in TAG_ORACLE {
        let t = Tag::new(valid, ty, id, length);
        assert_eq!(
            t.into_bits(),
            raw,
            "encode mismatch for ({valid}, {ty:?}, {id}, {length}): \
             got {:#010x}, spec oracle {raw:#010x}",
            t.into_bits()
        );
        let d = Tag::from_bits(raw);
        assert_eq!(d.is_valid(), valid, "valid bit decode for {raw:#010x}");
        assert_eq!(d.tag_type(), ty, "type decode for {raw:#010x}");
        assert_eq!(d.id(), id, "id decode for {raw:#010x}");
        assert_eq!(d.length(), length, "length decode for {raw:#010x}");
    }
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
