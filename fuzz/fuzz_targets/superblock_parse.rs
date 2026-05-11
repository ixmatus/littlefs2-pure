#![no_main]
//! Fuzz superblock decoding on arbitrary 24-byte bodies.
//!
//! Property: `Superblock::from_bytes` never panics. Either returns a
//! valid `Superblock` or an error. The mount-level wrapper
//! `Superblock::from_pair` is covered indirectly by the
//! `meta_reader_parse` target (it composes through `from_pair` to
//! `MetadataReader::iter_tags`); here we exercise the body decoder
//! directly.

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::Superblock;

fuzz_target!(|data: &[u8]| {
    // Wire format is 24 bytes; anything else should error cleanly.
    let _ = Superblock::from_bytes(data);
});
