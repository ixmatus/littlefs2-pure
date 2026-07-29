//! Review finding L2 (bead `lfs-bdv`): commit CRC recognition must match
//! the C reader's acceptance rule, not the narrower set of chunk values
//! the C writer happens to emit today.
//!
//! # The rule, derived from the vendored C reference
//!
//! `lfs_dir_fetchmatch` ends a commit when
//! `lfs_tag_type2(tag) == LFS_TYPE_CCRC` (`lfs.c:1182`), and uses the same
//! predicate on the previous tag to decide whether the region that follows
//! is still erased (`lfs.c:1173`). `lfs_tag_type2` keeps only the top four
//! bits of the eleven bit type field, `(tag & 0x78000000) >> 20`
//! (`lfs.c:347`), and `LFS_TYPE_CCRC` is `0x500` (`lfs.h:114`). A tag
//! therefore terminates a commit exactly when its type field satisfies
//! `type & 0x780 == 0x500`, which is abstract type `0x5` with any chunk
//! from `0x00` through `0x7f`. Only chunk bit 0 reaches the reader as
//! meaning: it is the perturb bit XORed into bit 31 of the running tag
//! base (`lfs.c:1201`). Chunk `0xff` is the forward CRC, matched on the
//! full eleven bit type field (`lfs.c:1264`, `LFS_TYPE_FCRC = 0x5ff` at
//! `lfs.h:115`), and chunks `0x80` through `0xfe` are ordinary data tags.
//!
//! The C writer emits only `0x500` and `0x501` (`lfs.c:1715`). The gap
//! between what it writes and what it reads is deliberate forward
//! compatibility room, so a reader that recognizes only the writer's two
//! values stops the commit chain on an image the C reference accepts.
//!
//! # What these tests pin
//!
//! - the classifier boundary at chunk `0x80`, in both directions;
//! - a metadata block whose commit ends in a chunk `0x04` commit CRC still
//!   parses (the reproducer for the finding);
//! - the perturb bit still comes from chunk bit 0 alone, for a chunk value
//!   outside the writer's range;
//! - a whole filesystem image whose final commit CRC carries chunk bit 2
//!   mounts and reads under this crate, and, when the C verifier binary is
//!   available, under the C reference as well. That last assertion is the
//!   oracle: it shows the widened range is what C actually accepts rather
//!   than only what this crate's own reader was taught.

use littlefs2_pure::crc;
use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::tag::{Tag, TagType, ID_NONE};
use littlefs2_pure::{Fs, Path};

mod common;
use common::{BlockBuilder, MemStorage};

/// The C reader's commit CRC predicate, restated over the raw eleven bit
/// type field. Independent of this crate's classifier on purpose: it is
/// the oracle the classifier is checked against.
fn c_reader_says_ccrc(type_field: u16) -> bool {
    type_field & 0x780 == 0x500
}

/// The classifier agrees with the C reader over the whole CRC family.
#[test]
fn classifier_matches_c_acceptance_rule_over_the_whole_crc_family() {
    for chunk in 0u16..=0xff {
        let type_field = 0x500 | chunk;
        let ours = matches!(TagType::from_bits(type_field), TagType::CommitCrc(_));
        assert_eq!(
            ours,
            c_reader_says_ccrc(type_field),
            "chunk {chunk:#04x}: this crate says ccrc={ours}, the C reader says {}",
            c_reader_says_ccrc(type_field)
        );
    }
}

/// The chunk byte survives classification unchanged across the widened
/// range, and the boundary at bit 7 is where the family ends.
#[test]
fn commit_crc_chunk_range_and_boundary() {
    for chunk in 0u8..=0x7f {
        assert_eq!(
            TagType::from_bits(0x500 | u16::from(chunk)),
            TagType::CommitCrc(chunk),
            "chunk {chunk:#04x} should classify as a commit CRC carrying its chunk"
        );
    }
    // Bit 7 set is not a commit CRC. 0xff is the forward CRC; the rest are
    // ordinary data tags this reader must walk past, exactly as C does.
    assert_eq!(TagType::from_bits(0x5ff), TagType::ForwardCrc);
    for chunk in 0x80u16..=0xfe {
        let ty = TagType::from_bits(0x500 | chunk);
        assert!(
            !matches!(ty, TagType::CommitCrc(_)),
            "chunk {chunk:#04x} has bit 7 set and must not be a commit CRC, got {ty:?}"
        );
    }
}

/// Reproducer for the finding. A metadata block whose single commit is
/// terminated by a commit CRC with chunk `0x04` is valid to the C reader:
/// the CRC is correct and the chunk is inside the accepted range. Before
/// the fix the tag classified as `Unknown`, the commit never closed, and
/// the block reported zero committed bytes.
#[test]
fn commit_terminated_by_chunk_0x04_is_read() {
    for chunk in [0x04u8, 0x05, 0x40, 0x7f] {
        let mut b = BlockBuilder::new(256, 7).unwrap();
        b.tag(Tag::new(true, TagType::Superblock, 0, 8), b"littlefs").unwrap();
        b.commit(chunk).unwrap();
        let used = b.offset();
        let block = b.finish();

        let reader = MetadataReader::new(&block).unwrap();
        assert_eq!(
            reader.committed_end(),
            used,
            "chunk {chunk:#04x}: the commit must close at the end of the CCRC"
        );
        assert_eq!(reader.revision(), 7);
        let names: Vec<_> = reader
            .iter_tags()
            .filter(|e| e.tag.tag_type() == TagType::Superblock)
            .map(|e| e.body.to_vec())
            .collect();
        assert_eq!(names, vec![b"littlefs".to_vec()], "chunk {chunk:#04x}");
    }
}

/// Only chunk bit 0 feeds the perturb. Two commits back to back, the first
/// terminated with chunk `0x05` (odd, so the running tag base flips) and
/// the second with chunk `0x06` (even), must both verify and the second
/// commit's tags must decode. A reader that took the whole chunk byte into
/// the perturb, or that dropped the perturb for chunks outside the writer's
/// range, would fail to decode the second commit's first tag.
#[test]
fn only_chunk_bit_zero_perturbs_the_tag_base() {
    let mut b = BlockBuilder::new(256, 3).unwrap();
    b.tag(Tag::new(true, TagType::Superblock, 0, 8), b"littlefs").unwrap();
    b.commit(0x05).unwrap();
    b.tag(Tag::new(true, TagType::RegularFile, 1, 3), b"abc").unwrap();
    b.commit(0x06).unwrap();
    let used = b.offset();
    let block = b.finish();

    let reader = MetadataReader::new(&block).unwrap();
    assert_eq!(reader.committed_end(), used, "both commits must verify");
    let names: Vec<_> = reader
        .iter_tags()
        .filter(|e| e.tag.tag_type() == TagType::RegularFile)
        .map(|e| e.body.to_vec())
        .collect();
    assert_eq!(names, vec![b"abc".to_vec()]);
}

/// A commit CRC tag whose body is too short is still rejected. Widening the
/// accepted chunk range must not relax any check that guards a genuinely
/// malformed tag.
#[test]
fn short_bodied_commit_crc_is_still_rejected() {
    let mut block = vec![0xFFu8; 256];
    block[0..4].copy_from_slice(&1u32.to_le_bytes());
    // A commit CRC declaring a 2 byte body: not enough room for the four
    // byte little endian CRC the format requires.
    let tag = Tag::new(true, TagType::CommitCrc(0x04), ID_NONE, 2);
    let raw = tag.into_bits() ^ 0xFFFF_FFFF;
    block[4..8].copy_from_slice(&raw.to_be_bytes());
    let reader = MetadataReader::new(&block).unwrap();
    assert_eq!(reader.committed_end(), 0, "a short bodied commit CRC must not close a commit");
}

/// A commit CRC tag inside the widened range whose stored CRC is wrong is
/// still rejected, so the widening buys forward compatibility without
/// buying tolerance for corruption.
#[test]
fn wrong_crc_in_widened_range_is_still_rejected() {
    let mut b = BlockBuilder::new(256, 1).unwrap();
    b.tag(Tag::new(true, TagType::Superblock, 0, 8), b"littlefs").unwrap();
    b.commit(0x04).unwrap();
    let used = b.offset();
    let mut block = b.finish();
    // Corrupt the stored CRC body of the commit CRC (the last four bytes
    // of the commit).
    block[used - 1] ^= 0x01;
    let reader = MetadataReader::new(&block).unwrap();
    assert_eq!(reader.committed_end(), 0, "a mismatched CRC must not close a commit");
}

// ---------------------------------------------------------------------
// Whole image, with the C reference as the oracle.
// ---------------------------------------------------------------------

/// Rewrite the last verified commit CRC of `block` so its chunk gains bit
/// 2, keeping bit 0 (the perturb) and the tag's id and length fields
/// unchanged, then repair the commit's stored CRC. Returns `true` if a
/// commit CRC was found and rewritten.
///
/// The walk mirrors the on disk format directly and classifies CRC tags
/// through [`c_reader_says_ccrc`] rather than through this crate's
/// classifier, so it produces the same bytes whether or not the fix under
/// test is present.
fn widen_last_ccrc_chunk(block: &mut [u8]) -> bool {
    let mut ptag: u32 = 0xFFFF_FFFF;
    let mut running = crc::update(crc::INIT, &block[0..4]);
    let mut off: usize = 0;
    // (offset of the tag word, tag base in force there, CRC before the tag
    // word was folded in, the decoded tag).
    let mut last: Option<(usize, u32, u32, u32)> = None;

    loop {
        off += Tag::from_bits(ptag).dsize();
        if off + 4 > block.len() {
            break;
        }
        let raw = u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        let decoded = raw ^ ptag;
        let tag = Tag::from_bits(decoded);
        if !tag.is_valid() || off + tag.dsize() > block.len() {
            break;
        }
        let before = running;
        running = crc::update(running, &block[off..off + 4]);
        if c_reader_says_ccrc(tag.type_bits()) {
            if tag.body_len() < 4 {
                break;
            }
            let body = off + 4;
            let stored = u32::from_le_bytes([
                block[body],
                block[body + 1],
                block[body + 2],
                block[body + 3],
            ]);
            if stored != running {
                break;
            }
            last = Some((off, ptag, before, decoded));
            ptag = decoded ^ ((decoded >> 20) & 1) << 31;
            running = crc::INIT;
        } else {
            running = crc::update(running, &block[off + 4..off + tag.dsize()]);
            ptag = decoded;
        }
    }

    let Some((tag_off, base, before, decoded)) = last else { return false };
    // Chunk bit 2 lives at bit 22 of the tag word: the chunk occupies bits
    // 20 through 27. Setting it keeps bit 0, so the perturb is untouched.
    let widened = decoded | (0x04 << 20);
    assert!(c_reader_says_ccrc(Tag::from_bits(widened).type_bits()));
    let raw = widened ^ base;
    block[tag_off..tag_off + 4].copy_from_slice(&raw.to_be_bytes());
    let fixed = crc::update(before, &raw.to_be_bytes());
    block[tag_off + 4..tag_off + 8].copy_from_slice(&fixed.to_le_bytes());
    true
}

/// Build the `inline` round-trip image, widen every metadata block's final
/// commit CRC chunk into the range only the C reader accepts, and check
/// that this crate still mounts and reads it. This is the finding's
/// user visible symptom: before the fix the root pair's commit chain broke
/// and the mount failed.
///
/// When the C verifier binary is present the same bytes are handed to the
/// C reference, which must mount and read the file. That is the oracle for
/// the widened rule.
#[test]
fn image_with_widened_ccrc_chunk_mounts_here_and_under_c() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.write_to_path(Path::new("/cfg").unwrap(), b"hello, rust", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // The root pair is blocks 0 and 1; `/cfg` is inline, so those two
    // blocks hold every commit in the image.
    let bs = MemStorage::BLOCK_SIZE;
    let mut widened = 0;
    for block in 0..2usize {
        let slice = &mut storage.data[block * bs..(block + 1) * bs];
        if widen_last_ccrc_chunk(slice) {
            widened += 1;
        }
    }
    assert!(widened > 0, "the root pair must carry at least one commit to widen");

    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
            .expect("mount an image whose commit CRC chunk is inside the C reader's range");
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let mut out = [0u8; 32];
        let n = fs.read_at_path(Path::new("/cfg").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(&out[..n], b"hello, rust");
        storage = fs.into_storage();
    }

    // The C reference is the oracle for the acceptance rule. Skipped when
    // the verifier binary has not been built, exactly as `roundtrip.rs`
    // does, and hard failed when the environment demands the gate.
    let verifier = std::path::PathBuf::from("tools/verify_image/build/verify_image");
    if !verifier.exists() {
        assert!(
            std::env::var_os("LFS_REQUIRE_VERIFIER").is_none(),
            "LFS_REQUIRE_VERIFIER is set but the verifier binary is missing at {}; \
             build it with `make -C tools/verify_image`.",
            verifier.display()
        );
        eprintln!(
            "C oracle skipped: verifier binary not found at {}. Build with \
             `make -C tools/verify_image` to enable.",
            verifier.display()
        );
        return;
    }
    let img =
        std::env::temp_dir().join(format!("littlefs2-pure-l2-ccrc-{}.bin", std::process::id()));
    std::fs::write(&img, &storage.data).expect("write the widened image");
    let status = std::process::Command::new(&verifier)
        .arg(&img)
        .arg("inline")
        .status()
        .expect("invoke verify_image");
    assert!(
        status.success(),
        "the C reference rejected an image whose commit CRC chunk is {:#04x}: status = {status:?}",
        0x04
    );
    let _ = std::fs::remove_file(&img);
}
