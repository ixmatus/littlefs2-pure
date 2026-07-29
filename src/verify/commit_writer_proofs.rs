//! Writer-side commit construction: totality and structure.
//!
//! [`crate::meta::Commit`] is the only thing in the kernel that turns
//! tags into on-disk bytes. Every metadata write in the crate ends up
//! here, so a cursor that runs past the caller's buffer, an arithmetic
//! wrap in the offset math, or a commit that ends without its CCRC
//! terminator would be a whole-filesystem defect rather than a local
//! one. [`crate::verify::commit_proofs`] proves the reader survives
//! arbitrary bytes; these harnesses prove the writer only ever
//! produces bytes inside the buffer it was handed, and that what it
//! produces carries the commit terminator the reader looks for.
//!
//! # What is symbolic and what is bounded
//!
//! The revision counter, the tag's 11-bit type field, its 10-bit id,
//! its length field, and the body bytes are all symbolic. Three
//! assumptions bound them:
//!
//! - `type_field < 0x800`: the type field is 11 bits on disk. Same
//!   bound, same reason, as [`crate::verify::tag_proofs`].
//! - the abstract type is not `0x5`: [`Commit::tag`] rejects a CCRC
//!   tag with [`crate::Error::InvalidTag`] because the terminator is
//!   [`Commit::finish`]'s to emit. The rejection path is proved in
//!   [`commit_writer_rejects_a_ccrc_data_tag`]; the harnesses that
//!   want a successful append assume it away.
//! - `id <= 0x3FF`: [`crate::Tag::new`] documents the 10-bit bound and
//!   debug-asserts it. Passing a wider id is a caller bug, not an
//!   input the writer is expected to survive.
//!
//! Body length is bounded to [`MAX_BODY`] and the buffer to
//! [`BUF_LEN`]. Those two are budget, not specification: a commit's
//! shape does not vary with body length past the first byte, and a
//! small buffer keeps CBMC's symbolic byte count low enough to
//! discharge inside CI's per-harness timeout. Long bodies and long
//! blocks are covered by `tests/property_meta.rs` and by the
//! conformance and round-trip gates.
//!
//! # Stubbing `crc::update`
//!
//! All three harnesses stub [`crate::crc::update`] with a
//! nondeterministic `u32`, for the same reason
//! [`crate::verify::commit_proofs`] does: the property under test is
//! the cursor arithmetic and the emitted tag structure, neither of
//! which depends on the CRC's value, and CRC correctness is discharged
//! in [`crate::verify::crc_proofs`]. Because the stub returns an
//! arbitrary value, the proofs hold for *every* CRC the writer could
//! compute, which is a stronger statement than pinning one.
//!
//! # The round trip Kani could not close
//!
//! The natural companion property, "a commit this writer emits reads
//! back through [`crate::meta::MetadataReader`] as exactly one commit
//! ending in a CCRC," is not here, and not for lack of trying.
//! Feeding the writer's output to the reader puts the real CRC on both
//! sides, which is what the property needs, and CBMC's refinement loop
//! does not converge on it: with a 24-byte buffer and a symbolic body it
//! spent 407 seconds in symbolic execution alone; shrunk to a 20-byte
//! buffer, an empty body, and a tight `unwind(6)`, it still built a
//! 1.9-million-clause formula and was still refining at 300 seconds.
//! CI's per-harness budget is 360 seconds. The measurements are
//! recorded here so the next person does not re-derive them.
//!
//! That coverage lives in the other stacks instead: the writer-reader
//! round trip is `tests/property_meta.rs` over randomized tag streams,
//! and the writer-versus-C-reference round trip is the round-trip gate
//! (the C reference mounts an image this writer produced).
//!
//! # What these proofs do not claim
//!
//! Not byte-for-byte agreement with `lfs_dir_commit`, and not that the
//! reader accepts the output. These are totality and structure proofs
//! about the writer alone: it stays inside its buffer, its bounds
//! checks are exact, it refuses to write a commit terminator a caller
//! supplied, and every commit it emits ends in a well-formed CCRC tag.

use crate::error::Error;
use crate::meta::Commit;
use crate::tag::{Tag, TagType};

/// Nondeterministic [`crate::crc::update`] replacement for the
/// structural harnesses. Sound for proving cursor and tag-layout
/// properties: neither depends on the CRC's value, and the writer
/// must be correct for *every* value the stub can return.
///
/// `#[allow(dead_code)]`: rustc's dead-code pass does not see a use
/// through the `#[kani::stub(..)]` attribute, and CI compiles the
/// harnesses under `-D warnings`. Same footgun, same suppression, as
/// [`crate::verify::commit_proofs`]'s stub.
#[cfg(kani)]
#[allow(dead_code)]
fn crc_update_stub(_seed: u32, _data: &[u8]) -> u32 {
    kani::any()
}

/// Buffer handed to the writer. Holds the 4-byte revision header, one
/// tag word, a [`MAX_BODY`]-byte body, and the 8-byte CCRC, with four
/// bytes left over so the harness can tell "the writer stopped" from
/// "the writer filled the buffer exactly."
const BUF_LEN: usize = 24;

/// Symbolic body-length ceiling. See the module docs on budget.
const MAX_BODY: u16 = 4;

/// Draw the symbolic data tag the append harnesses share, along with
/// its body length. See the module docs for each assumption.
#[cfg(kani)]
fn any_data_tag() -> (Tag, u16) {
    let type_field: u16 = kani::any();
    // The on-disk type field is 11 bits.
    kani::assume(type_field < 0x800);
    // Abstract type 0x5 is the CRC family; `Commit::tag` rejects it.
    kani::assume((type_field >> 8) & 0x7 != 0x5);
    let id: u16 = kani::any();
    // `Tag::new` documents and debug-asserts the 10-bit id bound.
    kani::assume(id <= 0x3FF);
    let length: u16 = kani::any();
    // Budget bound, not a specification bound.
    kani::assume(length <= MAX_BODY);
    (Tag::new(true, TagType::from_bits(type_field), id, length), length)
}

/// The writer never writes outside the buffer it was handed, its
/// cursor lands exactly where the tag stream says it should, and the
/// commit it produces ends in a valid CCRC tag whose body is the
/// 4-byte CRC.
///
/// The CCRC check reads the emitted bytes back directly rather than
/// through [`crate::meta::MetadataReader`]: the tag word on disk is
/// the CCRC tag XORed against the preceding tag, so decoding it with
/// the data tag's bits is the structural statement "a CCRC terminator
/// is present at the end of this commit," independent of any reader
/// logic.
#[kani::proof]
#[kani::stub(crate::crc::update, crc_update_stub)]
fn commit_writer_stays_in_buffer_and_emits_ccrc_tail() {
    let revision: u32 = kani::any();
    let (tag, length) = any_data_tag();
    let body: [u8; MAX_BODY as usize] = kani::any();

    let mut buf = [0xFFu8; BUF_LEN];
    let written = {
        let mut commit = Commit::new(&mut buf, revision).expect("24 bytes is above the 8 minimum");
        assert!(commit.tag(tag, &body[..length as usize]).is_ok(), "tag must fit in the budget");
        assert_eq!(commit.bytes_written(), 4 + 4 + length as usize, "cursor after the data tag");
        assert!(commit.finish(0).is_ok(), "CCRC must fit in the budget");
        commit.bytes_written()
    };

    assert_eq!(written, 4 + 4 + length as usize + 8, "cursor after the CCRC");
    assert!(written <= BUF_LEN, "writer ran past the caller's buffer");

    // The CCRC tag word sits at `written - 8`, XOR-encoded against the
    // data tag that precedes it.
    let ccrc_off = written - 8;
    let raw = u32::from_be_bytes([
        buf[ccrc_off],
        buf[ccrc_off + 1],
        buf[ccrc_off + 2],
        buf[ccrc_off + 3],
    ]);
    let ccrc = Tag::from_bits(raw ^ tag.into_bits());
    assert!(ccrc.is_valid(), "the CCRC terminator must decode as a valid tag");
    assert!(ccrc.is_ccrc(), "the commit must end in a CCRC tag");
    assert_eq!(ccrc.body_len(), 4, "a plain CCRC body is the 4-byte CRC");
}

/// Both of the writer's bounds checks are exact: a tag is accepted
/// exactly when its `dsize` fits in the remaining buffer, a CCRC is
/// accepted exactly when its 8 bytes fit, and a rejection leaves the
/// cursor untouched so the caller can recover by compacting elsewhere.
///
/// The buffer is sized so the symbolic length straddles both
/// boundaries: with 16 bytes and a 4-byte revision header, a tag fits
/// for `length <= 8` and a subsequent CCRC fits only for `length == 0`.
#[kani::proof]
#[kani::stub(crate::crc::update, crc_update_stub)]
fn commit_writer_bounds_checks_are_exact() {
    const SMALL: usize = 16;
    let revision: u32 = kani::any();
    let type_field: u16 = kani::any();
    kani::assume(type_field < 0x800);
    kani::assume((type_field >> 8) & 0x7 != 0x5);
    let length: u16 = kani::any();
    // Straddles the 8-byte boundary in both directions. Body content
    // is irrelevant with the CRC stubbed, so it stays concrete.
    kani::assume(length <= 12);
    let body = [0u8; 12];
    let tag = Tag::new(true, TagType::from_bits(type_field), 0x3FF, length);

    let mut buf = [0xFFu8; SMALL];
    let mut commit = Commit::new(&mut buf, revision).expect("16 bytes is above the 8 minimum");
    let fits = 4 + 4 + length as usize <= SMALL;
    let result = commit.tag(tag, &body[..length as usize]);
    assert_eq!(result.is_ok(), fits, "the tag bounds check must be exact");
    if let Err(e) = result {
        assert_eq!(e, Error::OutOfRange, "an oversized tag is out of range");
        assert_eq!(commit.bytes_written(), 4, "a rejected tag must not move the cursor");
        return;
    }

    let ccrc_fits = 4 + 4 + length as usize + 8 <= SMALL;
    let finished = commit.finish(0);
    assert_eq!(finished.is_ok(), ccrc_fits, "the CCRC bounds check must be exact");
    if finished.is_err() {
        assert_eq!(
            commit.bytes_written(),
            4 + 4 + length as usize,
            "a rejected CCRC must not move the cursor"
        );
    }
}

/// A CCRC-typed tag offered to [`Commit::tag`] is rejected rather than
/// written: the commit terminator is [`Commit::finish`]'s to emit, and
/// a caller-supplied one would leave the running CRC describing bytes
/// that are not the ones on disk. The rejection is total over every
/// CCRC chunk and every body length.
#[kani::proof]
#[kani::stub(crate::crc::update, crc_update_stub)]
fn commit_writer_rejects_a_ccrc_data_tag() {
    let revision: u32 = kani::any();
    let chunk: u8 = kani::any();
    // `TagType::CommitCrc` covers chunks 0..=3; wider chunks decode as
    // `TagType::Unknown` and are not CCRCs at all.
    kani::assume(chunk <= 3);
    let length: u16 = kani::any();
    kani::assume(length <= MAX_BODY);
    let body = [0u8; MAX_BODY as usize];
    let tag = Tag::new(true, TagType::CommitCrc(chunk), 0x3FF, length);

    let mut buf = [0xFFu8; BUF_LEN];
    let mut commit = Commit::new(&mut buf, revision).expect("24 bytes is above the 8 minimum");
    let result = commit.tag(tag, &body[..length as usize]);
    assert_eq!(result, Err(Error::InvalidTag), "a CCRC data tag must be rejected");
    assert_eq!(commit.bytes_written(), 4, "a rejected tag must not move the cursor");
}
