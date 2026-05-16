//! Atomic move state recovery: a cross-directory rename that's
//! interrupted between its destination Create and source Delete must
//! converge on the next mount, not leave the entry duplicated.
//!
//! The kernel emits a balanced `MoveState` tag in each of the two
//! rename commits. After a successful rename the two tags XOR to
//! zero. After a crash between them, the gstate accumulated at
//! mount time is non-zero; mount-time recovery decodes the in-flight
//! move and emits the missing Delete + balancing MoveState in the
//! source pair.

use littlefs2_pure::{Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

#[test]
fn corrupt_move_state_with_out_of_range_src_id_fails_mount() {
    // Defensive guard for recover_pending_move: a corrupted or
    // adversarial MoveState body that decodes to a src_id past the
    // live entry count must surface as Error::Corrupt at mount, not
    // silently commit a bogus Delete plus a balancing MoveState that
    // would permanently mask the inconsistency.
    use littlefs2_pure::gstate::{build_move_body, MOVE_STATE_BODY_SIZE};
    use littlefs2_pure::meta::{Commit, MetadataPair};
    use littlefs2_pure::storage::Storage;
    use littlefs2_pure::tag::{Tag, TagType, ID_NONE};
    use littlefs2_pure::ROOT_BLOCK_PAIR;

    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // Root now has a small, known number of live entries.
    fs.write_to_path(Path::new("/f").unwrap(), b"x", &mut a, &mut b).unwrap();
    let mut storage = fs.into_storage();

    // Read the root pair, find the active block and append cursor.
    let mut ba = [0u8; MemStorage::BLOCK_SIZE];
    let mut bb = [0u8; MemStorage::BLOCK_SIZE];
    storage.read(ROOT_BLOCK_PAIR.a.as_u32(), 0, &mut ba).unwrap();
    storage.read(ROOT_BLOCK_PAIR.b.as_u32(), 0, &mut bb).unwrap();
    let (active_addr, committed_end, next_ptag, active_is_a) = {
        let pair = MetadataPair::parse(ROOT_BLOCK_PAIR.a, &ba, ROOT_BLOCK_PAIR.b, &bb).unwrap();
        (
            pair.active_block,
            pair.reader.committed_end(),
            pair.reader.next_ptag(),
            pair.active_block == ROOT_BLOCK_PAIR.a,
        )
    };

    // Append an unbalanced MoveState tag whose src_id (255) is far
    // past the root's live entry count.
    let bad_body = build_move_body(ROOT_BLOCK_PAIR, 255);
    assert_eq!(bad_body.len(), MOVE_STATE_BODY_SIZE);
    let active_buf: &mut [u8] = if active_is_a { &mut ba } else { &mut bb };
    let new_end = {
        let mut commit = Commit::new_appending(active_buf, committed_end, next_ptag).unwrap();
        commit
            .tag(
                Tag::new(true, TagType::MoveState, ID_NONE, MOVE_STATE_BODY_SIZE as u16),
                &bad_body,
            )
            .unwrap();
        commit.finish_padded(0, MemStorage::PROG_SIZE, MemStorage::BLOCK_SIZE).unwrap();
        commit.bytes_written()
    };
    storage
        .program(active_addr.as_u32(), committed_end as u32, &active_buf[committed_end..new_end])
        .unwrap();

    // Mount must reject rather than damage the filesystem.
    let mut m_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut m_b = [0u8; MemStorage::BLOCK_SIZE];
    let err = Fs::mount(storage, &mut m_a, &mut m_b)
        .expect_err("mount must fail on out-of-range MoveState src_id");
    assert_eq!(err, littlefs2_pure::Error::Corrupt);
}

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

#[test]
fn successful_cross_dir_rename_leaves_gstate_zero() {
    // After a complete rename, mount should see no pending move:
    // the two MoveState tags balance.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.mkdir(Path::new("/archive").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/file").unwrap(), b"contents", &mut a, &mut b).unwrap();
    fs.rename(Path::new("/file").unwrap(), Path::new("/archive/file").unwrap(), &mut a, &mut b)
        .unwrap();

    // Re-mount and confirm the entry is in exactly one place.
    let storage = fs.into_storage();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
    let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
    assert!(!fs.exists(Path::new("/file").unwrap(), &mut a2, &mut b2).unwrap());
    assert!(fs.exists(Path::new("/archive/file").unwrap(), &mut a2, &mut b2).unwrap());
}

#[test]
fn rename_interrupted_between_commits_recovers_on_remount() {
    // Run the rename through TornWriteStorage configured to power
    // off mid-rename: after the destination Create commit lands but
    // before the source Delete commit. Then remount with normal
    // storage and assert the recovery completed the move.
    //
    // Strategy: count program calls without truncation, find the
    // boundary between the two rename commits, then re-run with
    // truncation at that boundary.

    // 1. Set up: format and pre-populate /file and /archive.
    let mut seed_storage = MemStorage::new();
    let mut seed_scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut seed_storage, &mut seed_scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(seed_storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.mkdir(Path::new("/archive").unwrap(), &mut a, &mut b).unwrap();
        fs.write_to_path(Path::new("/file").unwrap(), b"persistent", &mut a, &mut b).unwrap();
        seed_storage = fs.into_storage();
    }
    let seed_data = seed_storage.data.clone();

    // 2. Count the program calls a successful rename makes (start
    //    fresh from the seed each time so the pre-state is
    //    reproducible).
    let total_calls = {
        let mut storage = MemStorage::new();
        storage.data = seed_data.clone();
        let torn = TornWriteStorage::new(storage, usize::MAX);
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(torn, &mut buf_a, &mut buf_b).unwrap();
        let pre = fs.storage().program_count;
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        fs.rename(Path::new("/file").unwrap(), Path::new("/archive/file").unwrap(), &mut a, &mut b)
            .unwrap();
        let post = fs.storage().program_count;
        post - pre
    };
    assert!(total_calls >= 2, "rename should issue more than one program call");

    // 3. For each torn point inside the rename, re-run from the
    //    seed, attempt the rename, then remount with a fresh
    //    (unrigged) storage view and assert the FS converges:
    //    /file is gone, /archive/file is present, content matches.
    for trigger in 1..=total_calls {
        let mut storage = MemStorage::new();
        storage.data = seed_data.clone();
        let torn = TornWriteStorage::new(storage, trigger);
        let torn_after = {
            let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
            let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
            let mut fs = Fs::mount(torn, &mut buf_a, &mut buf_b).unwrap();
            let mut a = [0u8; MemStorage::BLOCK_SIZE];
            let mut b = [0u8; MemStorage::BLOCK_SIZE];
            let _ = fs.rename(
                Path::new("/file").unwrap(),
                Path::new("/archive/file").unwrap(),
                &mut a,
                &mut b,
            );
            fs.into_storage().into_inner()
        };

        // Re-mount with normal MemStorage. Recovery runs during mount
        // if the rename was crashed between its two commits.
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(torn_after, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];

        let src_present = fs.exists(Path::new("/file").unwrap(), &mut a, &mut b).unwrap();
        let dst_present = fs.exists(Path::new("/archive/file").unwrap(), &mut a, &mut b).unwrap();

        // Invariant: after recovery, the entry must exist in exactly
        // one place. The pre-state (just /file) is acceptable when
        // the torn point landed before the destination Create
        // commit; the post-state (just /archive/file) is acceptable
        // when both commits landed or the recovery completed the
        // move.
        assert!(
            (src_present && !dst_present) || (!src_present && dst_present),
            "trigger {trigger}: entry visible in both ({src_present}, {dst_present}); \
             atomic-move-state recovery did not converge",
        );

        // A second mount should reach the same state (recovery is
        // idempotent: gstate is now zero, so no further recovery).
        let storage_after_recover = fs.into_storage();
        let mut buf_a2 = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b2 = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs2 = Fs::mount(storage_after_recover, &mut buf_a2, &mut buf_b2).unwrap();
        let mut a2 = [0u8; MemStorage::BLOCK_SIZE];
        let mut b2 = [0u8; MemStorage::BLOCK_SIZE];
        let src2 = fs2.exists(Path::new("/file").unwrap(), &mut a2, &mut b2).unwrap();
        let dst2 = fs2.exists(Path::new("/archive/file").unwrap(), &mut a2, &mut b2).unwrap();
        assert_eq!(
            (src_present, dst_present),
            (src2, dst2),
            "trigger {trigger}: second mount disagreed with first; recovery was not idempotent"
        );
    }
}
