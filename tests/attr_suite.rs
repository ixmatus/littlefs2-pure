//! The attribute suite from the 2026-06 deep review's coverage debt
//! (V8): user attributes across HardTail continuations, rename in all
//! its shapes, and the rename-into-compaction path.
//!
//! Pins the read half of H5 (`get_attr` chases HardTail continuations,
//! landed with the C2 fix in ADR-0015) and H6 (cross-directory rename
//! preserves the moved entry's attributes, the C reference's
//! `LFS_FROM_MOVE` semantics). Compaction and splice attr coverage
//! lives in `tests/review_splice_attrs.rs`.

use littlefs2_pure::{Error, Fs, Path, Storage};

mod common;
use common::MemStorage;

/// 64-block variant of the in-RAM device for tests that need room for
/// directory splits and compaction headroom (MemStorage has 8 blocks).
struct Dev {
    data: Vec<u8>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 64;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}
impl Storage for Dev {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;
    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[s..e]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let s = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let e = s.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || e > self.data.len() {
            return Err(());
        }
        self.data[s..e].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let s = (block as usize) * Self::BLOCK_SIZE;
        self.data[s..s + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

fn buf() -> [u8; Dev::BLOCK_SIZE] {
    [0u8; Dev::BLOCK_SIZE]
}

fn make_dev_fs() -> Fs<Dev> {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    Fs::mount(storage, &mut ba, &mut bb).unwrap()
}

fn make_mem_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = common::make_buffer();
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = common::make_buffer();
    let mut buf_b = common::make_buffer();
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

fn get_attr_vec<S: Storage>(
    fs: &mut Fs<S>,
    path: &str,
    attr_id: u8,
    a: &mut [u8],
    b: &mut [u8],
) -> Vec<u8> {
    let mut out = [0u8; 64];
    let n = fs.get_attr(Path::new(path).unwrap(), attr_id, &mut out, a, b).unwrap();
    out[..n].to_vec()
}

#[test]
fn attr_on_hardtail_continuation_entry_roundtrips_and_persists() {
    // Review H5: an entry living in a split directory's continuation
    // pair must have readable attributes. 40 root entries force the
    // root across HardTail continuations; the highest-named entries
    // land in a continuation pair.
    let mut fs = make_dev_fs();
    let mut a = buf();
    let mut b = buf();
    for i in 0..40 {
        let name = format!("/f{i:03}");
        fs.write_to_path(Path::new(&name).unwrap(), b"v", &mut a, &mut b).unwrap();
    }

    fs.set_attr(Path::new("/f039").unwrap(), 7, b"deep", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/f000").unwrap(), 7, b"front", &mut a, &mut b).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/f039", 7, &mut a, &mut b), b"deep");
    assert_eq!(get_attr_vec(&mut fs, "/f000", 7, &mut a, &mut b), b"front");

    // Survives a remount (fresh chain walk from {0, 1}).
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/f039", 7, &mut a, &mut b), b"deep");
    assert_eq!(get_attr_vec(&mut fs, "/f000", 7, &mut a, &mut b), b"front");
}

#[test]
fn cross_dir_rename_preserves_attrs_inline() {
    // Review H6: the moved entry keeps its user attributes, atomically
    // with the move. Inline struct exercises the `Create` arm.
    let mut fs = make_mem_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/src/x").unwrap(), b"body", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/x").unwrap(), 1, b"alpha", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/x").unwrap(), 200, b"beta", &mut a, &mut b).unwrap();

    fs.rename(Path::new("/src/x").unwrap(), Path::new("/dst/y").unwrap(), &mut a, &mut b).unwrap();

    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 1, &mut a, &mut b), b"alpha");
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 200, &mut a, &mut b), b"beta");
    let err = fs.get_attr(Path::new("/src/x").unwrap(), 1, &mut [0u8; 8], &mut a, &mut b);
    assert_eq!(err.unwrap_err(), Error::NotFound);

    // And the content moved with it.
    let mut out = [0u8; 16];
    let n = fs.read_at_path(Path::new("/dst/y").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"body");

    // Persistence across remount.
    let storage = fs.into_storage();
    let mut ba = common::make_buffer();
    let mut bb = common::make_buffer();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 1, &mut a, &mut b), b"alpha");
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 200, &mut a, &mut b), b"beta");
}

#[test]
fn cross_dir_rename_preserves_attrs_ctz() {
    // CTZ struct exercises the `CreateCtz` arm; the chain itself does
    // not move, only the metadata entry.
    let mut fs = make_dev_fs();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    let content: Vec<u8> = (0..600).map(|i| (i % 251) as u8).collect();
    fs.write_to_path(Path::new("/src/big").unwrap(), &content, &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/big").unwrap(), 42, b"ctz-attr", &mut a, &mut b).unwrap();

    fs.rename(Path::new("/src/big").unwrap(), Path::new("/dst/big").unwrap(), &mut a, &mut b)
        .unwrap();

    assert_eq!(get_attr_vec(&mut fs, "/dst/big", 42, &mut a, &mut b), b"ctz-attr");
    let mut out = vec![0u8; 600];
    let n = fs.read_at_path(Path::new("/dst/big").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], &content[..]);
}

#[test]
fn cross_dir_rename_preserves_attrs_on_directory() {
    // Directory entry exercises the `CreateDir` arm; the directory's
    // own pair (and its content) stays where it is.
    let mut fs = make_dev_fs();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/src/d").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/src/d/inner").unwrap(), b"kept", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/d").unwrap(), 3, b"dir-attr", &mut a, &mut b).unwrap();

    fs.rename(Path::new("/src/d").unwrap(), Path::new("/dst/d2").unwrap(), &mut a, &mut b).unwrap();

    assert_eq!(get_attr_vec(&mut fs, "/dst/d2", 3, &mut a, &mut b), b"dir-attr");
    let mut out = [0u8; 8];
    let n =
        fs.read_at_path(Path::new("/dst/d2/inner").unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(&out[..n], b"kept");
}

#[test]
fn cross_dir_rename_into_compacting_destination_preserves_attrs() {
    // Fill the destination pair until the rename's Create cannot
    // append, so the moved attrs flow through the compaction emission
    // (`emit_compact_range`'s new-entry arm), not just `emit_op`.
    let mut fs = make_dev_fs();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/src/x").unwrap(), b"payload", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/x").unwrap(), 9, b"sticky", &mut a, &mut b).unwrap();

    // Churn the destination pair: repeated updates append until the
    // block fills; the eventual rename lands mid-log with little
    // append headroom on at least some iterations.
    for round in 0..6 {
        let body = [round as u8; 20];
        fs.write_to_path(Path::new("/dst/filler").unwrap(), &body, &mut a, &mut b).unwrap();
    }

    fs.rename(Path::new("/src/x").unwrap(), Path::new("/dst/y").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 9, &mut a, &mut b), b"sticky");

    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 9, &mut a, &mut b), b"sticky");
}

#[test]
fn in_dir_rename_keeps_attrs() {
    // Same-parent rename appends a NAME at the same id; attributes
    // must remain readable under the new name.
    let mut fs = make_mem_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.write_to_path(Path::new("/x").unwrap(), b"v", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/x").unwrap(), 5, b"stays", &mut a, &mut b).unwrap();
    fs.rename(Path::new("/x").unwrap(), Path::new("/renamed").unwrap(), &mut a, &mut b).unwrap();
    assert_eq!(get_attr_vec(&mut fs, "/renamed", 5, &mut a, &mut b), b"stays");
}

#[test]
fn removed_attr_does_not_resurrect_across_rename() {
    // A delete-marked attribute is dead; the move must not replay it.
    let mut fs = make_mem_fs();
    let mut a = common::make_buffer();
    let mut b = common::make_buffer();
    fs.mkdir(Path::new("/src").unwrap(), &mut a, &mut b).unwrap();
    fs.mkdir(Path::new("/dst").unwrap(), &mut a, &mut b).unwrap();
    fs.write_to_path(Path::new("/src/x").unwrap(), b"v", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/x").unwrap(), 1, b"doomed", &mut a, &mut b).unwrap();
    fs.set_attr(Path::new("/src/x").unwrap(), 2, b"kept", &mut a, &mut b).unwrap();
    fs.remove_attr(Path::new("/src/x").unwrap(), 1, &mut a, &mut b).unwrap();

    fs.rename(Path::new("/src/x").unwrap(), Path::new("/dst/y").unwrap(), &mut a, &mut b).unwrap();

    let mut out = [0u8; 16];
    let n = fs.get_attr(Path::new("/dst/y").unwrap(), 1, &mut out, &mut a, &mut b).unwrap();
    assert_eq!(n, 0, "delete-marked attr must not move");
    assert_eq!(get_attr_vec(&mut fs, "/dst/y", 2, &mut a, &mut b), b"kept");
}
