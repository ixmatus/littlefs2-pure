//! Reproduce-first target for `lfs-xmx` / investigation `lfs-l3f`:
//! global directory-list threading via SoftTail.
//!
//! The C reference threads every metadata pair into a filesystem-wide
//! linked list via tail pointers (a `SoftTail` to the next directory in
//! the list, a `HardTail` to a directory's own continuation). The crate's
//! writer emits no tail tag at all: it finds pairs by a parent->child
//! `DirStruct` BFS, which is self-consistent for crate-only use but means
//! crate-created images lack the global list the C reference's
//! `lfs_fs_traverse` / `lfs_fs_gc` / allocator rely on.
//!
//! This pins the write-side gap with a Rust-level assertion. The
//! C-interop severity (does C miss un-threaded subdir blocks when it
//! allocates or traverses?) is the separate `lfs-l3f` investigation,
//! which extends the C `verify_image` harness; see the bead.
//!
//! `directories_are_threaded_into_the_global_list` is the target,
//! `#[ignore]`d until SoftTail threading lands (lfs-xmx).

use littlefs2_pure::meta::MetadataReader;
use littlefs2_pure::{Fs, Path, Storage};

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

#[test]
fn directories_are_threaded_into_the_global_list() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    {
        let mut ba = buf();
        let mut bb = buf();
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = buf();
        let mut b = buf();
        fs.mkdir(Path::new("/a").unwrap(), &mut a, &mut b).unwrap();
        fs.mkdir(Path::new("/a/b").unwrap(), &mut a, &mut b).unwrap();
        storage = fs.into_storage();
    }

    // Walk the global metadata-pair list from the root by following tail
    // pointers, collecting every pair it threads through. With threading,
    // the two subdirectories are reachable this way (not only via the
    // parent->child DirStruct tree).
    let bs = Dev::BLOCK_SIZE;
    let mut pairs_via_tail = 0usize;
    let mut cur = Some(0u32); // root active block
    let mut guard = 0;
    while let Some(block) = cur {
        guard += 1;
        assert!(guard < 64, "tail walk did not terminate");
        let r =
            MetadataReader::new(&storage.data[(block as usize) * bs..(block as usize + 1) * bs])
                .unwrap();
        match r.tail() {
            Some(next) => {
                pairs_via_tail += 1;
                cur = Some(next.a.as_u32());
            }
            None => cur = None,
        }
    }
    assert!(
        pairs_via_tail >= 2,
        "the global tail-threaded list must reach the subdirectories (found {pairs_via_tail} tail links)",
    );
}
