//! Reproducers for the 2026-06 deep review findings C4 and C7
//! (beads lfs-w7w, lfs-njj): per-pair gstate read semantics and
//! gstate stealing when a pair is dropped from the global thread.
//!
//! Oracle: the C reference reads a pair's gstate contribution as the
//! single latest matching tag (`lfs_dir_getgstate` over
//! `lfs_dir_getslice`), and writers fold the pair's existing
//! contribution into every tag they commit, so each committed tag is
//! the pair's new total. `lfs_dir_drop` (lfs.c:1831, "// steal
//! state") XORs a dropped pair's contribution into the commit that
//! re-threads the survivor.

use littlefs2_pure::meta::{Commit, MetadataReader};
use littlefs2_pure::storage::Storage;
use littlefs2_pure::tag::{Tag, TagType, ID_NONE};
use littlefs2_pure::{Fs, Path};

mod common;
use common::MemStorage;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

fn p(s: &str) -> Path<'_> {
    Path::new(s).unwrap()
}

/// C4: a C-written log holds multiple MOVESTATE tags where each tag is
/// the pair's new TOTAL contribution; the latest tag wins. Craft a
/// root log whose tags are (in commit order) a non-zero total `M`
/// followed by an all-zero total (the C encoding of "this pair's
/// contribution returned to zero"). A latest-tag-wins reader decodes
/// no pending move; the v1.2.0 XOR-of-all-tags reader decoded the
/// phantom move `M ^ 0 = M` and mount recovery destroyed the live
/// entry `M` named.
#[test]
fn c4_latest_total_wins_over_xor_of_all_tags() {
    // Format and add a victim file at root id 1.
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.write_to_path(p("/victim"), b"precious", &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Append two commits to the root's active block, C-convention:
    // first a total naming a pending delete of id 1 in the root pair,
    // then an all-zero total (the move resolved; contribution back to
    // zero). The root pair is blocks {0, 1}; find the active one.
    let mut block0 = vec![0u8; MemStorage::BLOCK_SIZE];
    let mut block1 = vec![0u8; MemStorage::BLOCK_SIZE];
    storage.read(0, 0, &mut block0).unwrap();
    storage.read(1, 0, &mut block1).unwrap();
    let active: u32 = {
        let r0 = MetadataReader::new(&block0).unwrap();
        let r1 = MetadataReader::new(&block1).unwrap();
        u32::from(
            r1.has_commits()
                && (!r0.has_commits()
                    || littlefs2_pure::meta::rev_scmp(r1.revision(), r0.revision()) > 0),
        )
    };
    let mut active_buf = if active == 0 { block0 } else { block1 };
    let (end, ptag) = {
        let reader = MetadataReader::new(&active_buf).unwrap();
        (reader.committed_end(), reader.next_ptag())
    };

    // MoveState body: the Delete tag that would complete the move
    // (id 1 = /victim) plus the source pair address {0, 1}.
    let mut move_body = [0u8; 12];
    let del = Tag::new(true, TagType::Delete, 1, 0).into_bits();
    move_body[0..4].copy_from_slice(&del.to_le_bytes());
    move_body[4..8].copy_from_slice(&0u32.to_le_bytes());
    move_body[8..12].copy_from_slice(&1u32.to_le_bytes());

    let new_end = {
        let mut c = Commit::new_appending(&mut active_buf, end, ptag).unwrap();
        c.tag(Tag::new(true, TagType::MoveState, ID_NONE, 12), &move_body).unwrap();
        c.finish_padded(0, MemStorage::PROG_SIZE, MemStorage::BLOCK_SIZE).unwrap();
        // Second commit: the pair's contribution returns to zero; C
        // writes the new total, an all-zero body.
        c.tag(Tag::new(true, TagType::MoveState, ID_NONE, 12), &[0u8; 12]).unwrap();
        c.finish_padded(0, MemStorage::PROG_SIZE, MemStorage::BLOCK_SIZE).unwrap();
        c.bytes_written()
    };
    for off in (end..new_end).step_by(MemStorage::PROG_SIZE) {
        storage.program(active, off as u32, &active_buf[off..off + MemStorage::PROG_SIZE]).unwrap();
    }

    // Mount. Latest-total-wins decodes no pending move; XOR-of-all
    // decoded the phantom and recovery deleted /victim.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("C4: a balanced C-convention log must mount cleanly");
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs
        .resolve(p("/victim"), &mut a, &mut b)
        .expect("C4: phantom pending move deleted a live entry");
    assert_eq!(r.struct_body, b"precious");
}

/// Snapshot of the raw device contents.
fn device_bytes(storage: &mut MemStorage) -> Vec<u8> {
    let mut out = Vec::new();
    let mut block = vec![0u8; MemStorage::BLOCK_SIZE];
    for i in 0..MemStorage::BLOCK_COUNT {
        storage.read(i, 0, &mut block).unwrap();
        out.extend_from_slice(&block);
    }
    out
}

/// C7: a completed rename out of a directory leaves that directory's
/// pair with a non-zero (globally balanced) gstate total. `rmdir` of
/// that directory must steal the contribution into the un-thread
/// commit; otherwise the reachable aggregate is permanently non-zero
/// and every subsequent mount decodes a pending move against the dead
/// pair (observed pre-fix as `Error::Corrupt` at mount, since the
/// dead pair's live count no longer covers the decoded id).
#[test]
fn c7_rmdir_steals_dropped_pair_gstate() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.mkdir(p("/d"), &mut a, &mut b).unwrap();
        fs.write_to_path(p("/d/f"), b"moved", &mut a, &mut b).unwrap();
        // Cross-directory rename: source pair is /d's pair, which now
        // carries a non-zero gstate total balanced by the root's.
        fs.rename(p("/d/f"), p("/e"), &mut a, &mut b).unwrap();
        // /d is now empty; dropping it must steal its contribution.
        fs.rmdir(p("/d"), &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // First remount: must succeed with /e intact.
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)
        .expect("C7: mount failed after rmdir of a rename source dir");
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.resolve(p("/e"), &mut a, &mut b).expect("/e must survive");
    assert_eq!(r.struct_body, b"moved");
    storage = fs.into_storage();

    // The reachable aggregate must be zero: consecutive mounts make
    // no recovery writes, so the device bytes are stable across them.
    let before = device_bytes(&mut storage);
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        storage = fs.into_storage();
    }
    let after = device_bytes(&mut storage);
    assert_eq!(
        before, after,
        "C7: mount mutated the device; a residual gstate aggregate is firing futile recovery"
    );
}

/// Convention regression guard: this crate's own writer and reader
/// stay consistent under the C convention (each written tag is the
/// pair's new total, latest wins). Two renames from different source
/// directories into the root without an intervening compaction stack
/// two MoveState tags in the root's log; both files must survive a
/// remount and the aggregate must be quiescent.
#[test]
fn c4_two_moves_into_one_pair_stay_balanced() {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        fs.mkdir(p("/s1"), &mut a, &mut b).unwrap();
        fs.mkdir(p("/s2"), &mut a, &mut b).unwrap();
        fs.write_to_path(p("/s1/x"), b"xx", &mut a, &mut b).unwrap();
        fs.write_to_path(p("/s2/y"), b"yy", &mut a, &mut b).unwrap();
        fs.rename(p("/s1/x"), p("/x"), &mut a, &mut b).unwrap();
        fs.rename(p("/s2/y"), p("/y"), &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).expect("balanced image mounts");
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    assert_eq!(fs.resolve(p("/x"), &mut a, &mut b).unwrap().struct_body, b"xx");
    assert_eq!(fs.resolve(p("/y"), &mut a, &mut b).unwrap().struct_body, b"yy");
    storage = fs.into_storage();

    let before = device_bytes(&mut storage);
    {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        storage = fs.into_storage();
    }
    let after = device_bytes(&mut storage);
    assert_eq!(before, after, "consecutive mounts of a quiescent image must not write");
}
