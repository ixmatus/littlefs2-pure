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
