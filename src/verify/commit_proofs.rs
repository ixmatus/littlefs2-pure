//! Commit accept-or-reject dispatch totality.
//!
//! `MetadataReader::new` walks a block, verifying each commit's
//! CCRC and stopping at the first failure. The reader must never
//! panic regardless of the block's bytes: an attacker who can write
//! arbitrary content to a metadata pair (or a corrupted chip) must
//! get a clean parse-or-reject answer, not a process crash.
//!
//! Kani's symbolic-input loop budget caps the exhaustive proof at
//! short blocks. The fuzz harness in `fuzz/` extends coverage to
//! longer adversarial inputs.

use crate::meta::MetadataReader;

/// `MetadataReader::new` on a 32-byte block of arbitrary content
/// must not panic; it either returns `Ok(_)` with whatever commit
/// boundary it can verify or `Err(Error::Corrupt)` for a block too
/// short to even hold the revision header. No other failure mode.
///
/// 32 bytes is enough to hold the revision header + one tag word +
/// CCRC; Kani exhausts the byte space within its default budget.
/// Longer blocks are fuzzed.
#[kani::proof]
fn metadata_reader_does_not_panic_on_arbitrary_input() {
    let mut block: [u8; 32] = [0; 32];
    for byte in block.iter_mut() {
        *byte = kani::any();
    }
    // The result is allowed to be `Ok` (some prefix verifies) or
    // `Err` (block too short, which it isn't here, so this branch
    // is unreachable). What matters is that the call returns.
    let _ = MetadataReader::new(&block);
}

/// Same property, for an even smaller block. The reader returns
/// `Err(Error::Corrupt)` if the block is shorter than the 4-byte
/// revision header; this proof pins that lower bound.
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
/// where the property is exhaustively checkable.
#[kani::proof]
fn metadata_reader_does_not_read_past_block_end() {
    // 16-byte block: enough for revision + one max-length tag header
    // claim. If the walk respects the bound, it cannot panic from
    // index out-of-bounds.
    let mut block: [u8; 16] = [0; 16];
    for byte in block.iter_mut() {
        *byte = kani::any();
    }
    let r = MetadataReader::new(&block).unwrap();
    // Whatever the walk produced, `committed_end` is within bounds.
    assert!(r.committed_end() <= block.len());
}
