//! Review `lfs-w3o`: the `HardTail` chase sites owe the same pair
//! address posture as the walkers.
//!
//! The posture is written down on `fs::pair_in_bounds` and pinned for
//! the walkers by `tests/review_l9_walker_pair_bounds.rs` and
//! `tests/review_qeh_pair_posture.rs`: a pair address decoded from disk
//! that names a block the device does not have is
//! [`Error::Corrupt`](littlefs2_pure::Error::Corrupt), image corruption,
//! not [`Error::Io`](littlefs2_pure::Error::Io), a hardware fault.
//!
//! The walkers honored it; the *chase* sites did not. A chase follows a
//! `HardTail` to continue one directory's own pair chain, and each site
//! handed the decoded address straight to `Storage::read`. The adapter's
//! own range check then surfaced as `Io`, so a corrupt image read as a
//! failing device. The C reference has no such split: its bounds check
//! lives inside `lfs_dir_fetchmatch` (`lfs.c:1103`) and therefore covers
//! every fetch, chases included.
//!
//! Every pair-block read in `src/fs.rs` now routes through
//! `fs::read_pair_blocks`, which rejects an out of range pair before the
//! read and leaves a genuine device rejection as `Io`. These tests reach
//! four distinct chase sites through the public surface and pin the
//! answer at each.
//!
//! # Reaching a chase site at all
//!
//! Mount rejects an out of range tail up front (the gstate accumulation
//! walk sees it first), so a chase is only reachable when the device
//! changes *underneath* a mounted handle. Each test therefore mounts a
//! healthy filesystem and then programs a higher revision commit
//! carrying the bad `HardTail` into the root pair's alternate block,
//! exactly as `review_qeh_pair_posture` does.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, MetadataReader, OpenOptions, Path};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// Inside the allocator's 4096 entry tracking bitmap, past the device's
/// 8 block count: the range a chase used to dereference.
const NEAR_OOR: (u32, u32) = (200, 201);
/// Past the tracking bitmap too. A chase dereferences this identically
/// (it has no bitmap to consult), so both ranges owe the same answer.
const FAR_OOR: (u32, u32) = (5000, 5001);

fn superblock_bodies() -> (Vec<u8>, Vec<u8>) {
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = MetadataReader::new(&block0).unwrap();
    let mut name: Option<Vec<u8>> = None;
    let mut geom: Option<Vec<u8>> = None;
    for entry in reader.iter_tags() {
        match entry.tag.tag_type() {
            TagType::Superblock => name = Some(entry.body.to_vec()),
            TagType::InlineStruct if entry.tag.id() == 0 && geom.is_none() => {
                geom = Some(entry.body.to_vec());
            }
            _ => {}
        }
    }
    (name.expect("superblock NAME"), geom.expect("geometry struct"))
}

fn pair_body(pair: (u32, u32)) -> [u8; 8] {
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&pair.0.to_le_bytes());
    body[4..8].copy_from_slice(&pair.1.to_le_bytes());
    body
}

/// A healthy mounted filesystem whose root pair's alternate block has
/// since been overwritten with a higher revision commit whose `HardTail`
/// names `target`, a pair the device does not have.
///
/// The mount happened while the image was sound, so this models a device
/// that develops corruption after mount: precisely the state in which a
/// chase, rather than a mount time walk, is the first reader of the bad
/// address.
fn mounted_over_corrupt_hard_tail(target: (u32, u32)) -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    let (sb_name, sb_geom) = superblock_bodies();
    let mut blk = BlockBuilder::new(MemStorage::BLOCK_SIZE, 99).unwrap();
    blk.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    blk.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    blk.tag(Tag::new(true, TagType::HardTail, 0x3FF, 8), &pair_body(target)).unwrap();
    blk.commit(0).unwrap();
    let bytes = blk.finish();
    {
        let s = fs.storage_mut();
        s.erase(1).unwrap();
        for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
            s.program(1, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
        }
    }
    fs
}

/// `Fs::list_root` -> `list_pair_chain`: the chase that walks a
/// directory's whole pair chain emitting entries.
#[test]
fn list_pair_chain_rejects_an_out_of_range_hard_tail() {
    for target in [NEAR_OOR, FAR_OOR] {
        let mut fs = mounted_over_corrupt_hard_tail(target);
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let err = fs
            .list_root(|_| {}, &mut a, &mut b)
            .expect_err("a chase over an out of range pair must not succeed");
        assert_eq!(
            err,
            Error::Corrupt,
            "list_pair_chain: an out of range HardTail is image corruption, \
             not a device fault ({target:?})"
        );
    }
}

/// `Fs::resolve` on a single component path: the final component loop
/// chases `HardTail`s itself, without going through `find_dir_pair`.
#[test]
fn resolve_final_component_chase_rejects_an_out_of_range_hard_tail() {
    for target in [NEAR_OOR, FAR_OOR] {
        let mut fs = mounted_over_corrupt_hard_tail(target);
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let err = fs
            .resolve(Path::new("/ghost").unwrap(), &mut a, &mut b)
            .map(|_| ())
            .expect_err("a chase over an out of range pair must not succeed");
        assert_eq!(err, Error::Corrupt, "resolve ({target:?})");
    }
}

/// `Fs::resolve` on a multi component path: the intermediate component
/// descends through `find_dir_pair`, a separate chase loop.
#[test]
fn find_dir_pair_chase_rejects_an_out_of_range_hard_tail() {
    for target in [NEAR_OOR, FAR_OOR] {
        let mut fs = mounted_over_corrupt_hard_tail(target);
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        // Two components: "ghost" resolves through `find_dir_pair`,
        // which chases the root's HardTail looking for the name.
        let err = fs
            .exists(Path::new("/ghost/leaf").unwrap(), &mut a, &mut b)
            .map(|_| ())
            .expect_err("a chase over an out of range pair must not succeed");
        assert_eq!(err, Error::Corrupt, "find_dir_pair ({target:?})");
    }
}

/// `Fs::open` -> `seek_entry_in_chain`: the chase every write path uses
/// to find (or fail to find) a name across a directory's chain.
#[test]
fn seek_entry_in_chain_chase_rejects_an_out_of_range_hard_tail() {
    for target in [NEAR_OOR, FAR_OOR] {
        let mut fs = mounted_over_corrupt_hard_tail(target);
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let err = fs
            .open(
                Path::new("/ghost").unwrap(),
                OpenOptions::new().read(true).write(true).create(true),
                &mut a,
                &mut b,
            )
            .map(|_| ())
            .expect_err("a chase over an out of range pair must not succeed");
        assert_eq!(err, Error::Corrupt, "seek_entry_in_chain via open ({target:?})");
    }
}

/// `Fs::mkdir` reaches `seek_entry_in_chain` first and must not fall
/// through to allocating for a directory whose parent chain is corrupt.
#[test]
fn mkdir_over_a_corrupt_chain_rejects_before_allocating() {
    for target in [NEAR_OOR, FAR_OOR] {
        let mut fs = mounted_over_corrupt_hard_tail(target);
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let err = fs
            .mkdir(Path::new("/newdir").unwrap(), &mut a, &mut b)
            .expect_err("a chase over an out of range pair must not succeed");
        assert_eq!(err, Error::Corrupt, "mkdir ({target:?})");
    }
}

/// The counterweight, carried over from the walker suites: rejecting
/// every address a chase cannot fetch would also reject the all ones
/// sentinel, which is thread end rather than an address (the `lfs-yl6`
/// regression). A healthy filesystem must still list, resolve and open.
#[test]
fn a_healthy_filesystem_is_unaffected() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    fs.write_to_path(Path::new("/f").unwrap(), b"hello", &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let mut seen = 0usize;
    fs.list_root(|_| seen += 1, &mut a, &mut b).unwrap();
    assert_eq!(seen, 2, "both entries list");
    assert!(fs.exists(Path::new("/f").unwrap(), &mut a, &mut b).unwrap());
    assert!(fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).unwrap());
    assert!(!fs.exists(Path::new("/nope").unwrap(), &mut a, &mut b).unwrap());
    let mut out = [0u8; 16];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"hello");
}
