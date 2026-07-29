//! The 128 byte block size floor and the rest of the geometry
//! preconditions (`lfs-cw1`).
//!
//! The C reference asserts `block_size >= 128` on the first line of
//! mount (`tools/gen_vectors/littlefs/lfs.c:4189`), under the comment
//! that the block has to fit all CTZ pointers: the widest skip pointer
//! header is `4 * (ctz(0x8000_0000) + 1) = 128` bytes. Before this
//! suite nothing in this crate enforced it. `Fs::format` and `Fs::mount`
//! accepted a 64 byte device and produced an image the C reference
//! refuses to mount, and `ctz::content_bytes_in_block` underflowed:
//! abort in a debug build, wrap to 4294967232 in a release build, with
//! the caller sizing a read from that number.
//!
//! Two gates now stand in the way, and this file exercises the one that
//! can be exercised from a test. The `Fs` surface is gated at **compile
//! time**: `Fs::mount` and `Fs::format` name
//! `geometry::Geometry::<S>::CHECK`, so a sub floor device does not
//! build. A test calling `Fs::mount::<Tiny>` would therefore not
//! compile, which is the point; the compile failure is pinned instead by
//! the `compile_fail,E0080` doctests on `Geometry::CHECK`, which
//! `cargo test --doc` runs.
//!
//! What remains testable at runtime, and is tested here, is the
//! predicate itself over a fault table, and the guard on
//! `ctz::read_ctz_at`, the one public entry point that computes a per
//! block content capacity from a raw `Storage` without a mount.

mod common;

use common::MemStorageG;
use littlefs2_pure::ctz::{self, CtzStruct};
use littlefs2_pure::geometry::{self, GeometryFault, BLOCK_SIZE_MIN};
use littlefs2_pure::{BlockAddress, Error, Fs, Path, Storage};

/// A device below the floor. 64 bytes is the geometry that formatted
/// and mounted cleanly before `lfs-cw1`.
type Tiny = MemStorageG<64, 16, 16>;

/// A device exactly at the floor. Everything the kernel does must keep
/// working here, otherwise the floor would be a fiction.
type Floor = MemStorageG<128, 16, 16>;

#[test]
fn the_sub_floor_device_is_rejected_by_the_predicate() {
    assert_eq!(geometry::fault::<Tiny>(), Some(GeometryFault::BlockSizeBelowFloor));
    assert_eq!(geometry::validate::<Tiny>(), Err(Error::GeometryMismatch));
}

#[test]
fn the_floor_geometry_passes_the_predicate() {
    assert_eq!(geometry::fault::<Floor>(), None);
    assert_eq!(geometry::validate::<Floor>(), Ok(()));
    assert_eq!(<Floor as Storage>::BLOCK_SIZE, BLOCK_SIZE_MIN);
}

/// The read path's own guard. `read_ctz_at` computes
/// `BLOCK_SIZE - 4 * skip_pointers_in_block(i)` per chain block, so it
/// must refuse a sub floor device before the subtraction rather than
/// after it. The refusal precedes all I/O: the device here is blank, so
/// a walk that got past the guard would fail with `Corrupt` (an all
/// ones skip pointer) instead.
#[test]
fn read_ctz_at_refuses_a_sub_floor_device() {
    let mut dev = Tiny::new();
    let mut scratch = [0u8; 64];
    let mut out = [0u8; 32];
    let ctz = CtzStruct { head_block: BlockAddress::new(3), size: 64 };
    assert_eq!(
        ctz::read_ctz_at(&mut dev, &ctz, 0, &mut out, &mut scratch),
        Err(Error::GeometryMismatch)
    );
}

/// The same call at the floor gets past the guard and reads, so the
/// guard is not simply refusing everything. A 64 byte file at a 128
/// byte geometry is one chain block, index 0, which carries no skip
/// pointer header, so the read serves the erased device's bytes.
#[test]
fn read_ctz_at_admits_the_floor_geometry() {
    let mut dev = Floor::new();
    let mut scratch = [0u8; 128];
    let mut out = [0u8; 32];
    let ctz = CtzStruct { head_block: BlockAddress::new(3), size: 64 };
    assert_eq!(ctz::read_ctz_at(&mut dev, &ctz, 0, &mut out, &mut scratch), Ok(32));
    assert_eq!(out, [0xFFu8; 32]);
}

/// End to end at the floor: format, mount, write, read back. The gate
/// rejects everything below 128 bytes, so 128 itself has to work.
#[test]
fn a_floor_sized_device_formats_and_mounts() {
    let mut dev = Floor::new();
    let mut scratch = [0u8; 128];
    Fs::format(&mut dev, &mut scratch).expect("format at the floor geometry");

    let mut buf_a = [0u8; 128];
    let mut buf_b = [0u8; 128];
    let mut fs = Fs::mount(dev, &mut buf_a, &mut buf_b).expect("mount at the floor geometry");
    assert_eq!(fs.superblock().block_size, 128);

    fs.write_to_root(b"f", b"floor", &mut buf_a, &mut buf_b).expect("write at the floor");
    let mut out = [0u8; 8];
    let path = Path::new("/f").unwrap();
    let n = fs.read_at_path(path, 0, &mut out, &mut buf_a, &mut buf_b).expect("read at the floor");
    assert_eq!(&out[..n], b"floor");
}

/// The whole fault table, driven through the public `Storage` doubles
/// rather than the module's private ones, so the crate's own users can
/// see which geometries are refused.
#[test]
fn the_other_preconditions_are_refused_too() {
    // Block size off the program grid: 200 is not a multiple of 16.
    assert_eq!(
        geometry::fault::<MemStorageG<200, 16, 16>>(),
        Some(GeometryFault::BlockSizeOffTheProgGrid)
    );
    // Fewer blocks than the fixed root pair needs.
    assert_eq!(
        geometry::fault::<MemStorageG<256, 16, 1>>(),
        Some(GeometryFault::BlockCountBelowRootPair)
    );
    // The geometry every other suite in this repository runs at.
    assert_eq!(geometry::fault::<MemStorageG<256, 16, 8>>(), None);
}
