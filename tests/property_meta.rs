//! Property tests for the metadata block reader.
//!
//! The synthetic builder in `common::BlockBuilder` writes valid blocks
//! mirroring the C reference's `lfs_dir_commit` byte layout. The reader
//! walks them. The composition is a roundtrip: every (tag, body) pair fed
//! into the builder reappears when the reader iterates, in order.
//!
//! The builder is *not* the reader's inverse by construction (they are
//! independent implementations of the same algorithm), so this is a
//! genuine cross check, not a self consistency invariant.

extern crate alloc;

use littlefs2_pure::meta::{Commit, MetadataReader};
use littlefs2_pure::tag::{Tag, TagType};
use proptest::prelude::*;

mod common;
use common::BlockBuilder;

/// Strategy for a [`Tag`] of a non CCRC type with a body that fits in the
/// 10 bit length field. The id and length are bounded so we generate tags
/// the builder can emit.
fn arb_data_tag() -> impl Strategy<Value = (Tag, Vec<u8>)> {
    let ty = prop_oneof![
        Just(TagType::RegularFile),
        Just(TagType::Directory),
        Just(TagType::Superblock),
        Just(TagType::DirStruct),
        Just(TagType::InlineStruct),
        Just(TagType::CtzStruct),
        any::<u8>().prop_map(TagType::UserAttr),
        Just(TagType::Create),
        Just(TagType::SoftTail),
        Just(TagType::HardTail),
    ];
    (ty, 0u16..=0x3ff, 0usize..=32).prop_flat_map(|(ty, id, body_len)| {
        let body_len = body_len as u16;
        let tag = Tag::new(true, ty, id, body_len);
        let body = proptest::collection::vec(any::<u8>(), body_len as usize..=body_len as usize);
        (Just(tag), body)
    })
}

/// Strategy for a Delete tag (no body).
fn arb_delete_tag() -> impl Strategy<Value = (Tag, Vec<u8>)> {
    (0u16..=0x3ff).prop_map(|id| (Tag::new(true, TagType::Delete, id, 0x3FF), Vec::new()))
}

fn arb_tag_with_body() -> impl Strategy<Value = (Tag, Vec<u8>)> {
    prop_oneof![arb_data_tag(), arb_delete_tag()]
}

proptest! {
    /// Build a single commit block from a list of tags, then read it back.
    /// The reader's iterator emits the same tags plus one CCRC at the end.
    #[test]
    fn single_commit_roundtrip(
        revision: u32,
        tags in proptest::collection::vec(arb_tag_with_body(), 0..16),
    ) {
        let mut builder = BlockBuilder::new(2048, revision).unwrap();
        for (tag, body) in &tags {
            builder.tag(*tag, body).unwrap();
        }
        builder.commit(0).unwrap();
        let block = builder.finish();

        let r = MetadataReader::new(&block).unwrap();
        prop_assert_eq!(r.revision(), revision);
        prop_assert!(r.has_commits());

        let mut iter = r.iter_tags();
        for (expected_tag, expected_body) in &tags {
            let entry = iter.next().expect("ran out of tags before input was drained");
            prop_assert_eq!(entry.tag, *expected_tag);
            prop_assert_eq!(entry.body, expected_body.as_slice());
        }
        // Trailing CCRC.
        let ccrc_entry = iter.next().expect("missing trailing CCRC");
        prop_assert!(ccrc_entry.tag.is_ccrc());
        prop_assert!(iter.next().is_none());
    }

    /// Multi commit roundtrip: alternate CCRC chunks (0, 1, 0, 1, ...) and
    /// verify that the parity flip threads correctly across commits.
    #[test]
    fn multi_commit_roundtrip(
        revision: u32,
        commits in proptest::collection::vec(
            proptest::collection::vec(arb_tag_with_body(), 0..8),
            1..6,
        ),
    ) {
        let mut builder = BlockBuilder::new(4096, revision).unwrap();
        for (i, tags) in commits.iter().enumerate() {
            for (tag, body) in tags {
                builder.tag(*tag, body).unwrap();
            }
            builder.commit((i & 1) as u8).unwrap();
        }
        let block = builder.finish();

        let r = MetadataReader::new(&block).unwrap();
        prop_assert_eq!(r.revision(), revision);

        let mut iter = r.iter_tags();
        for tags in &commits {
            for (expected_tag, expected_body) in tags {
                let entry = iter.next().expect("ran out of tags before input drained");
                prop_assert_eq!(entry.tag, *expected_tag);
                prop_assert_eq!(entry.body, expected_body.as_slice());
            }
            // Each commit ends with a CCRC.
            let ccrc = iter.next().expect("missing CCRC at commit boundary");
            prop_assert!(ccrc.tag.is_ccrc());
        }
        prop_assert!(iter.next().is_none());
    }

    /// The kernel's slice-based `Commit` builder produces a byte layout
    /// that the `MetadataReader` can parse and walk. Asserts the tag
    /// stream is reproduced verbatim (including the trailing CCRC).
    #[test]
    fn kernel_commit_builder_roundtrips(
        revision: u32,
        tags in proptest::collection::vec(arb_tag_with_body(), 0..16),
    ) {
        let mut buf = alloc::vec![0xFFu8; 2048];
        {
            let mut c = Commit::new(&mut buf, revision).unwrap();
            for (tag, body) in &tags {
                c.tag(*tag, body).unwrap();
            }
            c.finish(0).unwrap();
        }

        let r = MetadataReader::new(&buf).unwrap();
        prop_assert_eq!(r.revision(), revision);
        prop_assert!(r.has_commits());
        let mut iter = r.iter_tags();
        for (expected_tag, expected_body) in &tags {
            let e = iter.next().expect("ran out of tags");
            prop_assert_eq!(e.tag, *expected_tag);
            prop_assert_eq!(e.body, expected_body.as_slice());
        }
        let ccrc = iter.next().expect("missing CCRC");
        prop_assert!(ccrc.tag.is_ccrc());
        prop_assert!(iter.next().is_none());
    }

    /// Corrupting any single byte of the committed region invalidates at
    /// least one commit. (May invalidate more if the corruption falls in
    /// an earlier commit, since later commits depend on the running ptag
    /// chain.)
    #[test]
    fn single_byte_corruption_invalidates(
        revision: u32,
        tags in proptest::collection::vec(arb_data_tag(), 1..5),
        flip_index in 0usize..512,
        flip_mask in 1u8..=0xFF,
    ) {
        let mut builder = BlockBuilder::new(512, revision).unwrap();
        for (tag, body) in &tags {
            builder.tag(*tag, body).unwrap();
        }
        builder.commit(0).unwrap();
        let mut block = builder.finish();
        let committed_end = block.len();
        // Recompute committed_end from a clean parse.
        let clean = MetadataReader::new(&block).unwrap();
        let clean_end = clean.committed_end();
        prop_assume!(clean_end > 0);

        let flip_index = flip_index % clean_end;
        let original = block[flip_index];
        block[flip_index] ^= flip_mask;
        prop_assume!(block[flip_index] != original);

        let r = MetadataReader::new(&block).unwrap();
        // The corruption must shorten the committed region. (Equal length
        // would mean the corruption was in the CCRC body itself flipping
        // to a different but consistent value, which is statistically
        // negligible for a CRC32; assert strictly less.)
        prop_assert!(r.committed_end() < clean_end,
            "corruption at byte {flip_index} (mask {flip_mask:#x}) did not shorten committed region: \
             {clean_end} -> {}", r.committed_end());
        let _ = committed_end; // silence unused
    }
}
