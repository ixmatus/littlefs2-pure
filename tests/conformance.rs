//! Conformance against C-reference-generated vectors.
//!
//! Each `tests/vectors/NN_*.bin` file is a 2 KiB image produced by
//! `tools/gen_vectors/main.c` running the C littlefs reference at a
//! pinned version (see `tools/gen_vectors/littlefs/lfs.h`'s
//! `LFS_DISK_VERSION`). The scenarios cover:
//!
//! - `01_empty_format`: fresh `lfs_format` with no entries.
//! - `02_single_inline`: one tiny file at root (`/cfg`, 15 bytes).
//! - `03_single_ctz`: one CTZ-backed file at root (`/payload.bin`,
//!   500 bytes, content `i & 0xff` for `i = 0..500`).
//! - `04_nested_dir`: `/audit/` directory containing
//!   `/audit/log` with the body `entry-0001;`.
//! - `05_hardtail_dir`: `/d` filled with 26 zero length files
//!   (`a`..`z`), dense enough to span a HardTail chain.
//! - `06_inline_ctz_boundary`: `/b128` and `/b129`, straddling the
//!   inline/CTZ size region (both CTZ at this geometry).
//! - `07_deleted_recreated`: `/x` created, removed, then recreated
//!   with a different body; a delete tombstone precedes the live entry.
//! - `08_user_attrs`: `/aa`, `/bb`, `/cc` each with user attributes,
//!   `/bb` then removed so the survivors' live ids shift; pins
//!   splice-aware attribute reads (review C1/C2).
//! - `09_deep_ctz`: `/big.bin`, 900 bytes, a CTZ skip list spanning
//!   four data blocks; exercises back-pointer traversal past 03/06.
//! - `10_delete_tombstone`: `/aa` kept beside a bare `/bb` tombstone
//!   (no recreate); the C-writes/Rust-reads companion to the roundtrip
//!   `remove` scenario (review C3).
//! - `11_compacted_rename`: three entries, one renamed, then compacted;
//!   the compacted block carries NAME ids non-monotonic in log order,
//!   which a reader requiring id-dense NAME order rejects (review H1).
//! - `12_multimove_gstate`: two cross-directory renames into `/dst`
//!   leave two `MOVESTATE` tags in one log; an XOR-accumulating reader
//!   decodes a phantom pending move at mount (review C4).
//! - `13_null_tail`: `mkdir /a`, `mkdir /b`, `/b/keep`, then `rmdir /a`
//!   removes the last directory in the global thread, so the C
//!   reference's `lfs_dir_drop` hands `/b` an explicit all-ones SoftTail
//!   body; a reader that treats the sentinel as an address rejects the
//!   image as `Corrupt` (review L9, `lfs-yl6`).
//!
//! The runner loads each vector into a `MemStorage`, mounts with our
//! reader, and asserts the expected (name, kind, content) tuples.
//!
//! # Regenerating
//!
//! ```text
//! make -C tools/gen_vectors vectors
//! ```
//!
//! requires a host C toolchain (`cc`). The bin files are committed at
//! rest; CI does not invoke the C compiler.
//!
//! Bit-level cross check rather than self-consistency: if our writer
//! and reader both have a coordinated bug, the property tests miss
//! it. The C-reference images keep us honest at the byte boundary.

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{crc, Error, Fs, Path};

mod common;
use common::MemStorage;

/// Per-vector content pin: the LittleFS CRC32 ([`crc::compute`]) of the
/// full image bytes.
///
/// This is a regeneration tripwire, not an adversarial integrity check.
/// The vectors are produced by the C reference and committed at rest in a
/// trusted repository; the failure this guards against is a silent
/// `make -C tools/gen_vectors vectors` that changes the bytes while the
/// only existing check (file size) still passes, so the conformance
/// assertions quietly start exercising a different image. A CRC32 over
/// 2 KiB is ample to catch accidental drift; it is deliberately not a
/// cryptographic hash because the threat model is mistake, not malice.
///
/// When a vector legitimately changes, regenerate it and update the
/// expected value here in the same commit as the new `.bin`.
const VECTOR_CRCS: &[(&str, u32)] = &[
    ("01_empty_format.bin", 0xd225_bb0e),
    ("02_single_inline.bin", 0xeab2_c5e3),
    ("03_single_ctz.bin", 0x14c7_2253),
    ("04_nested_dir.bin", 0x0886_0052),
    ("05_hardtail_dir.bin", 0xed09_9bc9),
    ("06_inline_ctz_boundary.bin", 0xc999_5194),
    ("07_deleted_recreated.bin", 0x3f96_a60e),
    ("08_user_attrs.bin", 0xe13d_7a8f),
    ("09_deep_ctz.bin", 0xe5a6_a7e5),
    ("10_delete_tombstone.bin", 0xeb99_730e),
    ("11_compacted_rename.bin", 0x9e98_a6ee),
    ("12_multimove_gstate.bin", 0xf1b1_6107),
    ("13_null_tail.bin", 0x8a06_ba04),
];

/// Load a vector file's bytes into a fresh `MemStorage`. Panics on
/// I/O error (these files are committed; a missing one is a setup
/// bug, not a flake).
fn load(vector_name: &str) -> MemStorage {
    let path = format!("tests/vectors/{vector_name}");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert_eq!(
        bytes.len(),
        MemStorage::BLOCK_SIZE * MemStorage::BLOCK_COUNT as usize,
        "vector {vector_name} is the wrong size; regenerate with `make -C tools/gen_vectors vectors`"
    );
    let expected = VECTOR_CRCS
        .iter()
        .find(|(n, _)| *n == vector_name)
        .unwrap_or_else(|| panic!("no CRC pin for vector {vector_name}; add one to VECTOR_CRCS"))
        .1;
    let actual = crc::compute(&bytes);
    assert_eq!(
        actual, expected,
        "vector {vector_name} content changed (CRC {actual:#010x} != pinned {expected:#010x}); \
         if this was an intentional regeneration, update VECTOR_CRCS in the same commit"
    );
    let mut s = MemStorage::new();
    s.data.copy_from_slice(&bytes);
    s
}

fn mount(s: MemStorage) -> Fs<MemStorage> {
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    Fs::mount(s, &mut buf_a, &mut buf_b).expect("mount conformance vector")
}

fn read_inline<'a>(fs: &mut Fs<MemStorage>, p: &str, scratch: &'a mut [u8]) -> &'a [u8] {
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let r = fs.resolve(Path::new(p).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(r.struct_type, TagType::InlineStruct);
    let n = r.struct_body.len();
    scratch[..n].copy_from_slice(r.struct_body);
    &scratch[..n]
}

fn read_ctz_all(fs: &mut Fs<MemStorage>, p: &str) -> Vec<u8> {
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let size = fs.size_of(Path::new(p).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    let mut out = vec![0u8; size as usize];
    let n = fs.read_at_path(Path::new(p).unwrap(), 0, &mut out, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(n, size as usize);
    out
}

fn get_attr_vec(fs: &mut Fs<MemStorage>, p: &str, attr_id: u8) -> Vec<u8> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 64];
    let n = fs.get_attr(Path::new(p).unwrap(), attr_id, &mut out, &mut a, &mut b).unwrap();
    out[..n].to_vec()
}

fn root_names(fs: &mut Fs<MemStorage>) -> Vec<Vec<u8>> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_root(|e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    names
}

#[test]
fn vector_01_empty_format_mounts_clean() {
    let mut fs = mount(load("01_empty_format.bin"));
    // Superblock geometry matches MemStorage's constants.
    let sb = fs.superblock();
    assert_eq!(sb.block_size, MemStorage::BLOCK_SIZE as u32);
    assert_eq!(sb.block_count, MemStorage::BLOCK_COUNT);
    assert_eq!(sb.version, littlefs2_pure::DISK_VERSION);

    // Root is empty (no user entries).
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut count = 0usize;
    fs.list_root(|_| count += 1, &mut a, &mut b).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn vector_02_single_inline_resolves_and_reads() {
    let mut fs = mount(load("02_single_inline.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // /cfg exists, is a regular file, and resolves to InlineStruct
    // body == "hello, littlefs" (15 bytes).
    assert!(fs.exists(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap());
    let mut scratch = [0u8; 32];
    let body = read_inline(&mut fs, "/cfg", &mut scratch);
    assert_eq!(body, b"hello, littlefs");
    assert_eq!(body.len(), 15);

    // Root has exactly one user entry.
    let mut count = 0usize;
    fs.list_root(|_| count += 1, &mut a, &mut b).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn vector_03_single_ctz_resolves_and_reads() {
    let mut fs = mount(load("03_single_ctz.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // /payload.bin exists as a CTZ-backed regular file with 500 bytes.
    let r = fs.resolve(Path::new("/payload.bin").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    assert_eq!(ctz.size, 500);

    let bytes = read_ctz_all(&mut fs, "/payload.bin");
    let expected: Vec<u8> = (0..500).map(|i| (i & 0xff) as u8).collect();
    assert_eq!(bytes, expected);
}

#[test]
fn vector_04_nested_dir_resolves_through_dirstruct() {
    let mut fs = mount(load("04_nested_dir.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // /audit is a Directory; /audit/log is a regular file with the
    // expected inline body.
    assert!(fs.exists(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap());
    let mut scratch = [0u8; 32];
    let body = read_inline(&mut fs, "/audit/log", &mut scratch);
    assert_eq!(body, b"entry-0001;");

    // Listing /audit yields exactly one entry named "log".
    let mut entries: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/audit").unwrap(), |e| entries.push(e.name.to_vec()), &mut a, &mut b)
        .unwrap();
    assert_eq!(entries, vec![b"log".to_vec()]);
}

#[test]
fn vector_05_hardtail_dir_lists_every_entry() {
    // The C reference filled /d with 26 zero length files (a..z). At
    // this geometry that directory does not fit in a single metadata
    // pair, so the image links continuation pairs with a HardTail.
    // Whether and where the split lands is a C-reference internal; the
    // observable contract this pins is that our reader enumerates every
    // entry, which only holds if it walks the whole HardTail chain.
    let mut fs = mount(load("05_hardtail_dir.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let r = fs.resolve(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_type, TagType::DirStruct);

    let mut entries: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/d").unwrap(), |e| entries.push(e.name.to_vec()), &mut a, &mut b)
        .unwrap();
    let expected: Vec<Vec<u8>> = (b'a'..=b'z').map(|c| vec![c]).collect();
    assert_eq!(entries, expected, "every a..z entry must survive the HardTail walk");
}

#[test]
fn vector_06_inline_ctz_boundary_classified_as_c_wrote_it() {
    // /b128 (128 bytes) and /b129 (129 bytes) straddle the inline/CTZ
    // size region. At this geometry the C reference stored both as CTZ
    // skip lists; the value of the pin is that our reader classifies
    // each exactly as the writer did and reads the bytes back
    // identically, so a future inline-threshold drift on either side is
    // caught.
    let mut fs = mount(load("06_inline_ctz_boundary.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    for (name, len) in [("/b128", 128usize), ("/b129", 129usize)] {
        let r = fs.resolve(Path::new(name).unwrap(), &mut a, &mut b).unwrap();
        assert_eq!(r.struct_type, TagType::CtzStruct, "{name} struct type");
        let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
        assert_eq!(ctz.size as usize, len, "{name} size");
        let got = read_ctz_all(&mut fs, name);
        let want: Vec<u8> = (0..len).map(|i| (i & 0xff) as u8).collect();
        assert_eq!(got, want, "{name} body");
    }
}

#[test]
fn vector_07_deleted_recreated_resolves_to_fresh_body() {
    // /x was created (body "stale-v1"), removed, then recreated (body
    // "fresh-v2!!"). The metadata pair carries a delete tombstone
    // followed by a fresh create for the same name. A reader that
    // stops at the first name match or ignores the tombstone would
    // resolve the stale 8 byte body; the correct result is the fresh
    // 10 byte one and exactly one live /x.
    let mut fs = mount(load("07_deleted_recreated.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let mut scratch = [0u8; 32];
    let body = read_inline(&mut fs, "/x", &mut scratch);
    assert_eq!(body, b"fresh-v2!!");

    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_root(|e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"x".to_vec()], "only the recreated /x is live");
}

#[test]
fn vector_08_user_attrs_reads_splice_aware() {
    // /aa, /bb, /cc were each written with user attributes, then /bb
    // (the middle id) removed. The delete splices the survivors down one
    // live id, so /cc's attribute lives at a raw committed id one above
    // its current live id. A reader that compares the raw id without
    // splice-correcting loses /cc's attribute or reads /bb's across the
    // gap (review C1/C2). Attr ids are the ASCII bytes 't' and 'u'.
    let mut fs = mount(load("08_user_attrs.bin"));

    assert_eq!(get_attr_vec(&mut fs, "/aa", b't'), b"meta");
    assert_eq!(get_attr_vec(&mut fs, "/aa", b'u'), b"data99");
    assert_eq!(get_attr_vec(&mut fs, "/cc", b't'), b"cmeta");
    // An attribute id never set reads as absent (zero bytes).
    assert_eq!(get_attr_vec(&mut fs, "/aa", b'z'), b"");

    // /bb is gone; only /aa and /cc survive, in live-id order.
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    assert!(!fs.exists(Path::new("/bb").unwrap(), &mut a, &mut b).unwrap());
    assert_eq!(root_names(&mut fs), vec![b"aa".to_vec(), b"cc".to_vec()]);
}

#[test]
fn vector_09_deep_ctz_traverses_full_chain() {
    // 900 bytes lands as a four-block CTZ skip list at this geometry, so
    // reading it back walks the multi-level back pointers the smaller CTZ
    // vectors (03, 06) never reach.
    let mut fs = mount(load("09_deep_ctz.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let r = fs.resolve(Path::new("/big.bin").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);
    let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
    assert_eq!(ctz.size, 900);
    let blocks = littlefs2_pure::ctz::block_count(ctz.size, MemStorage::BLOCK_SIZE as u32);
    assert!(blocks >= 3, "deep CTZ vector must span 3+ blocks, got {blocks}");

    let bytes = read_ctz_all(&mut fs, "/big.bin");
    let expected: Vec<u8> = (0..900).map(|i| (i & 0xff) as u8).collect();
    assert_eq!(bytes, expected);
}

#[test]
fn vector_10_delete_tombstone_absent_neighbor_intact() {
    // /aa is kept; /bb was created then removed with no recreate, leaving
    // a bare delete tombstone beside a live neighbor. The C-writes /
    // Rust-reads companion to the roundtrip `remove` scenario (review C3):
    // a reader mishandling the size-0 delete resolves /bb to /aa. Distinct
    // from 07, which recreates the deleted name.
    let mut fs = mount(load("10_delete_tombstone.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let mut scratch = [0u8; 32];
    assert_eq!(read_inline(&mut fs, "/aa", &mut scratch), b"keep-me");
    assert!(!fs.exists(Path::new("/bb").unwrap(), &mut a, &mut b).unwrap());
    assert_eq!(root_names(&mut fs), vec![b"aa".to_vec()], "only /aa is live");
}

#[test]
fn vector_11_compacted_rename_accepts_non_id_dense_order() {
    // /aaa, /bbb, /ccc were created, /aaa renamed to /zzz, then the pair
    // forced through a C compaction. lfs_dir_compact re-emits survivors in
    // log order, so the renamed entry's NAME lands after higher-id NAMEs:
    // the compacted block carries NAME ids non-monotonic in log order
    // (1, 2, 4, 3). Before the H1 fix the reader required id-dense NAME
    // order and rejected this valid C image with Error::Corrupt; the mount
    // succeeding is the regression guard. The churned /t entry was the
    // compaction trigger and is gone.
    let mut fs = mount(load("11_compacted_rename.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    assert_eq!(root_names(&mut fs), vec![b"bbb".to_vec(), b"ccc".to_vec(), b"zzz".to_vec()]);
    for (name, body) in [("/zzz", b"AAAA"), ("/bbb", b"BBBB"), ("/ccc", b"CCCC")] {
        let r = fs.resolve(Path::new(name).unwrap(), &mut a, &mut b).unwrap();
        assert_eq!(r.struct_body, body, "{name} body");
    }
    assert!(!fs.exists(Path::new("/aaa").unwrap(), &mut a, &mut b).unwrap());
    assert!(!fs.exists(Path::new("/t").unwrap(), &mut a, &mut b).unwrap());
}

#[test]
fn vector_12_multimove_gstate_reads_latest_tag_wins() {
    // Two cross-directory renames into /dst with no intervening compaction
    // leave two MOVESTATE tags in /dst's log. C reads a pair's gstate
    // contribution as the single latest matching tag; an XOR-accumulating
    // reader decodes a phantom pending move and deletes a live entry at
    // mount (review C4). Mounting cleanly with both files intact is the
    // guard, and a second mount over the recovered image proves the
    // recovery is idempotent (no futile balancing commit corrupts state).
    let check = |fs: &mut Fs<MemStorage>| {
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        assert_eq!(root_names(fs), vec![b"dst".to_vec(), b"src".to_vec()]);
        let mut scratch = [0u8; 8];
        assert_eq!(read_inline(fs, "/dst/a", &mut scratch), b"AA");
        assert_eq!(read_inline(fs, "/dst/b", &mut scratch), b"BB");
        let mut count = 0usize;
        fs.list_dir(Path::new("/src").unwrap(), |_| count += 1, &mut a, &mut b).unwrap();
        assert_eq!(count, 0, "/src is empty after the moves");
        assert!(!fs.exists(Path::new("/src/a").unwrap(), &mut a, &mut b).unwrap());
    };

    let mut fs = mount(load("12_multimove_gstate.bin"));
    check(&mut fs);
    // Remount the recovered image: mount-time gstate recovery must be a
    // no-op here, not a deletion of a live entry.
    let storage = fs.into_storage();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs2 = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("remount recovered image");
    check(&mut fs2);
}

#[test]
fn vector_13_null_tail_mounts_and_reads() {
    // `lfs_dir_drop` (lfs.c:1831) commits the dropped directory's tail
    // body with no `lfs_pair_isnull` guard, so removing the LAST directory
    // in the global thread hands its predecessor a literal all-ones
    // SoftTail body. This image is that history: mkdir /a, mkdir /b (which
    // threads root -> /b -> /a, mkdir inserting at the head), a file in
    // /b, then rmdir /a. The surviving /b pair carries the sentinel.
    //
    // The C reader accepts it because every thread walk is gated on
    // `lfs_pair_isnull(dir->tail)` before the pair is fetched. Reporting
    // the sentinel to our walkers as a real address instead made mount
    // return Corrupt on a conforming image (`lfs-yl6`).
    let mut fs = mount(load("13_null_tail.bin"));
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    assert_eq!(root_names(&mut fs), vec![b"b".to_vec()], "/a was removed, /b survives");
    assert!(!fs.exists(Path::new("/a").unwrap(), &mut a, &mut b).unwrap());

    // The survivor is a directory, and its contents read back through the
    // pair that carries the sentinel tail.
    let mut names: Vec<Vec<u8>> = Vec::new();
    fs.list_dir(Path::new("/b").unwrap(), |e| names.push(e.name.to_vec()), &mut a, &mut b).unwrap();
    assert_eq!(names, vec![b"keep".to_vec()]);
    let mut scratch = [0u8; 8];
    assert_eq!(read_inline(&mut fs, "/b/keep", &mut scratch), b"KEEP");
}

#[test]
fn vector_13_null_tail_carries_the_explicit_sentinel() {
    // Structural pin for the vector above: assert the image really does
    // contain a committed tail tag whose body is all ones, so a future
    // regeneration that happens to take the compacting path (which omits
    // the tag, lfs.c:2003) cannot silently turn the mount test into a
    // check of the ordinary tag-absent encoding.
    //
    // Read at the raw tag level rather than through `MetadataReader::tail`,
    // which deliberately reports the sentinel as `None`.
    let bytes = std::fs::read("tests/vectors/13_null_tail.bin").expect("read vector");
    let bs = MemStorage::BLOCK_SIZE;
    let mut found = false;
    for blk in 0..MemStorage::BLOCK_COUNT as usize {
        let block = &bytes[blk * bs..(blk + 1) * bs];
        let Ok(reader) = littlefs2_pure::meta::MetadataReader::new(block) else { continue };
        if !reader.has_commits() {
            continue;
        }
        for tag in reader.iter_tags() {
            if !matches!(tag.tag.tag_type(), TagType::SoftTail | TagType::HardTail) {
                continue;
            }
            if tag.body.len() == 8 && tag.body == [0xFF; 8] {
                found = true;
            }
        }
    }
    assert!(
        found,
        "13_null_tail.bin no longer carries an explicit all-ones tail body; \
         regenerate the scenario so it exercises `lfs_dir_drop`'s unguarded \
         tail commit, or the mount test below proves nothing new"
    );
}

#[test]
fn unformatted_buffer_returns_unformatted() {
    // Sanity gate: a vector path that goes through the same code as
    // the conformance loader. Confirms the conformance harness's
    // mount path correctly distinguishes Unformatted (all-0xFF) from
    // a real vector.
    let s = MemStorage::new();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let err = Fs::mount(s, &mut buf_a, &mut buf_b).unwrap_err();
    assert_eq!(err, Error::Unformatted);
}
