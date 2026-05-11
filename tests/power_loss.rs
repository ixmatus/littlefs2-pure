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
//! cut would produce.

use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::{MemStorage, TornWriteStorage};

/// Count how many `program` calls a fresh-FS scenario triggers,
/// without any torn-write injection. This is the upper bound on
/// scenarios we need to test: there are this many possible
/// power-loss points.
fn count_program_calls<F: FnOnce(&mut Fs<TornWriteStorage>)>(scenario: F) -> usize {
    let storage = MemStorage::new();
    let mut torn = TornWriteStorage::new(storage, usize::MAX);
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut torn, &mut scratch).unwrap();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(torn, &mut buf_a, &mut buf_b).unwrap();
    let pre_count = fs.storage().program_count;
    scenario(&mut fs);
    let post_count = fs.storage().program_count;
    post_count - pre_count
}

/// Run a scenario with power loss at the `i`-th program call
/// (1-indexed). Returns the post-mount file content if the
/// scenario completed; `None` if the mount fails (allowed —
/// `Unformatted` / `Corrupt` from a torn format is acceptable;
/// the caller asserts the appropriate variant).
fn run_torn_at<F>(scenario: F, trigger_at: usize) -> Result<Vec<u8>, Error>
where
    F: FnOnce(&mut Fs<TornWriteStorage>),
{
    let storage = MemStorage::new();
    let mut torn = TornWriteStorage::new(storage, trigger_at);

    // First, format. If format gets torn, the chip is in a partial
    // state and Fs::mount will return Unformatted or Corrupt.
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    let format_result = Fs::format(&mut torn, &mut scratch);
    if format_result.is_err() {
        // Format itself was torn. The chip is unformatted (still all
        // 0xFF) or partially programmed (Corrupt). Bail.
        return remount_and_read_log(torn);
    }
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        match Fs::mount(torn, &mut buf_a, &mut buf_b) {
            Ok(mut fs) => {
                // Run the scenario; the storage may fail mid-call.
                scenario(&mut fs);
                let inner = fs.into_storage();
                remount_and_read_log(inner)
            }
            Err(e) => Err(e),
        }
    }
}

fn remount_and_read_log(torn: TornWriteStorage) -> Result<Vec<u8>, Error> {
    let storage = torn.into_inner();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b)?;
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
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
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let _ = fs.write_to_path(Path::new("/log").unwrap(), b"ONE", &mut a, &mut b);
    };

    let total = count_program_calls(scenario);
    assert!(total > 0, "scenario must perform at least one program call");

    for trigger in 1..=total + 5 {
        let result = run_torn_at(scenario, trigger);
        match result {
            Ok(content) => {
                assert!(
                    content.is_empty() || content == b"ONE",
                    "trigger {trigger}: unexpected content {content:?}; \
                     invariant violated (must be pre-state '' or post-state 'ONE')"
                );
            }
            // Mount errors are acceptable for torn formats and other
            // pre-FS states (Unformatted / Corrupt). Any other error
            // is a regression.
            Err(Error::Unformatted | Error::Corrupt) => {}
            Err(e) => panic!("trigger {trigger}: unexpected error {e:?}"),
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
    let mut seed_scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut seed_storage, &mut seed_scratch).unwrap();
    let seed_bytes = {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(seed_storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let initial: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
        fs.write_to_path(Path::new("/log").unwrap(), &initial, &mut a, &mut b).unwrap();
        fs.into_storage()
    };
    let seed_data = seed_bytes.data.clone();

    let append_scenario = |fs: &mut Fs<TornWriteStorage>| {
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
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
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
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
            let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
            let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
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
