//! R2 regression: the reader uses the forward CRC (FCRC) to decide
//! whether the block is still *erased*, never to discard a durable
//! commit.
//!
//! Source: `docs/reviews/2026-05-15-six-agent-correctness-review.md`
//! High finding #2, corrected by the 2026-05-29 review (`lfs-3q9`).
//!
//! `Commit::finish_padded` emits a spec-shaped FCRC tag whose body is
//! `(prog_size, crc_of_prog_size_0xFF_bytes)`: the CRC the next
//! prog-aligned window should have while it is still in the erased
//! state. The C reference (`lfs_dir_fetchmatch`) fixes `dir->off` once
//! a commit's CCRC verifies and never moves it again; the FCRC's only
//! effect is `dir->erased = (recomputed_window_crc == fcrc.crc)`, a
//! hint that governs whether the *next* write may append in place or
//! must compact onto a freshly erased block.
//!
//! An earlier remediation of this finding mistakenly *rolled back* the
//! last verified commit on an FCRC mismatch, which discarded durably
//! committed data on the exact intra-program torn-write case the
//! filesystem is meant to survive (a power loss inside the program that
//! follows a CCRC-valid commit). These tests pin the corrected
//! behavior: the commit is kept, and only `erased()` flips to `false`.
//!
//! `tests/power_loss.rs` tears at program-call boundaries; this file
//! covers the intra-program torn forward window, which that harness
//! cannot generate.

use littlefs2_pure::meta::{Commit, MetadataReader};
use littlefs2_pure::tag::{Tag, TagType};
use littlefs2_pure::{Fs, Path};

mod common;
use common::{make_buffer, MemStorage};

const PROG: usize = 16;
const BLOCK: usize = 256;

/// Build a block with `n` commits, each a single small Name tag
/// finalized with `finish_padded` (so each emits an FCRC).
fn build_block(n: usize) -> [u8; BLOCK] {
    let mut blk = [0xFFu8; BLOCK];
    {
        let mut c = Commit::new(&mut blk, 1).unwrap();
        for i in 0..n {
            let body = [b'a' + i as u8; 4];
            c.tag(Tag::new(true, TagType::RegularFile, i as u16, 4), &body).unwrap();
            c.finish_padded(0, PROG, BLOCK).unwrap();
        }
    }
    blk
}

/// committed_end of a clean block with exactly `n` commits.
fn clean_committed_end(n: usize) -> usize {
    let blk = build_block(n);
    let r = MetadataReader::new(&blk).unwrap();
    assert!(r.has_commits(), "clean {n}-commit block must have commits");
    r.committed_end()
}

#[test]
fn clean_block_with_fcrc_is_erased_and_mounts() {
    // A pristine erased window after the last commit matches the FCRC,
    // so the block reports erased(): a writer may append in place. This
    // is the path every real mount and every C-written conformance
    // vector takes.
    for n in 1..=3 {
        let blk = build_block(n);
        let r = MetadataReader::new(&blk).unwrap();
        assert!(r.committed_end() > 0, "{n}-commit clean block must mount");
        assert!(r.erased(), "{n}-commit clean block's forward window is still erased");
    }
}

#[test]
fn single_commit_with_torn_forward_window_is_kept_but_not_erased() {
    // One commit, CCRC valid, but the next prog window holds a torn
    // partial write (not 0xFF, not a valid following commit). The
    // commit's own CCRC verified, so it is durable and must be kept;
    // only erased() flips to false so the next writer compacts rather
    // than appending into the dirty window. The C reference does the
    // same: dir->off keeps the commit, dir->erased goes false.
    let end1 = clean_committed_end(1);
    let mut blk = build_block(1);

    // committed_end is prog-aligned, so the next prog window starts
    // exactly there. Tear it: a few non-erased bytes that do not form a
    // valid commit.
    assert!(end1 + 4 <= BLOCK);
    blk[end1] = 0xAB;
    blk[end1 + 1] = 0xCD;
    blk[end1 + 2] = 0x00;
    blk[end1 + 3] = 0x12;

    let r = MetadataReader::new(&blk).unwrap();
    assert!(r.has_commits(), "a CCRC-valid commit is durable and must not be discarded");
    assert_eq!(
        r.committed_end(),
        end1,
        "the committed boundary stays at the verified commit (no rollback)",
    );
    assert!(!r.erased(), "a torn forward window must clear the erased flag");
}

#[test]
fn last_commit_torn_forward_window_keeps_both_commits() {
    // Two commits. The window after commit 2 is torn. Both commits are
    // CCRC-valid and durable, so both are kept; only erased() flips to
    // false. (Contrast the earlier, incorrect behavior, which rolled
    // back to commit 1's boundary and lost commit 2.)
    let end1 = clean_committed_end(1);
    let end2 = clean_committed_end(2);
    assert!(end2 > end1, "two commits must extend past one");

    let mut blk = build_block(2);
    assert!(end2 + 4 <= BLOCK);
    blk[end2] = 0xAB;
    blk[end2 + 1] = 0xCD;
    blk[end2 + 2] = 0x00;
    blk[end2 + 3] = 0x12;

    let r = MetadataReader::new(&blk).unwrap();
    assert!(r.has_commits(), "commits survive a torn window after the last commit");
    assert_eq!(
        r.committed_end(),
        end2,
        "both durable commits are kept; the torn window does not discard commit 2",
    );
    assert!(!r.erased(), "a torn forward window must clear the erased flag");
}

/// Locate the active block of the root pair (blocks 0/1) in a raw
/// `MemStorage` image and return `(block_index, committed_end)`.
fn active_root_block(storage: &MemStorage) -> (usize, usize) {
    let bs = MemStorage::BLOCK_SIZE;
    let blk0 = &storage.data[0..bs];
    let blk1 = &storage.data[bs..2 * bs];
    let r0 = MetadataReader::new(blk0).unwrap();
    let r1 = MetadataReader::new(blk1).unwrap();
    // Active block = higher revision among those with commits, ties to 0.
    let pick0 = match (r0.has_commits(), r1.has_commits()) {
        (true, false) => true,
        (false, true) => false,
        _ => (r0.revision().wrapping_sub(r1.revision()) as i32) >= 0,
    };
    if pick0 {
        (0, r0.committed_end())
    } else {
        (1, r1.committed_end())
    }
}

#[test]
fn intra_program_torn_window_keeps_latest_committed_data() {
    // End-to-end: write two versions of the same file so the active
    // root block holds a second durable commit followed by an erased
    // window, then simulate a power loss *inside* the program after
    // that commit by tearing the forward window. A remount must still
    // read the latest version. The earlier (buggy) rollback behavior
    // discarded the last commit and returned the stale first version.
    let mut storage = MemStorage::new();
    let mut scratch = make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();

    {
        let mut buf_a = make_buffer();
        let mut buf_b = make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = make_buffer();
        let mut b = make_buffer();
        fs.write_inline_to_root(b"cfg", b"v1", &mut a, &mut b).unwrap();
        fs.write_inline_to_root(b"cfg", b"v2", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Tear the forward window of whichever block is active.
    let (blk, end) = active_root_block(&storage);
    let base = blk * MemStorage::BLOCK_SIZE;
    assert!(end > 0 && base + end + 4 <= storage.data.len());
    storage.data[base + end] = 0xAB;
    storage.data[base + end + 1] = 0xCD;
    storage.data[base + end + 2] = 0x00;
    storage.data[base + end + 3] = 0x12;

    // Remount and confirm the latest version is intact, not the stale one.
    let mut buf_a = make_buffer();
    let mut buf_b = make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a = make_buffer();
    let mut b = make_buffer();
    let r = fs.resolve(Path::new("/cfg").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(
        r.struct_body, b"v2",
        "the latest durably committed version must survive a torn forward window",
    );
}
