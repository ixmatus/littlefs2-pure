//! Power-loss safety scenarios.
//!
//! The invariant: a torn write at any page boundary leaves the
//! filesystem mountable as **either** the pre-commit state or the
//! post-commit state. Never a corrupt mid-state, never a different
//! state.
//!
//! Each scenario test:
//!
//! 1. Captures a pre-image of the storage (before the operation
//!    under test).
//! 2. Runs the operation through `TornWriteStorage` configured to
//!    "power off" at each program-call boundary from 1 to N.
//! 3. After power loss, mounts the resulting image and asserts the
//!    invariant: either the file appears in its pre-state (the
//!    operation didn't land) or its post-state (the operation
//!    fully landed). Anything else is a power-loss-safety
//!    regression.
//!
//! Each scenario is swept twice, through two different models of
//! where the power cut lands:
//!
//! 1. `TornWriteStorage` over a plain RAM device interrupts at the
//!    KERNEL's program call boundaries. The kernel hands a whole
//!    commit span to `program`, so these boundaries are coarse: the
//!    inline write below is a single call.
//! 2. `TornPartialStorage` inside `NorAlignedStorage` over
//!    `StrictNorStorage` interrupts at the DEVICE's program
//!    boundaries, and can land a prefix of the interrupted window with
//!    the rest of it left as it was (review coverage item V4, bead
//!    `lfs-hki`). The alignment adapter splits each commit span into
//!    `PROG_SIZE` windows, so this model is the finer one, and it
//!    reaches the case the first cannot express: a half programmed
//!    page, which the next mount must either read as absent or reject
//!    on its CRC, never accept as a commit.
//!
//! Strong semantics (review H7): a mount failure is acceptable ONLY
//! while the tear can have hit `Fs::format` itself. Once format has
//! completed, the device holds a valid filesystem, and a torn write
//! that leaves it unmountable is a bricked device — the exact
//! regression these sweeps exist to catch. The pre-H7 version of
//! this file accepted `Corrupt`/`Unformatted` at every trigger.

use littlefs2_pure::storage::Storage;
use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

/// Read `/log`'s content from a mounted post-tear image. Every error
/// here is a regression: the image mounted, so the read surface must
/// be coherent.
fn read_log<S: Storage>(fs: &mut Fs<S>) -> Vec<u8> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    if !fs.exists(Path::new("/log").unwrap(), &mut a, &mut b).unwrap() {
        return Vec::new();
    }
    let size = fs.size_of(Path::new("/log").unwrap(), &mut a, &mut b).unwrap();
    let mut out = vec![0u8; size as usize];
    fs.read_at_path(Path::new("/log").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    out
}

fn remount_and_read_log(torn: TornWriteStorage) -> Result<Vec<u8>, Error> {
    let storage = torn.into_inner();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)?;
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    if fs.exists(Path::new("/log").unwrap(), &mut a, &mut b)? {
        let size = fs.size_of(Path::new("/log").unwrap(), &mut a, &mut b)?;
        let mut out = vec![0u8; size as usize];
        fs.read_at_path(Path::new("/log").unwrap(), 0, &mut out, &mut a, &mut b)?;
        Ok(out)
    } else {
        Ok(Vec::new())
    }
}

/// Write "ONE" to `/log`: an inline file in a single commit. Generic
/// over the storage so the same sequence runs under both tear models.
fn inline_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
}

#[test]
fn inline_write_atomic_across_every_power_loss() {
    // Across every possible power-loss point, the FS must mount as
    // either: no /log (pre-state) or /log = "ONE" (post-state).
    let scenario = inline_scenario::<TornWriteStorage>;

    // Triggers count from format's first program (review L10: arming
    // over the scenario count alone under-covers the tail).
    let (fmt_calls, scenario_calls) = common::torn_call_counts(scenario);
    assert!(scenario_calls > 0, "scenario must perform at least one program call");

    for trigger in 1..=fmt_calls + scenario_calls + 5 {
        match common::run_torn_scenario(trigger, scenario) {
            common::TornRun::TornFormat => {
                assert!(
                    trigger <= fmt_calls,
                    "trigger {trigger}: format reported torn past its own \
                     {fmt_calls} program calls"
                );
            }
            common::TornRun::Image(image) => {
                let mut fs =
                    common::mount_image_strict(image, &format!("inline sweep trigger {trigger}"));
                let content = read_log(&mut fs);
                assert!(
                    content.is_empty() || content == b"ONE",
                    "trigger {trigger}: unexpected content {content:?}; \
                     invariant violated (must be pre state '' or post state 'ONE')"
                );
            }
        }
    }
}

/// The 200-byte seed `/log` every append sweep starts from.
fn ctz_seed_content() -> Vec<u8> {
    (0..200).map(|i| (i & 0xff) as u8).collect()
}

/// Append 16 bytes to `/log`: one tail fill program plus an UpdateCtz
/// commit. Generic over the storage so the same sequence runs under
/// both tear models.
fn append_scenario<S: Storage>(fs: &mut Fs<S>) {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let _ =
        fs.append_to_path(Path::new("/log").unwrap(), b"0123456789ABCDEF", &mut [], &mut a, &mut b);
}

#[test]
fn ctz_streaming_append_atomic_across_every_power_loss() {
    // Scenario: starting from a /log already populated with "HEAD",
    // append "TAIL" (which crosses the inline->CTZ boundary if
    // initial /log is > 128 bytes, but here it stays inline).
    //
    // We set up a CTZ-bearing log first (a 200-byte seed) so the
    // streaming append path is exercised. Then append 16 bytes
    // (single tail-fill program + UpdateCtz commit).
    let mut seed_storage = MemStorage::new();
    let mut seed_scratch = common::make_buffer();
    Fs::format(&mut seed_storage, &mut seed_scratch).unwrap();
    let seed_bytes = {
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(seed_storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let initial: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
        fs.write_to_path(Path::new("/log").unwrap(), &initial, &mut a, &mut b).unwrap();
        fs.into_storage()
    };
    let seed_data = seed_bytes.data.clone();

    let append_scenario = append_scenario::<TornWriteStorage>;

    // Count program calls for the append operation only.
    let total_calls = {
        let mut storage = MemStorage::new();
        storage.data = seed_data.clone();
        let torn = TornWriteStorage::new(storage, usize::MAX);
        let mut buf_a = common::make_buffer();
        let mut buf_b = common::make_buffer();
        let mut fs = Fs::mount(torn, &mut buf_a, &mut buf_b).unwrap();
        let pre = fs.storage().program_count;
        append_scenario(&mut fs);
        let post = fs.storage().program_count;
        post - pre
    };

    let mut initial: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
    let mut appended = initial.clone();
    appended.extend_from_slice(b"0123456789ABCDEF");

    for trigger in 1..=total_calls + 5 {
        let mut storage = MemStorage::new();
        storage.data = seed_data.clone();
        let torn = TornWriteStorage::new(storage, trigger);
        let result = {
            let mut buf_a = common::make_buffer();
            let mut buf_b = common::make_buffer();
            match Fs::mount(torn, &mut buf_a, &mut buf_b) {
                Ok(mut fs) => {
                    append_scenario(&mut fs);
                    let inner = fs.into_storage().into_inner();
                    remount_and_read_log(TornWriteStorage::new(inner, usize::MAX))
                }
                Err(e) => Err(e),
            }
        };

        match result {
            Ok(content) => {
                assert!(
                    content == initial || content == appended,
                    "trigger {trigger}: streaming append left {} bytes; \
                     must be pre-state ({} bytes) or post-state ({} bytes)",
                    content.len(),
                    initial.len(),
                    appended.len()
                );
            }
            Err(Error::Corrupt | Error::Unformatted) => {
                panic!(
                    "trigger {trigger}: append broke a previously-mountable FS \
                     (Corrupt/Unformatted post-append)"
                );
            }
            Err(e) => panic!("trigger {trigger}: unexpected error {e:?}"),
        }
    }

    // Touch unused vars so the compiler is happy.
    let _ = &mut initial;
}

/// The inline write sweep at DEVICE program granularity, with partial
/// window landings (review coverage item V4, bead `lfs-hki`).
///
/// The tear injector sits inside `NorAlignedStorage` over a strict NOR
/// device, so each trigger is a real page program, and each partial
/// landing leaves a page carrying the first `k` bytes of a commit with
/// the rest of the page still erased. Every such image must remount as
/// the pre state or the post state, and must answer the same on a
/// second consecutive mount (recovery is idempotent).
///
/// Landing lengths come from `common::NOR_PARTIAL_LANDINGS`; that
/// constant documents the sampling bound.
#[test]
fn inline_write_atomic_across_every_nor_program_landing() {
    let scenario = inline_scenario::<common::NorTornStorage>;
    let (fmt_calls, scenario_calls) = common::nor_torn_call_counts(scenario);
    assert!(scenario_calls > 0, "scenario must perform at least one device program");

    let mut witness = common::PartialLandingWitness::new();
    for partial in common::NOR_PARTIAL_LANDINGS {
        for trigger in 1..=fmt_calls + scenario_calls + 5 {
            let ctx = format!("nor inline sweep trigger {trigger}, partial landing {partial}");
            match common::run_nor_torn_scenario(trigger, partial, scenario) {
                common::TornRun::TornFormat => {
                    assert!(
                        trigger <= fmt_calls,
                        "{ctx}: format reported torn past its own {fmt_calls} device programs"
                    );
                }
                common::TornRun::Image(image) => {
                    witness.observe(partial, trigger, &image);
                    let mut fs = common::mount_nor_image_strict(image, &ctx);
                    let first = read_log(&mut fs);
                    assert!(
                        first.is_empty() || first == b"ONE",
                        "{ctx}: unexpected content {first:?}; must be pre state '' \
                         or post state 'ONE'"
                    );
                    let image = common::nor_image_of(fs);
                    let mut fs =
                        common::mount_nor_image_strict(image, &format!("{ctx}, second remount"));
                    assert_eq!(first, read_log(&mut fs), "{ctx}: state changed across remounts");
                }
            }
        }
    }
    witness.assert_partials_landed("nor inline sweep");
}

/// The CTZ streaming append sweep at DEVICE program granularity, with
/// partial window landings (review coverage item V4, bead `lfs-hki`).
///
/// This is the sweep the partial landing model matters most for: the
/// append fills the erased tail of a committed data block, so a torn
/// page leaves bytes past the committed EOF programmed. The committed
/// size must still read back exactly, and a follow up append must land
/// its own bytes rather than AND them into the residue (the C8 failure
/// mode, here reached through a partial page rather than a whole one).
#[test]
fn ctz_streaming_append_atomic_across_every_nor_program_landing() {
    let initial = ctz_seed_content();
    let mut appended = initial.clone();
    appended.extend_from_slice(b"0123456789ABCDEF");

    let seed_image = {
        let initial = &initial;
        common::nor_seed_image(move |fs| {
            let mut a = common::make_buffer();
            let mut b = common::make_buffer();
            fs.write_to_path(Path::new("/log").unwrap(), initial, &mut a, &mut b)
                .expect("seeding /log must succeed");
        })
    };

    let scenario = append_scenario::<common::NorTornStorage>;
    let (mount_calls, scenario_calls) = common::nor_seeded_call_counts(&seed_image, scenario);
    assert!(scenario_calls > 0, "scenario must perform at least one device program");

    let mut witness = common::PartialLandingWitness::new();
    for partial in common::NOR_PARTIAL_LANDINGS {
        for trigger in mount_calls + 1..=mount_calls + scenario_calls + 2 {
            let ctx = format!("nor append sweep trigger {trigger}, partial landing {partial}");
            let image = common::run_nor_torn_from_seed(&seed_image, trigger, partial, scenario);
            witness.observe(partial, trigger, &image);

            let mut fs = common::mount_nor_image_strict(image, &ctx);
            let first = read_log(&mut fs);
            assert!(
                first == initial || first == appended,
                "{ctx}: streaming append left {} bytes; must be pre state ({}) \
                 or post state ({})",
                first.len(),
                initial.len(),
                appended.len()
            );

            // Second consecutive mount answers the same.
            let image = common::nor_image_of(fs);
            let mut fs = common::mount_nor_image_strict(image, &format!("{ctx}, second remount"));
            assert_eq!(first, read_log(&mut fs), "{ctx}: state changed across remounts");

            // A follow up append must land its own bytes exactly. On a
            // half programmed tail page an implementation that reuses
            // the committed block would AND the new bytes into the torn
            // residue; the strict NOR device would sooner panic on the
            // 0 -> 1 flip.
            let mut a = common::make_buffer();
            let mut b = common::make_buffer();
            let mut scratch = vec![0u8; 2048];
            let follow = [0x33u8; 24];
            fs.append_to_path(Path::new("/log").unwrap(), &follow, &mut scratch, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("{ctx}: follow up append failed: {e:?}"));
            let after = read_log(&mut fs);
            assert_eq!(after.len(), first.len() + follow.len(), "{ctx}: follow up size");
            assert_eq!(&after[..first.len()], &first[..], "{ctx}: committed prefix corrupted");
            assert!(
                after[first.len()..].iter().all(|&x| x == 0x33),
                "{ctx}: appended bytes corrupted by torn page residue: {:02x?}",
                &after[first.len()..],
            );
        }
    }
    witness.assert_partials_landed("nor append sweep");
}
