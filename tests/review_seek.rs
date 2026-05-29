//! Oracle test for `ctz::seek_block` (`lfs-o72`): the O(log n) skip-list
//! descent must return exactly the same physical block address as the
//! O(n) backward walk `collect_chain_blocks` for every index of a real
//! chain. `collect_chain_blocks` is the trusted oracle (it backs the
//! read path and the conformance vectors).

use littlefs2_pure::ctz::{block_count, collect_chain_blocks, seek_block, MAX_CTZ_BLOCKS};
use littlefs2_pure::{BlockAddress, Fs, OpenOptions, Path, Storage};

struct Big {
    data: Vec<u8>,
}

impl Big {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 512;
    fn new() -> Self {
        Self { data: vec![0xFFu8; Self::BLOCK_SIZE * Self::BLOCK_COUNT as usize] }
    }
}

impl Storage for Big {
    type Error = ();
    const READ_SIZE: usize = 16;
    const PROG_SIZE: usize = 16;
    const BLOCK_SIZE: usize = Self::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = Self::BLOCK_COUNT;
    const CACHE_SIZE: usize = 64;
    const LOOKAHEAD_SIZE: usize = 8;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(buf.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), ()> {
        let start = (block as usize) * Self::BLOCK_SIZE + off as usize;
        let end = start.checked_add(data.len()).ok_or(())?;
        if block >= Self::BLOCK_COUNT || end > self.data.len() {
            return Err(());
        }
        self.data[start..end].copy_from_slice(data);
        Ok(())
    }
    fn erase(&mut self, block: u32) -> Result<(), ()> {
        if block >= Self::BLOCK_COUNT {
            return Err(());
        }
        let start = (block as usize) * Self::BLOCK_SIZE;
        self.data[start..start + Self::BLOCK_SIZE].fill(0xFF);
        Ok(())
    }
}

#[test]
fn seek_block_matches_collect_chain_blocks_for_every_index() {
    // Build a long CTZ file through the real write path, then recover the
    // raw storage to drive the low-level chain functions directly.
    let mut storage = Big::new();
    let mut sb = [0u8; Big::BLOCK_SIZE];
    Fs::format(&mut storage, &mut sb).unwrap();

    let (head, size) = {
        let mut ba = [0u8; Big::BLOCK_SIZE];
        let mut bb = [0u8; Big::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
        let mut a = [0u8; Big::BLOCK_SIZE];
        let mut b = [0u8; Big::BLOCK_SIZE];

        // ~200 blocks: append in chunks through one File handle.
        let chunk = [0xC7u8; 240];
        {
            let opts = OpenOptions::new().write(true).append(true).create(true);
            let mut f = fs.open(Path::new("/big").unwrap(), opts, &mut a, &mut b).unwrap();
            for _ in 0..200 {
                f.write(&chunk, &mut a, &mut b).unwrap();
            }
            f.close(&mut a, &mut b).unwrap();
        }
        let r = fs.resolve(Path::new("/big").unwrap(), &mut a, &mut b).unwrap();
        // struct_body of a CtzStruct is (head_le, size_le).
        let body = r.struct_body;
        let head = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let size = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        storage = fs.into_storage();
        (head, size)
    };

    let n = block_count(size, Big::BLOCK_SIZE as u32);
    assert!(n as usize >= 100 && (n as usize) <= MAX_CTZ_BLOCKS, "chain length {n}");

    // Oracle: the full backward walk.
    let mut chain = [BlockAddress::NONE; MAX_CTZ_BLOCKS];
    collect_chain_blocks(&mut storage, BlockAddress::new(head), n, &mut chain[..n as usize])
        .unwrap();

    // Every index must seek to exactly the oracle's address.
    for target in 0..n {
        let got = seek_block(&mut storage, BlockAddress::new(head), n - 1, target).unwrap();
        assert_eq!(
            got, chain[target as usize],
            "seek to index {target} (chain length {n}) disagreed with collect_chain_blocks",
        );
    }

    // Degenerate: seeking to the head index returns the head.
    assert_eq!(
        seek_block(&mut storage, BlockAddress::new(head), n - 1, n - 1).unwrap().as_u32(),
        head
    );
}
