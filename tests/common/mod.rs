//! Shared helpers for the property test files. Lives at `tests/common/mod.rs`
//! so each `tests/property_*.rs` file can pull it in via `mod common;` without
//! Cargo treating it as a separate integration test binary.
//!
//! The pattern is borrowed from ferrodec's testing layout. Helpers here are
//! split across consumers, so a blanket `#[allow(dead_code)]` keeps unused
//! warnings off in files that import only one helper.

#![allow(dead_code)]
