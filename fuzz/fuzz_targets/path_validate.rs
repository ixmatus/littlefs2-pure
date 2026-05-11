#![no_main]
//! Fuzz `Path::new` with arbitrary UTF-8 inputs.
//!
//! Property: validation either accepts the input or returns
//! `Error::InvalidPath`; it never panics. Accepted paths satisfy
//! every documented rule (non-empty, components <= NAME_MAX,
//! no `.`/`..` components, no `//`).

use libfuzzer_sys::fuzz_target;
use littlefs2_pure::Path;

fuzz_target!(|input: &str| {
    // The harness uses `&str` so libfuzzer's UTF-8 validity check
    // handles us at the boundary; the validator should still reject
    // every disallowed input.
    if let Ok(path) = Path::new(input) {
        // Documented invariants the constructor must enforce.
        let s = path.as_str();
        assert!(!s.is_empty(), "constructed Path is non-empty");
        assert!(s.len() <= littlefs2_pure::path::MAX_PATH);
        for component in path.components() {
            assert!(!component.is_empty(), "no empty components");
            assert!(component.len() <= littlefs2_pure::NAME_MAX);
            assert_ne!(component, ".");
            assert_ne!(component, "..");
            assert!(!component.contains('/'));
        }
    }
});
