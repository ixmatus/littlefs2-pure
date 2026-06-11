//! Reproducers for the 2026-06 deep review findings C1, C2, C5, and H1
//! (beads lfs-2dg, lfs-3z8, lfs-fb2, lfs-r88).
//!
//! Each test failed against the v1.2.0 tree before the corresponding
//! fix landed; together they pin the splice and attribute remediation
//! arc. The oracle for every expected behavior is the vendored C
//! reference (`tools/gen_vectors/littlefs/lfs.c`):
//!
//! - C1: `lfs_dir_compact` replays every unique tag per live id,
//!   user attributes included (lfs.c, `lfs_dir_traverse` filter).
//! - C2: `lfs_dir_getslice` carries a splice diff (`gdiff`) across
//!   every SPLICE tag, so attribute reads track entry renumbering.
//! - C5: `lfs_fs_parent` resolves through `lfs_dir_fetchmatch`, whose
//!   `tempbesttag` is splice-corrected and delete-invalidated.
//! - H1: `lfs_dir_fetchmatch` accepts a NAME tag at any id and bumps
//!   the entry count to `max(id + 1, count)`; compaction emits
//!   surviving tags in log order, which is not id order after a
//!   rename, so a reader requiring id-dense NAME order rejects valid
//!   C images.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Fs, Path};

mod common;
use common::{BlockBuilder, MemStorage};

extern crate alloc;
use alloc::vec;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

fn get_attr_vec(fs: &mut Fs<MemStorage>, path: &str, attr_id: u8) -> alloc::vec::Vec<u8> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 64];
    let n = fs.get_attr(p(path), attr_id, &mut out, &mut a, &mut b).unwrap();
    out[..n].to_vec()
}

// ---------------------------------------------------------------------
// C1: compaction must preserve user attributes.
// ---------------------------------------------------------------------

/// Set an attribute, then force the root pair through at least one
/// compaction with content updates. The attribute must survive.
///
/// Geometry: 256-byte blocks. Each update commit is at least one
/// 16-byte prog window plus CCRC padding, so a dozen updates push the
/// active block past capacity several times over; at least one
/// append-to-compact rotation is guaranteed.
#[test]
fn c1_attr_survives_compaction() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/f"), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(p("/f"), 7, b"secret", &mut a, &mut b).unwrap();

    for i in 0..16u8 {
        let content = [i; 24];
        fs.write_to_path(p("/f"), &content, &mut a, &mut b).unwrap();
    }

    assert_eq!(
        get_attr_vec(&mut fs, "/f", 7),
        b"secret".to_vec(),
        "C1: compaction stripped the user attribute"
    );
    // The file content must be the last write.
    let r = fs.resolve(p("/f"), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, &[15u8; 24]);
}

/// A `set_attr` that itself lands when the active block is full takes
/// the compact path. It must persist the new value (the review found
/// it returned `Ok(())` while persisting nothing).
#[test]
fn c1_set_attr_on_full_block_persists() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/f"), b"x", &mut a, &mut b).unwrap();

    // Interleave: every set_attr is followed by filler updates, so at
    // least one set_attr in the sequence falls on a full block and
    // compacts. The last set_attr value must win regardless.
    for round in 0..12u8 {
        let val = [b'A' + round; 6];
        fs.set_attr(p("/f"), 9, &val, &mut a, &mut b).unwrap();
        let filler = [round; 24];
        fs.write_to_path(p("/f"), &filler, &mut a, &mut b).unwrap();
    }

    assert_eq!(
        get_attr_vec(&mut fs, "/f", 9),
        vec![b'A' + 11; 6],
        "C1: set_attr on the compact path did not persist"
    );
}

/// Attribute removal must also survive compaction: a removed attribute
/// stays removed after the pair rotates.
#[test]
fn c1_removed_attr_stays_removed_across_compaction() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/f"), b"x", &mut a, &mut b).unwrap();
    fs.set_attr(p("/f"), 3, b"present", &mut a, &mut b).unwrap();
    fs.remove_attr(p("/f"), 3, &mut a, &mut b).unwrap();

    for i in 0..16u8 {
        let content = [i; 24];
        fs.write_to_path(p("/f"), &content, &mut a, &mut b).unwrap();
    }

    assert_eq!(
        get_attr_vec(&mut fs, "/f", 3),
        alloc::vec::Vec::<u8>::new(),
        "a removed attribute reappeared after compaction"
    );
}

/// Attributes on multiple entries each survive compaction and stay
/// attached to their own entry.
#[test]
fn c1_attrs_stay_with_their_entries_across_compaction() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/one"), b"1", &mut a, &mut b).unwrap();
    fs.write_to_path(p("/two"), b"2", &mut a, &mut b).unwrap();
    fs.set_attr(p("/one"), 1, b"first", &mut a, &mut b).unwrap();
    fs.set_attr(p("/two"), 1, b"second", &mut a, &mut b).unwrap();

    for i in 0..16u8 {
        let content = [i; 24];
        fs.write_to_path(p("/one"), &content, &mut a, &mut b).unwrap();
    }

    assert_eq!(get_attr_vec(&mut fs, "/one", 1), b"first".to_vec());
    assert_eq!(get_attr_vec(&mut fs, "/two", 1), b"second".to_vec());
}

// ---------------------------------------------------------------------
// C2: attribute reads must be splice-aware.
// ---------------------------------------------------------------------

/// Deleting a lower-id entry renumbers the survivors. The attribute
/// must still be found under the entry's new live id (it "vanished" in
/// v1.2.0), and an entry created afterward that reuses the raw id must
/// NOT see the old entry's attribute (the cross-entry leak).
#[test]
fn c2_attr_tracks_entry_across_delete_and_does_not_leak() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    // Root pair ids: superblock = 0, /a = 1, /b = 2.
    fs.write_to_path(p("/a"), b"aaa", &mut a, &mut b).unwrap();
    fs.write_to_path(p("/b"), b"bbb", &mut a, &mut b).unwrap();
    fs.set_attr(p("/b"), 7, b"bee", &mut a, &mut b).unwrap();

    // Delete /a: /b renumbers from live id 2 to live id 1, while the
    // committed attr tag still carries raw id 2.
    fs.remove_at_path(p("/a"), &mut a, &mut b).unwrap();

    assert_eq!(
        get_attr_vec(&mut fs, "/b", 7),
        b"bee".to_vec(),
        "C2: attribute vanished after a lower-id delete"
    );

    // Create /c: it takes live id 2, the raw id the old attr tag
    // carries. It must not inherit /b's attribute.
    fs.write_to_path(p("/c"), b"ccc", &mut a, &mut b).unwrap();
    assert_eq!(
        get_attr_vec(&mut fs, "/c", 7),
        alloc::vec::Vec::<u8>::new(),
        "C2: attribute leaked across entries via raw id reuse"
    );
    // And /b still reads its own.
    assert_eq!(get_attr_vec(&mut fs, "/b", 7), b"bee".to_vec());
}

/// Same shape with the attribute set, the entry renumbered TWICE
/// (two deletes below it), exercising an accumulated splice diff.
#[test]
fn c2_attr_tracks_entry_across_two_deletes() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(p("/a"), b"aaa", &mut a, &mut b).unwrap();
    fs.write_to_path(p("/b"), b"bbb", &mut a, &mut b).unwrap();
    fs.write_to_path(p("/c"), b"ccc", &mut a, &mut b).unwrap();
    fs.set_attr(p("/c"), 5, b"sea", &mut a, &mut b).unwrap();

    fs.remove_at_path(p("/a"), &mut a, &mut b).unwrap();
    fs.remove_at_path(p("/b"), &mut a, &mut b).unwrap();

    assert_eq!(
        get_attr_vec(&mut fs, "/c", 5),
        b"sea".to_vec(),
        "C2: attribute lost across two renumbering deletes"
    );
}

// ---------------------------------------------------------------------
// C5: relocation must repoint the parent's LIVE entry.
// ---------------------------------------------------------------------

/// 32-block geometry with aggressive wear levelling (`BLOCK_CYCLES =
/// 1`), copied from `tests/wear_leveling.rs`. Relocation fires on
/// every third compaction of a non-root pair.
#[derive(Debug)]
struct WearStorage {
    data: alloc::vec::Vec<u8>,
}

impl WearStorage {
    fn new() -> Self {
        Self { data: vec![0xFFu8; 256 * 32] }
    }
}

impl Storage for WearStorage {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 32;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    const BLOCK_CYCLES: i32 = 1;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + buf.len() > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + (off as usize);
        if start + data.len() > self.data.len() {
            return Err(());
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        if end > self.data.len() {
            return Err(());
        }
        for v in &mut self.data[start..end] {
            *v = 0xFF;
        }
        Ok(())
    }
}

/// The parent's log holds the child's DirStruct at raw id 2, then a
/// Delete at id 1, so the child's live id is 1 and a sibling file
/// holds live id 2. When the child pair relocates, the parent update
/// must hit the child's live entry, not the sibling's.
///
/// Before the fix, `find_parent_in_tree` returned the raw id 2 and
/// `propagate_relocation` rewrote the sibling's struct body into a
/// DirStruct (destroying the file) while the child's entry kept its
/// stale pair address.
#[test]
fn c5_relocation_repoints_live_entry_not_raw_id() {
    let mut storage = WearStorage::new();
    let mut scratch = vec![0u8; WearStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut buf_b = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();

    let mut a = vec![0u8; WearStorage::BLOCK_SIZE];
    let mut b = vec![0u8; WearStorage::BLOCK_SIZE];

    // Root ids: superblock = 0, /pad = 1, /d = 2, /victim = 3.
    fs.write_to_path(p("/pad"), b"pad", &mut a, &mut b).unwrap();
    fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
    fs.write_to_path(p("/victim"), b"victim-content", &mut a, &mut b).unwrap();
    // Delete /pad: live ids become /d = 1, /victim = 2, while /d's
    // DirStruct tag in the root log still carries raw id 2.
    fs.remove_at_path(p("/pad"), &mut a, &mut b).unwrap();

    // Hammer /d so its pair compacts repeatedly; with BLOCK_CYCLES = 1
    // a compaction relocates the pair every third revision, forcing
    // propagate_relocation to update the root's entry for /d.
    for i in 0..30u8 {
        let content = [i; 24];
        fs.write_to_path(p("/d/f"), &content, &mut a, &mut b).unwrap();
    }

    // The sibling file must be untouched.
    let r = fs.resolve(p("/victim"), &mut a, &mut b).unwrap();
    assert_eq!(
        r.struct_body, b"victim-content",
        "C5: relocation repointed the sibling's struct body"
    );
    // And /d must still resolve through its (updated) pair address.
    let r = fs.resolve(p("/d/f"), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_body, &[29u8; 24], "C5: child pair address went stale");
}

// ---------------------------------------------------------------------
// H1: the reader must accept C-compacted tag orders.
// ---------------------------------------------------------------------

/// Hand-build a root block whose surviving-tag order mimics what C
/// compaction emits after a rename: an entry's STRUCT precedes the
/// count-establishing NAME tags, and NAME ids are not in ascending
/// order. The C reader accepts this (count = max(id + 1, count));
/// v1.2.0 rejected it with `Error::Corrupt` at mount.
///
/// Construction sketch (all in one commit, as compaction emits):
///   Superblock NAME id 0 + geometry InlineStruct id 0
///   InlineStruct id 1   ("a"'s struct survived from before a rename)
///   RegularFile NAME id 2 "b" + InlineStruct id 2
///   RegularFile NAME id 1 "a" (the rename's NAME, last in log order)
#[test]
fn h1_reader_accepts_non_id_dense_name_order() {
    // Pull a valid superblock NAME + geometry body out of a freshly
    // formatted image, so this test does not hardcode the disk
    // version or geometry encoding.
    let mut donor = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut donor, &mut scratch).unwrap();
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    donor.read(0, 0, &mut block0).unwrap();
    let reader = littlefs2_pure::meta::MetadataReader::new(&block0).unwrap();
    let mut sb_name: Option<alloc::vec::Vec<u8>> = None;
    let mut sb_geom: Option<alloc::vec::Vec<u8>> = None;
    for entry in reader.iter_tags() {
        match entry.tag.tag_type() {
            TagType::Superblock => sb_name = Some(entry.body.to_vec()),
            TagType::InlineStruct if entry.tag.id() == 0 && sb_geom.is_none() => {
                sb_geom = Some(entry.body.to_vec());
            }
            _ => {}
        }
    }
    let sb_name = sb_name.expect("formatted image carries a superblock NAME");
    let sb_geom = sb_geom.expect("formatted image carries the geometry struct");

    // Craft the C-compaction-shaped root block.
    let mut builder = BlockBuilder::new(MemStorage::BLOCK_SIZE, 1).unwrap();
    builder.tag(Tag::new(true, TagType::Superblock, 0, sb_name.len() as u16), &sb_name).unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 0, sb_geom.len() as u16), &sb_geom).unwrap();
    // "a"'s struct, surviving in log order from before its rename.
    builder.tag(Tag::new(true, TagType::InlineStruct, 1, 4), b"AAAA").unwrap();
    // "b": NAME at id 2 arrives while only ids 0..=0 are name-known.
    builder.tag(Tag::new(true, TagType::RegularFile, 2, 1), b"b").unwrap();
    builder.tag(Tag::new(true, TagType::InlineStruct, 2, 4), b"BBBB").unwrap();
    // "a"'s NAME, last in log order (the rename wrote it latest).
    builder.tag(Tag::new(true, TagType::RegularFile, 1, 1), b"a").unwrap();
    builder.commit(0).unwrap();
    let crafted = builder.finish();

    let mut storage = MemStorage::new();
    // Block 0 = crafted; block 1 stays erased (revision reads as
    // 0xFFFFFFFF with no commits, so block 0 is the active one).
    for (i, chunk) in crafted.chunks(MemStorage::PROG_SIZE).enumerate() {
        storage.program(0, (i * MemStorage::PROG_SIZE) as u32, chunk).unwrap();
    }

    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("H1: mount must accept a C-compacted tag order");

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.resolve(p("/a"), &mut a, &mut b).expect("entry a resolves");
    assert_eq!(r.struct_body, b"AAAA");
    let r = fs.resolve(p("/b"), &mut a, &mut b).expect("entry b resolves");
    assert_eq!(r.struct_body, b"BBBB");
}
