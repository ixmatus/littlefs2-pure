//! Review L9 (`lfs-b2m`): the allocator's forest walkers must reject an
//! out of range metadata pair address as [`Error::Corrupt`] before
//! dereferencing it.
//!
//! The [`Storage`] trait's own documentation states the contract that is
//! under test here: "The kernel defensively rejects out-of-range pair
//! addresses and CTZ skip pointers before dereferencing them, classifying
//! such an address as `crate::Error::Corrupt`, but it still relies on this
//! trait contract as the final backstop." Every pair pointer decoded from
//! disk in `src/fs.rs` honors that with a `pair_in_bounds` check. The two
//! walkers in `src/alloc.rs` did not: they enqueued a `DirStruct` body or
//! a tail body straight from disk, so a corrupt or adversarial image drove
//! a read of a block the device does not have. The storage adapter's own
//! range check then surfaced as [`Error::Io`], which reads as a hardware
//! fault rather than as the image corruption it is.
//!
//! The C reference draws the same line and draws it as `CORRUPT`:
//! `lfs_dir_fetchmatch` opens with
//! `if (lfs->block_count && (pair[0] >= lfs->block_count || pair[1] >=
//! lfs->block_count)) return LFS_ERR_CORRUPT;`.
//!
//! Two address ranges behave differently before the fix and both are
//! covered below. An address inside the allocator's 4096 entry tracking
//! bitmap but past `BLOCK_COUNT` was dereferenced and produced `Io`. An
//! address past the bitmap was silently swallowed instead, because
//! `Bitmap::is_set` answers `true` out of range and the "already marked"
//! guard then skipped the enqueue. Neither is the documented rejection.

use littlefs2_pure::alloc::{scan_used_blocks, scan_used_with_single_buf, Bitmap};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{BlockAddress, BlockPair, Error, Fs, ROOT_BLOCK_PAIR};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

/// Inside the allocator's tracking bitmap, past the device's block count.
/// This is the range the walkers used to dereference.
const NEAR_OOR: (u32, u32) = (200, 201);
/// Past the allocator's tracking bitmap as well. This is the range the
/// walkers used to swallow silently.
const FAR_OOR: (u32, u32) = (5000, 5001);

fn superblock_bodies() -> (Vec<u8>, Vec<u8>) {
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = littlefs2_pure::MetadataReader::new(&block0).unwrap();
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
    (
        name.expect("formatted image carries a superblock NAME"),
        geom.expect("formatted image carries the geometry struct"),
    )
}

fn pair_body(pair: (u32, u32)) -> [u8; 8] {
    let mut body = [0u8; 8];
    body[0..4].copy_from_slice(&pair.0.to_le_bytes());
    body[4..8].copy_from_slice(&pair.1.to_le_bytes());
    body
}

fn program_block(storage: &mut MemStorage, block: u32, bytes: &[u8]) {
    for (i, chunk) in bytes.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(block, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }
}

/// How the crafted root references the out of range pair.
#[derive(Clone, Copy)]
enum Reference {
    /// A live `DirStruct` on entry id 1.
    LiveChild,
    /// A `SoftTail` on the pair itself.
    Tail,
    /// A `DirStruct` on entry id 1 that a later `Delete` splices away, so
    /// the splice correct view never sees it but a raw tag scan does.
    DeletedChild,
}

/// Build an image whose root pair is superblock valid and whose log
/// references `target` in the requested way.
fn image_referencing(target: (u32, u32), how: Reference) -> MemStorage {
    let (sb_name, sb_geom) = superblock_bodies();
    let body = pair_body(target);

    let mut root = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    root.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    root.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    match how {
        Reference::LiveChild => {
            root.tag(Tag::new(true, TagType::Directory, 1, 1), b"d").unwrap();
            root.tag(Tag::new(true, TagType::DirStruct, 1, 8), &body).unwrap();
        }
        Reference::Tail => {
            root.tag(Tag::new(true, TagType::SoftTail, 0x3FF, 8), &body).unwrap();
        }
        Reference::DeletedChild => {
            root.tag(Tag::new(true, TagType::Directory, 1, 1), b"d").unwrap();
            root.tag(Tag::new(true, TagType::DirStruct, 1, 8), &body).unwrap();
            root.tag(Tag::new(true, TagType::Delete, 1, 0), &[]).unwrap();
        }
    }
    root.commit(0).unwrap();
    let root_bytes = root.finish();

    let mut storage = MemStorage::new();
    // Block 0 holds the commit; block 1 stays erased so block 0 is active.
    program_block(&mut storage, 0, &root_bytes);
    storage
}

fn two_buffer_scan(storage: &mut MemStorage) -> Result<(), Error> {
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b)
}

fn single_buffer_scan(storage: &mut MemStorage) -> Result<(), Error> {
    let mut used = Bitmap::EMPTY;
    let mut buf = common::make_buffer();
    scan_used_with_single_buf(storage, ROOT_BLOCK_PAIR, &mut used, &mut buf)
}

#[test]
fn two_buffer_scan_rejects_out_of_range_dir_struct() {
    // Before the fix this dereferenced block 200 and returned Io.
    let mut storage = image_referencing(NEAR_OOR, Reference::LiveChild);
    let err = two_buffer_scan(&mut storage)
        .expect_err("an out of range DirStruct child must not be walked");
    assert_eq!(
        err,
        Error::Corrupt,
        "an out of range pair address is image corruption, not a device fault"
    );
}

#[test]
fn two_buffer_scan_rejects_out_of_range_tail() {
    // Before the fix this dereferenced block 200 and returned Io.
    let mut storage = image_referencing(NEAR_OOR, Reference::Tail);
    let err =
        two_buffer_scan(&mut storage).expect_err("an out of range tail pair must not be walked");
    assert_eq!(err, Error::Corrupt, "a tail is a decoded on disk pointer like any other");
}

#[test]
fn single_buffer_scan_rejects_out_of_range_dir_struct() {
    // Before the fix this dereferenced block 200 and returned Io.
    let mut storage = image_referencing(NEAR_OOR, Reference::LiveChild);
    let err = single_buffer_scan(&mut storage)
        .expect_err("an out of range DirStruct child must not be walked");
    assert_eq!(err, Error::Corrupt, "the single buffer walker owes the same rejection");
}

#[test]
fn two_buffer_scan_rejects_pair_past_the_tracking_bitmap() {
    // Before the fix `Bitmap::is_set` answered true for both blocks, the
    // "already marked" guard skipped the enqueue, and the scan returned
    // Ok while silently accepting a corrupt pointer.
    let mut storage = image_referencing(FAR_OOR, Reference::LiveChild);
    let err = two_buffer_scan(&mut storage)
        .expect_err("a pair past the tracking bitmap must be rejected, not swallowed");
    assert_eq!(err, Error::Corrupt, "silently accepting a corrupt pointer is not a rejection");
}

#[test]
fn single_buffer_scan_rejects_pair_past_the_tracking_bitmap() {
    let mut storage = image_referencing(FAR_OOR, Reference::LiveChild);
    let err = single_buffer_scan(&mut storage)
        .expect_err("a pair past the tracking bitmap must be rejected, not swallowed");
    assert_eq!(err, Error::Corrupt, "silently accepting a corrupt pointer is not a rejection");
}

#[test]
fn mount_accepts_an_image_whose_raw_log_holds_an_out_of_range_pair() {
    // Reachability. The single buffer walker iterates RAW tags, so it
    // follows a `DirStruct` that a later `Delete` spliced away. Mount's
    // walks are splice correct and never see that stale body, so the
    // filesystem opens cleanly and the bad pointer stays live in the log
    // until the next relocation runs the single buffer scan over it. The
    // walker is therefore the only place the rejection can happen.
    let storage = image_referencing(NEAR_OOR, Reference::DeletedChild);
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("a spliced away DirStruct must not stop mount");
    let mut storage = fs.into_storage();

    // The splice correct walker agrees with mount and never sees it.
    two_buffer_scan(&mut storage).expect("the live walk must not see a spliced away entry");

    // The raw walker does see it, and must call it corruption.
    let err = single_buffer_scan(&mut storage)
        .expect_err("the raw walker reaches the stale out of range DirStruct");
    assert_eq!(err, Error::Corrupt, "reached from a mountable image, so the rejection must hold");
}

#[test]
fn in_range_pair_addresses_still_walk() {
    // Guard against the bounds check rejecting legitimate images: an
    // ordinary formatted filesystem with a real subdirectory must still
    // scan clean.
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    fs.mkdir(littlefs2_pure::Path::new("/d").unwrap(), &mut buf_a, &mut buf_b).unwrap();
    let mut storage = fs.into_storage();

    two_buffer_scan(&mut storage).expect("a healthy image must scan clean");
    single_buffer_scan(&mut storage).expect("a healthy image must scan clean");

    // And the child pair really is in range, so the test is not vacuous.
    let mut used = Bitmap::EMPTY;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    scan_used_blocks(&mut storage, ROOT_BLOCK_PAIR, &mut used, &mut a, &mut b).unwrap();
    let marked: Vec<u32> = (0..MemStorage::BLOCK_COUNT).filter(|&x| used.is_set(x)).collect();
    assert!(marked.len() >= 4, "root pair plus the subdirectory pair must be marked: {marked:?}");
    let _ = BlockPair::new(BlockAddress::new(0), BlockAddress::new(1));
}
