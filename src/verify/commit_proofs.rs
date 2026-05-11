//! Commit accept-or-reject dispatch totality.
//!
//! `MetadataReader::new` walks a block, verifying each commit's
//! CCRC and stopping at the first failure. The reader must never
//! panic regardless of the block's bytes: an attacker who can write
//! arbitrary content to a metadata pair (or a corrupted chip) must
//! get a clean parse-or-reject answer, not a process crash.
//!
//! ## Stub for `crc::update`
//!
//! Two of the three harnesses below stub out [`crate::crc::update`]
//! with a nondeterministic `u32` return. The property under test
//! is panic-freedom on adversarial bytes, not CRC correctness;
//! CRC correctness is discharged separately in
//! [`crate::verify::crc_proofs`]. Without the stub, CBMC tries to
//! unwind the CRC byte-loop combinatorially across every reachable
//! reader path, exhausting the solver budget. With the stub, the
//! reader's bounds-check and dispatch logic is the only thing
//! CBMC reasons about — which is the right scope for "no panic."
//!
//! Kani's symbolic-input loop budget still caps exhaustive
//! coverage at short blocks. The fuzz harness in `fuzz/` extends
//! coverage to longer adversarial inputs.

use crate::meta::MetadataReader;

/// Nondeterministic [`crate::crc::update`] replacement used by the
/// stubbed harnesses below. CBMC treats the return value as
/// arbitrary, which is sound for proving panic-freedom (the reader
/// must reject *every* path).
#[cfg(kani)]
fn crc_update_stub(_seed: u32, _data: &[u8]) -> u32 {
    kani::any()
}

/// `MetadataReader::new` on a 32-byte block of arbitrary content
/// must not panic; it either returns `Ok(_)` with whatever commit
/// boundary it can verify or `Err(Error::Corrupt)` for a block too
/// short to even hold the revision header. No other failure mode.
///
/// 32 bytes is enough to hold the revision header + one tag word +
/// CCRC; with `crc::update` stubbed (see module docs) Kani
/// exhausts the byte space within its default budget. Longer
/// blocks are fuzzed.
#[kani::proof]
#[kani::stub(crate::crc::update, crc_update_stub)]
#[kani::unwind(33)]
fn metadata_reader_does_not_panic_on_arbitrary_input() {
    let mut block: [u8; 32] = [0; 32];
    for byte in block.iter_mut() {
        *byte = kani::any();
    }
    // The result is allowed to be `Ok` (some prefix verifies) or
    // `Err` (block too short, which it isn't here). What matters
    // is that the call returns. The unwind bound is `block.len() /
    // min_tag_dsize + 1 = 32/4 + 1 = 9`; tighter than the loop's
    // worst case so CBMC terminates without combinatorial blow-up.
    let _ = MetadataReader::new(&block);
}

/// Same property, for an even smaller block. The reader returns
/// `Err(Error::Corrupt)` if the block is shorter than the 4-byte
/// revision header; this proof pins that lower bound. No CRC stub
/// needed: the block is too short for the CRC loop to fire.
#[kani::proof]
fn metadata_reader_rejects_short_blocks() {
    let mut block: [u8; 3] = [0; 3];
    for byte in block.iter_mut() {
        *byte = kani::any();
    }
    let r = MetadataReader::new(&block);
    assert!(r.is_err(), "blocks shorter than 4 bytes must error");
}

/// Tag bounds-check totality: the reader never reads past the end
/// of the block. If a tag's length field claims more bytes than the
/// block has, the walk stops cleanly. Pinned here on a tiny block
/// where the property is exhaustively checkable with the CRC stub
/// (see module docs).
#[kani::proof]
#[kani::stub(crate::crc::update, crc_update_stub)]
#[kani::unwind(17)]
fn metadata_reader_does_not_read_past_block_end() {
    // 16-byte block: enough for revision + one max-length tag header
    // claim. If the walk respects the bound, it cannot panic from
    // index out-of-bounds. Unwind = `block.len() / min_tag_dsize +
    // 1 = 16/4 + 1 = 5`.
    let mut block: [u8; 16] = [0; 16];
    for byte in block.iter_mut() {
        *byte = kani::any();
    }
    let r = MetadataReader::new(&block).unwrap();
    // Whatever the walk produced, `committed_end` is within bounds.
    assert!(r.committed_end() <= block.len());
}
