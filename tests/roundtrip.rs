//! Round-trip conformance: produce a LittleFS image with this crate's
//! writer, then mount it via the C reference and validate the
//! expected content. Companion to `tests/conformance.rs` which
//! exercises the opposite direction (C produces, Rust reads).
//!
//! The verifier binary lives at `tools/verify_image/build/verify_image`
//! and must be built before the test runs (`make -C tools/verify_image`).
//! When the binary is missing, individual tests are skipped with a
//! note rather than failing, so CI can run the rest of the suite on
//! systems without a C toolchain.
//!
//! Three scenarios mirror the C-to-Rust suite so the cross-mount
//! property holds bidirectionally:
//!
//! - `inline`: `/cfg = "hello, rust"`
//! - `ctz`:    `/payload.bin = (i & 0xff) for i in 0..500`
//! - `nested`: `/audit/log = "entry-0001;"`

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use littlefs2_pure::{Fs, Path};

mod common;
use common::MemStorage;

fn verifier_path() -> PathBuf {
    PathBuf::from("tools/verify_image/build/verify_image")
}

/// Returns the verifier binary path if present, else logs a skip
/// note and returns `None`. Tests `return` early when this returns
/// `None`.
fn require_verifier() -> Option<PathBuf> {
    let p = verifier_path();
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "round-trip test skipped: verifier binary not found at {}. \
             Build with `make -C tools/verify_image` to enable.",
            p.display()
        );
        None
    }
}

/// Write a 2 KiB image (MemStorage's geometry) to a temp file and
/// return the path. The caller picks a `label` distinct from any
/// concurrent test's label so parallel runs do not collide on the
/// same path.
fn dump_image(image: &MemStorage, label: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!(
        "littlefs2-pure-roundtrip-{}-{}.bin",
        std::process::id(),
        label
    ));
    let mut f = std::fs::File::create(&tmp).expect("create tmp image");
    f.write_all(&image.data).expect("write tmp image");
    tmp
}

fn invoke_verifier(verifier: &std::path::Path, image: &std::path::Path, scenario: &str) {
    let status =
        Command::new(verifier).arg(image).arg(scenario).status().expect("invoke verify_image");
    assert!(
        status.success(),
        "verify_image rejected our image for scenario {scenario}: status = {status:?}"
    );
}

#[test]
fn roundtrip_inline_file() {
    let Some(verifier) = require_verifier() else { return };
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.write_to_path(Path::new("/cfg").unwrap(), b"hello, rust", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let img = dump_image(&storage, "inline");
    invoke_verifier(&verifier, &img, "inline");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn roundtrip_ctz_file() {
    let Some(verifier) = require_verifier() else { return };
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let body: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
        fs.write_to_path(Path::new("/payload.bin").unwrap(), &body, &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let img = dump_image(&storage, "ctz");
    invoke_verifier(&verifier, &img, "ctz");
    let _ = std::fs::remove_file(&img);
}

#[test]
fn roundtrip_nested_dir() {
    let Some(verifier) = require_verifier() else { return };
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/audit/log").unwrap(), b"entry-0001;", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let img = dump_image(&storage, "nested");
    invoke_verifier(&verifier, &img, "nested");
    let _ = std::fs::remove_file(&img);
}
