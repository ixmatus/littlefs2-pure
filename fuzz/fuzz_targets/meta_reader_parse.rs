#![no_main]
//! Fuzz the metadata block parser on adversarial byte sequences.
//!
//! Property: `MetadataReader::new(bytes)` never panics, never reads
//! past the end of `bytes`, and produces a `committed_end <= bytes.len()`
//! on success. Either `Ok(_)` or `Err(_)` is acceptable; what matters
//! is termination without UB.
//!
//! Companions the Kani proof
//! `verify::commit_proofs::metadata_reader_does_not_panic_on_arbitrary_input`,
//! which exhausts small blocks symbolically. This fuzz extends to
//! the longer adversarial inputs Kani's loop budget cannot cover.

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::meta::MetadataReader;

fuzz_target!(|data: &[u8]| {
    if let Ok(reader) = MetadataReader::new(data) {
        // `committed_end` is the post-condition the fs kernel relies
        // on when planning the next commit's offset; a value past the
        // end of the block would corrupt subsequent writes.
        assert!(reader.committed_end() <= data.len());
        // The tag iterator must terminate; consume it.
        let mut taken = 0usize;
        for _ in reader.iter_tags() {
            taken += 1;
            if taken > 1024 {
                panic!("iter_tags did not terminate within 1024 tags");
            }
        }
    }
});
