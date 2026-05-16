//! Integration tests for the stateful [`File`] handle.
//!
//! The headline property is **commit batching**: a session of many
//! [`File::write`] calls touches the metadata pair exactly once at
//! [`File::sync`] time. Tests verify the batching invariant by
//! counting metadata-pair revisions before and after a session, plus
//! the usual read-back / remount-survival / mode-rejection
//! invariants for a file API.

use littlefs2_pure::{Fs, OpenOptions, Path, SeekFrom};

mod common;
use common::MemStorage;

extern crate alloc as core_alloc;
use core_alloc::vec;

fn fresh_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    Fs::mount(storage, &mut a, &mut b).unwrap()
}

fn root_revision(fs: &mut Fs<MemStorage>) -> u32 {
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let pair = fs.read_pair(fs.root(), &mut a, &mut b).unwrap();
    pair.reader.revision()
}

#[test]
fn open_create_then_write_then_sync_persists() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/log").unwrap();

    {
        let mut file = fs
            .open(path, OpenOptions::new().write(true).create(true).append(true), &mut a, &mut b)
            .unwrap();
        for _ in 0..32 {
            assert_eq!(file.write(&[b'x'; 16], &mut a, &mut b).unwrap(), 16);
        }
        file.close(&mut a, &mut b).unwrap();
    }

    // Read back: 32 * 16 = 512 bytes of 'x'.
    let mut out = vec![0u8; 1024];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 512);
    assert!(out[..n].iter().all(|&v| v == b'x'));
}

#[test]
fn batched_writes_amortize_to_one_commit() {
    // The headline File property: many File::write calls produce ONE
    // metadata-pair revision bump at sync, instead of one per call.
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/log").unwrap();

    // Materialize the file first as a CTZ-backed file (past
    // INLINE_MAX) so the open() goes through the CTZ path. The File
    // handle batches writes against CTZ chains; inline-only upserts
    // belong on the path-based API.
    fs.write_to_path(path, &[b'i'; 200], &mut a, &mut b).unwrap();
    let rev_before = root_revision(&mut fs);

    {
        let mut file =
            fs.open(path, OpenOptions::new().write(true).append(true), &mut a, &mut b).unwrap();
        for _ in 0..16 {
            file.write(&[b'y'; 32], &mut a, &mut b).unwrap();
        }
        file.close(&mut a, &mut b).unwrap();
    }

    let rev_after = root_revision(&mut fs);
    // Each metadata-pair update bumps the revision by one when the
    // commit lands as an append, or by one when a compact rotates
    // the pair (still one bump per write call). After the File
    // session, we expect at most one bump (the single sync), not 16.
    assert!(
        rev_after - rev_before <= 1,
        "root revision should bump at most once across 16 batched writes (before={rev_before}, after={rev_after})",
    );

    // Read back: 200 + 16*32 = 712 bytes.
    let mut out = vec![0u8; 1024];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 712);
    assert!(out[..200].iter().all(|&v| v == b'i'));
    assert!(out[200..n].iter().all(|&v| v == b'y'));
}

#[test]
fn drop_without_sync_discards_writes() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/log").unwrap();
    fs.write_to_path(path, &[b'i'; 200], &mut a, &mut b).unwrap();

    {
        let mut file =
            fs.open(path, OpenOptions::new().write(true).append(true), &mut a, &mut b).unwrap();
        file.write(&[b'z'; 32], &mut a, &mut b).unwrap();
        // Drop without sync. The new chain blocks are on flash but
        // the metadata-pair entry was never updated, so the file
        // remains at its pre-open state from the Fs's point of
        // view. The dropped blocks become orphan and are reclaimed
        // by the next allocator scan.
        drop(file);
    }
    let mut out = vec![0u8; 256];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 200, "pre-open size persists when sync was skipped");
    assert!(out[..n].iter().all(|&v| v == b'i'));
}

#[test]
fn read_via_file_handle_matches_read_at_path() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/big").unwrap();
    let payload: alloc::vec::Vec<u8> = (0u32..512).map(|i| (i % 251) as u8).collect();
    fs.write_to_path(path, &payload, &mut a, &mut b).unwrap();

    let mut out = vec![0u8; 512];
    {
        let mut file = fs.open(path, OpenOptions::new().read(true), &mut a, &mut b).unwrap();
        let mut total = 0usize;
        while total < out.len() {
            let n = file.read(&mut out[total..], &mut a, &mut b).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, payload.len());
        // Drop the read-only file (no sync needed).
    }
    assert_eq!(out, payload);
}

#[test]
fn seek_then_read_returns_offset_content() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/seek").unwrap();
    let payload: alloc::vec::Vec<u8> = (0u32..400).map(|i| (i % 251) as u8).collect();
    fs.write_to_path(path, &payload, &mut a, &mut b).unwrap();

    let mut file = fs.open(path, OpenOptions::new().read(true), &mut a, &mut b).unwrap();
    let _ = file.seek(SeekFrom::Start(200)).unwrap();
    let mut out = vec![0u8; 100];
    let n = file.read(&mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 100);
    assert_eq!(&out[..], &payload[200..300]);
}

#[test]
fn set_len_shrink_drops_tail_bytes() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/shrink").unwrap();
    let payload: alloc::vec::Vec<u8> = (0u32..400).map(|i| (i % 251) as u8).collect();
    fs.write_to_path(path, &payload, &mut a, &mut b).unwrap();

    {
        let mut file = fs.open(path, OpenOptions::new().write(true), &mut a, &mut b).unwrap();
        file.set_len(120, &mut a, &mut b).unwrap();
        assert_eq!(file.size(), 120);
        file.close(&mut a, &mut b).unwrap();
    }

    let new_size = fs.size_of(path, &mut a, &mut b).unwrap();
    assert_eq!(new_size, 120);
    let mut out = vec![0u8; 200];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 120);
    assert_eq!(&out[..n], &payload[..120]);
}

#[test]
fn set_len_extend_zero_fills() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/extend").unwrap();
    // Start with a small CTZ file.
    fs.write_to_path(path, &[b'a'; 200], &mut a, &mut b).unwrap();

    {
        let mut file = fs.open(path, OpenOptions::new().write(true), &mut a, &mut b).unwrap();
        file.set_len(320, &mut a, &mut b).unwrap();
        assert_eq!(file.size(), 320);
        file.close(&mut a, &mut b).unwrap();
    }

    let mut out = vec![0u8; 400];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 320);
    assert!(out[..200].iter().all(|&v| v == b'a'));
    assert!(out[200..n].iter().all(|&v| v == 0));
}

#[test]
fn truncate_open_then_rewrite_replaces_content() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/rewrite").unwrap();
    fs.write_to_path(path, &[b'o'; 256], &mut a, &mut b).unwrap();

    {
        let mut file = fs
            .open(path, OpenOptions::new().write(true).truncate(true).append(true), &mut a, &mut b)
            .unwrap();
        assert_eq!(file.size(), 0);
        file.write(&[b'n'; 200], &mut a, &mut b).unwrap();
        file.close(&mut a, &mut b).unwrap();
    }

    let mut out = vec![0u8; 400];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 200);
    assert!(out[..n].iter().all(|&v| v == b'n'));
}

#[test]
fn open_missing_without_create_returns_not_found() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/nope").unwrap();
    let r = fs.open(path, OpenOptions::new().read(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(littlefs2_pure::Error::NotFound));
}

#[test]
fn open_inline_without_truncate_rejected() {
    // A small inline-stored file (≤ INLINE_MAX = 128) cannot be
    // opened through File without truncate; the path-based API is
    // the right tool. Verify the typed rejection.
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/small").unwrap();
    fs.write_to_path(path, &[b's'; 32], &mut a, &mut b).unwrap();
    let r = fs.open(path, OpenOptions::new().read(true), &mut a, &mut b);
    assert_eq!(r.err(), Some(littlefs2_pure::Error::OutOfRange));
}

#[test]
fn write_at_non_eof_rejected() {
    // The File handle is append-only at the streaming level. A write
    // with cursor != size returns OutOfRange rather than silently
    // corrupting.
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/append-only").unwrap();
    fs.write_to_path(path, &[b'i'; 200], &mut a, &mut b).unwrap();

    let mut file =
        fs.open(path, OpenOptions::new().read(true).write(true), &mut a, &mut b).unwrap();
    file.seek(SeekFrom::Start(50)).unwrap();
    let r = file.write(&[b'x'; 10], &mut a, &mut b);
    assert_eq!(r.err(), Some(littlefs2_pure::Error::OutOfRange));
    // Drop without sync (file was never marked dirty).
}

#[test]
fn append_mode_forces_writes_to_eof() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/forced-append").unwrap();
    fs.write_to_path(path, &[b'i'; 200], &mut a, &mut b).unwrap();

    {
        let mut file =
            fs.open(path, OpenOptions::new().write(true).append(true), &mut a, &mut b).unwrap();
        // Even if we seek mid-file, append mode forces writes to EOF.
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write(&[b'a'; 50], &mut a, &mut b).unwrap();
        file.close(&mut a, &mut b).unwrap();
    }

    let mut out = vec![0u8; 400];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 250);
    assert!(out[..200].iter().all(|&v| v == b'i'));
    assert!(out[200..n].iter().all(|&v| v == b'a'));
}

#[test]
fn write_session_survives_remount() {
    let mut fs = fresh_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    let path = Path::new("/durable").unwrap();
    {
        let mut file = fs
            .open(path, OpenOptions::new().write(true).create(true).append(true), &mut a, &mut b)
            .unwrap();
        for _ in 0..8 {
            file.write(&[b'D'; 64], &mut a, &mut b).unwrap();
        }
        file.close(&mut a, &mut b).unwrap();
    }
    let storage = fs.into_storage();
    let mut fs = Fs::mount(storage, &mut a, &mut b).unwrap();

    let mut out = vec![0u8; 1024];
    let n = fs.read_at_path(path, 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 512);
    assert!(out[..n].iter().all(|&v| v == b'D'));
}

// Pull in core_alloc::vec under the standard `alloc` namespace name
// for the few test functions above.
extern crate alloc;
