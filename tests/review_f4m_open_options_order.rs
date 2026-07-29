//! Review `lfs-f4m`: [`OpenOptions`] setters must commute.
//!
//! `append(true)` used to set the `write` field as a side effect while
//! `write(on)` overwrote that field unconditionally, so the two setters
//! did not commute: `.append(true).write(false)` and
//! `.write(false).append(true)` built different values from the same
//! two calls. The standard library has no such order dependence,
//! because its fields are independent and its access predicate is
//! `write || append`, read at open time.
//!
//! The fix makes the fields independent here too and normalizes at read
//! time through a crate internal `writable()` accessor, which every
//! consumer uses: the access mode check in `Fs::open`, the L6 create and
//! truncate predicate, `File::write`, and `File::set_len`.
//!
//! # The behavior this changes
//!
//! `.append(true).write(false)` is now a writable append mode open,
//! matching `std`. Before the fix it was uniformly non writable: the
//! `write` field had been cleared, so `Fs::open` rejected it outright
//! when no `read` was set, and `File::write` refused even when `read`
//! was. Nothing that worked before stops working; an options value that
//! was rejected is now accepted, which is why the L6 rejections are
//! re-pinned below in their own suite and re-checked here.

use littlefs2_pure::{Error, Fs, OpenOptions, Path};

mod common;
use common::MemStorage;

fn fresh_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).unwrap()
}

/// Open `path` with `options`, write `data`, sync, and report the
/// outcome. `Ok(size)` is the file size after the session.
fn open_write_sync(
    fs: &mut Fs<MemStorage>,
    path: &str,
    options: OpenOptions,
    data: &[u8],
) -> Result<u32, Error> {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut file = fs.open(Path::new(path).unwrap(), options, &mut a, &mut b)?;
    file.write(data, &mut a, &mut b)?;
    let size = file.size();
    file.sync(&mut a, &mut b)?;
    Ok(size)
}

// ---- Order independence ----

/// The headline: the same two calls in either order must build the same
/// value, so the same open must have the same outcome.
#[test]
fn append_and_write_setters_commute() {
    let a_then_w = OpenOptions::new().append(true).write(false).create(true);
    let w_then_a = OpenOptions::new().write(false).append(true).create(true);

    let mut fs1 = fresh_fs();
    let r1 = open_write_sync(&mut fs1, "/f", a_then_w, b"hello");
    let mut fs2 = fresh_fs();
    let r2 = open_write_sync(&mut fs2, "/f", w_then_a, b"hello");

    assert_eq!(
        r1, r2,
        "append(true).write(false) and write(false).append(true) are the same two \
         calls; the order they arrive in must not change the value they build"
    );
    assert_eq!(r1, Ok(5), "and per std both name append access, so both are writable");
}

/// The commuting property holds for the `write(true)` pairing too: a
/// later `append(true)` must not be needed to keep write access, and an
/// earlier one must not be needed either.
#[test]
fn append_and_write_true_setters_commute() {
    let a_then_w = OpenOptions::new().append(true).write(true).create(true);
    let w_then_a = OpenOptions::new().write(true).append(true).create(true);

    let mut fs1 = fresh_fs();
    let r1 = open_write_sync(&mut fs1, "/f", a_then_w, b"hello");
    let mut fs2 = fresh_fs();
    let r2 = open_write_sync(&mut fs2, "/f", w_then_a, b"hello");
    assert_eq!(r1, r2);
    assert_eq!(r1, Ok(5));
}

/// `write(false)` after `write(true)` still clears write access, so the
/// normalization did not turn `write` into a latch.
#[test]
fn write_false_still_clears_write_access() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let r = fs.open(
        Path::new("/f").unwrap(),
        OpenOptions::new().read(true).write(true).write(false).create(true),
        &mut a,
        &mut b,
    );
    assert_eq!(r.err(), Some(Error::InvalidPath), "create needs write or append access");
}

// ---- Append only opens are writable ----

/// An options value naming only `append` grants write access, exactly
/// as `std::fs::OpenOptions::new().append(true)` does.
#[test]
fn append_only_open_can_create_and_write() {
    let mut fs = fresh_fs();
    let size = open_write_sync(&mut fs, "/f", OpenOptions::new().append(true).create(true), b"one")
        .expect("append only must grant write access");
    assert_eq!(size, 3);

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 16];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"one");
}

/// Append mode still appends: a second session lands after the first
/// content regardless of where the cursor is left.
#[test]
fn append_only_open_appends_rather_than_overwrites() {
    let mut fs = fresh_fs();
    open_write_sync(&mut fs, "/f", OpenOptions::new().append(true).create(true), b"one").unwrap();
    let size =
        open_write_sync(&mut fs, "/f", OpenOptions::new().append(true), b"two").expect("reopen");
    assert_eq!(size, 6, "the second write must land at end of file");

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let mut out = [0u8; 16];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"onetwo");
}

/// `set_len` shares the write gate, so append access licenses it too.
#[test]
fn append_only_open_can_set_len() {
    let mut fs = fresh_fs();
    open_write_sync(&mut fs, "/f", OpenOptions::new().append(true).create(true), b"onetwo")
        .unwrap();

    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    {
        let mut file = fs
            .open(Path::new("/f").unwrap(), OpenOptions::new().append(true), &mut a, &mut b)
            .unwrap();
        file.set_len(3, &mut a, &mut b).expect("append access must license set_len");
        file.sync(&mut a, &mut b).unwrap();
    }

    let mut out = [0u8; 16];
    let n = fs.read_at_path(Path::new("/f").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"one");
}

// ---- The rejections that must survive ----

/// No access mode at all is still rejected, in either setter order.
#[test]
fn no_access_mode_is_still_rejected() {
    for options in [
        OpenOptions::new(),
        OpenOptions::new().append(false),
        OpenOptions::new().write(false).append(false),
        OpenOptions::new().append(false).write(false),
        OpenOptions::new().create(true),
        OpenOptions::new().truncate(true),
    ] {
        let mut fs = fresh_fs();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let r = fs.open(Path::new("/f").unwrap(), options, &mut a, &mut b);
        assert_eq!(r.err(), Some(Error::InvalidPath), "options {options:?}");
    }
}

/// A read only open still refuses writes; widening the write predicate
/// must not widen it to read only handles.
#[test]
fn a_read_only_open_still_refuses_writes() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    // Through `File`, so the content lands in a CTZ chain; a
    // non truncating `File` open of an *inline* file is a separate
    // unsupported case (`Error::OutOfRange`) and would mask this one.
    open_write_sync(&mut fs, "/f", OpenOptions::new().write(true).create(true), b"content")
        .unwrap();

    let mut file =
        fs.open(Path::new("/f").unwrap(), OpenOptions::new().read(true), &mut a, &mut b).unwrap();
    assert_eq!(file.write(b"x", &mut a, &mut b).err(), Some(Error::InvalidPath));
    assert_eq!(file.set_len(0, &mut a, &mut b).err(), Some(Error::InvalidPath));
}

/// The L6 rule in this suite's own terms: `create` with a read only
/// access mode is still rejected, and so is `truncate`.
#[test]
fn create_and_truncate_still_need_write_or_append() {
    for options in
        [OpenOptions::new().read(true).create(true), OpenOptions::new().read(true).truncate(true)]
    {
        let mut fs = fresh_fs();
        let mut a = common::make_buffer();
        let mut b = common::make_buffer();
        let r = fs.open(Path::new("/f").unwrap(), options, &mut a, &mut b);
        assert_eq!(r.err(), Some(Error::InvalidPath), "options {options:?}");
    }
}

/// And the L6 permission: append alone licenses `create`, which was
/// already true and must stay true now that it is normalized rather
/// than latched.
#[test]
fn append_alone_still_licenses_create() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let file = fs
        .open(
            Path::new("/f").unwrap(),
            OpenOptions::new().append(true).create(true),
            &mut a,
            &mut b,
        )
        .expect("append access licenses create");
    drop(file);
}
