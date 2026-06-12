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
//! `TornWriteStorage` interrupts at program-call boundaries, not
//! mid-program. Mid-program torns are NOR-internal (the chip is
//! responsible for not landing partial bytes; the FCRC tag the
//! writer emits is what guards against torn-within-program scenarios
//! at the reader side). Boundary torns are what an MCU-level power
//! cut would produce. Partial-program landings through
//! `NorAlignedStorage` are review coverage item V4 (bead lfs-hki).
//!
//! Strong semantics (review H7): a mount failure is acceptable ONLY
//! while the tear can have hit `Fs::format` itself. Once format has
//! completed, the device holds a valid filesystem, and a torn write
//! that leaves it unmountable is a bricked device — the exact
//! regression these sweeps exist to catch. The pre-H7 version of
//! this file accepted `Corrupt`/`Unformatted` at every trigger.

use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

/// Read `/log`'s content from a mounted post-tear image. Every error
/// here is a regression: the image mounted, so the read surface must
/// be coherent.
fn read_log(fs: &mut Fs<common::MemStorage>) -> Vec<u8> {
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

#[test]
fn inline_write_atomic_across_every_power_loss() {
    // Scenario: write "ONE" to /log (inline file, single commit).
    // Across every possible power-loss point, the FS must mount as
    // either: no /log (pre-state) or /log = "ONE" (post-state).
    let scenario = |fs: &mut Fs<TornWriteStorage>| {
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
    };

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
                     invariant violated (must be pre-state '' or post-state 'ONE')"
                );
            }
        }
    }
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

    let append_scenario = |fs: &mut Fs<TornWriteStorage>| {
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let _ = fs.append_to_path(
            Path::new("/log").unwrap(),
            b"0123456789ABCDEF",
            &mut [],
            &mut a,
            &mut b,
        );
    };

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
                    "trigger {trigger}: streaming-append left {} bytes; \
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
