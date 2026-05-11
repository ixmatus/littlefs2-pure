#![no_main]
//! Fuzz `CtzStruct::from_bytes` + `to_bytes` on arbitrary 8-byte
//! bodies.
//!
//! Property: every 8-byte slice decodes (round-trips), every other
//! length errors cleanly. `to_bytes` of a decoded struct produces
//! the original 8 bytes.

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::ctz::CtzStruct;

fuzz_target!(|data: &[u8]| {
    match CtzStruct::from_bytes(data) {
        Ok(ctz) => {
            // Round-trip.
            let encoded = ctz.to_bytes();
            assert_eq!(&encoded[..], data, "from_bytes/to_bytes are inverses");
        }
        Err(_) => {
            // Validation rejected; that's fine for non-8-byte inputs.
            assert_ne!(
                data.len(),
                CtzStruct::SIZE,
                "8-byte input must decode"
            );
        }
    }
});
