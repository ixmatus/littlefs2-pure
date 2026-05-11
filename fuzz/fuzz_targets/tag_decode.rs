#![no_main]
//! Fuzz the tag decode + classification surface.
//!
//! Property: `Tag::from_bits(bits)` never panics, and every accessor
//! on the resulting tag returns a finite value. Round-trip: a
//! `TagType::from_bits` followed by `into_bits` recovers the
//! requested 11-bit type field.

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::tag::{Tag, TagType};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let bits = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let tag = Tag::from_bits(bits);

    // Every accessor must return without panicking.
    let _ = tag.is_valid();
    let _ = tag.tag_type();
    let _ = tag.id();
    let _ = tag.length();
    let _ = tag.body_len();
    let _ = tag.dsize();
    let _ = tag.is_ccrc();
    let _ = tag.ccrc_chunk();

    // Round-trip the type field.
    let type_field = ((bits >> 20) & 0x7FF) as u16;
    let decoded = TagType::from_bits(type_field);
    assert_eq!(decoded.into_bits(), type_field);

    // dsize() and body_len() agree.
    assert_eq!(tag.dsize(), 4 + tag.body_len());
});
