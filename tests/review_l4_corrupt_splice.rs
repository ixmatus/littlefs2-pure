//! Review finding L4 (bead `lfs-ceq`): a metadata pair whose splice
//! stream cannot be replayed must surface as corruption, not as a missing
//! name.
//!
//! `dir::lookup` was the odd one out among the four walkers that replay
//! Create and Delete tags through `dir::splice_step` (ADR-0015).
//! `live_entries` and `fs::gather_live_slots` propagate the core's
//! rejection; `lookup` swallowed it and answered `None`, the same answer
//! a healthy pair gives for a name it does not hold. `lookup`'s `Option`
//! return is frozen for the 1.x line, so the fix adds the crate internal
//! `lookup_checked`, routes every `Fs` path through it, and leaves the
//! public wrapper answering exactly as before.
//!
//! # What lives here and what lives in the unit tests
//!
//! The red to green reproducers are unit tests, in
//! `fs::corrupt_splice_resolve_tests` and `dir::corrupt_splice_tests`,
//! because of a shadow this file pins: `Fs::mount` sweeps every reachable
//! pair through `accumulate_gstate`, which walks each one with
//! `gather_live_slots` and so already rejects these images. An image
//! corrupted this way therefore fails at mount and never reaches
//! `resolve` through the public entry points. The shadow is a property of
//! the current mount sequence rather than of `resolve`, and nothing in
//! `resolve`'s contract promises the tree was pre screened, which is why
//! the routing change is worth making and why its reproducers construct
//! the handle directly.
//!
//! This file pins the parts that are observable from outside the crate:
//! the shadow itself, the divergence between the two public walkers, the
//! frozen wrapper contract, and that healthy images are untouched.

use littlefs2_pure::block::BlockAddress;
use littlefs2_pure::meta::{Commit, MetadataPair};
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::{make_buffer, MemStorage};

/// An id no two entry root pair ever created, so the `Delete` naming it
/// is unambiguously a corrupt splice rather than a stale but replayable
/// log.
const PHANTOM_ID: u16 = 200;

/// Build a formatted image holding `/keep`, then append one more commit
/// to the root pair's active block carrying a `Delete` of
/// [`PHANTOM_ID`]. The bytes past the committed end are still erased, so
/// this is the same shape an appending writer produces, with one tag the
/// splice replay cannot accept.
fn image_with_corrupt_root_splice() -> MemStorage {
    let mut storage = healthy_image();

    let bs = MemStorage::BLOCK_SIZE;
    let block_a = storage.data[0..bs].to_vec();
    let block_b = storage.data[bs..2 * bs].to_vec();
    let (active, committed_end, next_ptag) = {
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        (pair.active_block.as_u32(), pair.reader.committed_end(), pair.reader.next_ptag())
    };

    let mut block = if active == 0 { block_a } else { block_b };
    {
        let mut commit = Commit::new_appending(&mut block, committed_end, next_ptag).unwrap();
        commit.tag(Tag::new(true, TagType::Delete, PHANTOM_ID, 0), &[]).unwrap();
        commit.finish(0).unwrap();
    }
    let base = active as usize * bs;
    storage.data[base..base + bs].copy_from_slice(&block);
    storage
}

/// The same image without the appended commit.
fn healthy_image() -> MemStorage {
    let mut storage = MemStorage::new();
    let mut scratch = make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = make_buffer();
    let mut b = make_buffer();
    fs.write_to_path(Path::new("/keep").unwrap(), b"v", &mut a, &mut b).unwrap();
    fs.into_storage()
}

fn parse(storage: &MemStorage) -> MetadataPair<'_> {
    let bs = MemStorage::BLOCK_SIZE;
    MetadataPair::parse(
        BlockAddress::new(0),
        &storage.data[0..bs],
        BlockAddress::new(1),
        &storage.data[bs..2 * bs],
    )
    .unwrap()
}

/// The corrupt commit really does verify, so the `Delete` reaches the
/// entry walk. Without this the rest of the file would be testing
/// nothing.
#[test]
fn the_corrupt_commit_is_a_verified_commit() {
    let storage = image_with_corrupt_root_splice();
    let pair = parse(&storage);
    let hits = pair
        .reader
        .iter_tags()
        .filter(|e| e.tag.tag_type() == TagType::Delete && e.tag.id() == PHANTOM_ID)
        .count();
    assert_eq!(hits, 1, "the appended Delete must sit inside a verified commit");
}

/// The two public walkers over the same pair, side by side. This is the
/// finding's literal claim: one reports corruption, the other reports
/// absence. The wrapper's answer is frozen for 1.x, so this asymmetry
/// stays; the fix is that no path inside the crate depends on it.
#[test]
fn the_two_public_walkers_disagree_on_the_same_pair() {
    let storage = image_with_corrupt_root_splice();
    let pair = parse(&storage);

    let walked = littlefs2_pure::live_entries(&pair, |_| Ok::<(), Error>(()));
    assert_eq!(walked, Err(Error::Corrupt), "the splice-correct walker reports corruption");

    assert!(
        littlefs2_pure::lookup(&pair, b"keep").is_none(),
        "the frozen wrapper reports absence for the same stream"
    );
}

/// `Fs::mount` already refuses this image, which is why the routing
/// change has no visible effect through the public entry points today.
/// Pinning it means a future change that makes mount lazier, or that
/// narrows the gstate sweep, shows up here as a behaviour change rather
/// than as a silently reopened hole.
#[test]
fn mount_already_refuses_the_corrupt_image() {
    let storage = image_with_corrupt_root_splice();
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let err = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(
        err,
        Error::Corrupt,
        "mount's whole-tree sweep is what currently shadows the resolve path"
    );
}

/// The frozen wrapper still resolves a name a healthy pair holds, and
/// still answers `None` for one it does not.
#[test]
fn public_lookup_contract_is_unchanged_on_healthy_pairs() {
    let storage = healthy_image();
    let pair = parse(&storage);
    let hit = littlefs2_pure::lookup(&pair, b"keep").expect("a healthy pair still resolves");
    assert_eq!(hit.entry.name, b"keep");
    assert_eq!(hit.entry.kind, littlefs2_pure::EntryKind::RegularFile);
    assert_eq!(hit.struct_type, TagType::InlineStruct);
    assert_eq!(hit.struct_body, b"v");
    assert!(littlefs2_pure::lookup(&pair, b"absent").is_none(), "a genuinely absent name is None");
}

/// A healthy image is unaffected by the routing change end to end:
/// resolve, read, exists, and a fresh create all behave as before.
#[test]
fn healthy_image_is_unaffected() {
    let storage = healthy_image();
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = make_buffer();
    let mut b = make_buffer();

    assert!(fs.exists(Path::new("/keep").unwrap(), &mut a, &mut b).unwrap());
    assert!(!fs.exists(Path::new("/gone").unwrap(), &mut a, &mut b).unwrap());
    let err = fs.resolve(Path::new("/gone").unwrap(), &mut a, &mut b).unwrap_err();
    assert_eq!(err, Error::NotFound, "an absent name is still NotFound on a healthy pair");

    let mut out = [0u8; 8];
    let n = fs.read_at_path(Path::new("/keep").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"v");

    fs.write_to_path(Path::new("/fresh").unwrap(), b"x", &mut a, &mut b).unwrap();
    let n = fs.read_at_path(Path::new("/fresh").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"x");
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    assert!(fs.exists(Path::new("/d").unwrap(), &mut a, &mut b).unwrap());
}
