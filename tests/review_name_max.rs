//! Review M2 (`lfs-ax2`): the raw-name write APIs must reject a name
//! longer than `NAME_MAX` (255), not the tag length field's `0x3FF`
//! (1023). A name in `(255, 1023]` was accepted before the fix and
//! produced an entry unreachable or wrongly resolved under the C
//! reference, which caps names at `LFS2_NAME_MAX`.
//!
//! At this geometry a `NAME_MAX`-length name does not fit a 256-byte
//! metadata block, so the boundary is asserted by the error *kind*:
//! `NAME_MAX + 1` is rejected with `InvalidPath` (the length gate),
//! while `NAME_MAX` passes the gate (it may still fail for lack of room,
//! but never with `InvalidPath`).

use littlefs2_pure::{Error, Fs, Path, NAME_MAX};

mod common;
use common::MemStorage;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).unwrap()
}

#[test]
fn raw_name_over_name_max_is_rejected() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    let too_long = vec![b'a'; NAME_MAX + 1]; // 256 bytes

    // Every raw-name write entry point rejects it with InvalidPath,
    // before any room or allocation consideration.
    assert_eq!(
        fs.write_inline_to_root(&too_long, b"x", &mut a, &mut b),
        Err(Error::InvalidPath),
        "write_inline_to_root must reject a name longer than NAME_MAX"
    );
    assert_eq!(
        fs.write_ctz_to_root(&too_long, &[0u8; 200], &mut a, &mut b),
        Err(Error::InvalidPath),
        "write_ctz_to_root must reject a name longer than NAME_MAX"
    );
    assert_eq!(
        fs.write_to_root(&too_long, b"x", &mut a, &mut b),
        Err(Error::InvalidPath),
        "write_to_root must reject a name longer than NAME_MAX"
    );
}

#[test]
fn name_at_name_max_passes_the_length_gate() {
    let mut fs = make_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();

    // Exactly NAME_MAX is a legal length. At this geometry it will not
    // fit a 256-byte block, so it may fail for lack of room, but it must
    // never be rejected as InvalidPath (that would mean the gate rejects
    // a legal name).
    let at_max = vec![b'a'; NAME_MAX]; // 255 bytes
    let res = fs.write_inline_to_root(&at_max, b"x", &mut a, &mut b);
    assert_ne!(res, Err(Error::InvalidPath), "a NAME_MAX-length name is legal");
}

#[test]
fn path_component_over_name_max_is_rejected_at_construction() {
    // The Path-based write APIs cap component length in Path::new, so an
    // over-long component never reaches the writer. This confirms the two
    // validation sites agree on NAME_MAX.
    let too_long = format!("/{}", "a".repeat(NAME_MAX + 1));
    assert_eq!(Path::new(&too_long).err(), Some(Error::InvalidPath));

    let at_max = format!("/{}", "a".repeat(NAME_MAX));
    assert!(Path::new(&at_max).is_ok(), "a NAME_MAX-length component is a valid path");
}
