//! R2 regression: the reader must validate the forward CRC (FCRC).
//!
//! Source: `docs/reviews/2026-05-15-six-agent-correctness-review.md`
//! High finding #2.
//!
//! `Commit::finish_padded` emits a spec-shaped FCRC tag whose body is
//! `(prog_size, crc_of_prog_size_0xFF_bytes)`: the CRC the next
//! prog-aligned window should have while it is still in the erased
//! state. The C reference, after a commit's CCRC verifies, recomputes
//! that window's CRC from disk and rejects the commit if it does not
//! match the FCRC, closing the intra-program torn-write hole. This
//! crate's reader previously checked only the CCRC and never read the
//! FCRC, so a commit whose CCRC is valid but whose following prog
//! window was contaminated by a torn write (a power loss *inside* a
//! program, not at a program boundary) was wrongly accepted: the
//! reader returned a state the writer never atomically committed.
//!
//! `tests/power_loss.rs` only tears at program-call boundaries (its
//! own header documents that intra-program torns are "what the FCRC
//! tag the writer emits is [meant to guard] at the reader side"),
//! so this class was untested. These tests pin it.

use littlefs2_pure::meta::{Commit, MetadataReader};
use littlefs2_pure::tag::{Tag, TagType};

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
fn clean_block_with_fcrc_still_mounts() {
    // Guard against over-strict rejection: a pristine erased window
    // after the last commit matches the FCRC, so the commit stays
    // accepted. (Also the path every real mount and every C-written
    // conformance vector takes.)
    for n in 1..=3 {
        let end = clean_committed_end(n);
        assert!(end > 0, "{n}-commit clean block must mount");
    }
}

#[test]
fn single_commit_with_torn_forward_window_is_rejected() {
    // One commit, CCRC valid, but the next prog window holds a torn
    // partial write (not 0xFF, not a valid following commit). The
    // FCRC in the commit says that window should be erased; it is
    // not, so the commit must be rejected. With only one commit,
    // rejection means the block has no durable state (the metadata
    // pair's other block carries the prior revision; that fallback
    // is the pair layer's job).
    let end1 = clean_committed_end(1);
    let mut blk = build_block(1);

    // committed_end is prog-aligned, so the next prog window starts
    // exactly there. Tear it: a few non-erased bytes that do not
    // form a valid commit.
    assert!(end1 + 4 <= BLOCK);
    blk[end1] = 0xAB;
    blk[end1 + 1] = 0xCD;
    blk[end1 + 2] = 0x00;
    blk[end1 + 3] = 0x12;

    let r = MetadataReader::new(&blk).unwrap();
    assert!(
        !r.has_commits(),
        "a commit whose forward window is torn must be rejected (got committed_end = {})",
        r.committed_end()
    );
}

#[test]
fn last_commit_torn_forward_window_rolls_back_one_commit() {
    // Two commits. The window after commit 2 is torn. Commit 2 must
    // be rejected, but commit 1 survives: it is followed by a
    // CCRC-valid commit (commit 2), so its own forward window held a
    // real commit, not a torn write. The reader must roll back
    // exactly one level, to commit 1's boundary.
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
    assert!(r.has_commits(), "commit 1 must survive a torn window after commit 2");
    assert_eq!(
        r.committed_end(),
        end1,
        "reader must roll back to commit 1's boundary, not keep the torn commit 2",
    );
}
