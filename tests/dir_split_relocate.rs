//! Wear-levelling relocation of a HardTail continuation pair (`lfs-cvh.6`).
//!
//! A split directory's continuation pair is reached only via the preceding
//! pair's HardTail; no parent holds a `DirStruct` pointing at it. When such
//! a pair relocates (wear-levelling), `propagate_relocation` must re-point
//! its *thread predecessor's* HardTail, not a parent `DirStruct`. Before
//! `lfs-cvh.6` that path returned `Error::Corrupt`.
//!
//! Reproduce-first: build a multi-pair `/d`, then churn updates against an
//! entry that lives in a continuation pair, with `BLOCK_CYCLES = 1` so a
//! relocation fires every few compactions. Every operation must succeed and
//! the filesystem must stay consistent and remountable.

use littlefs2_pure::{Fs, Path, Storage};

struct Dev {
    data: Vec<u8>,
}
impl Dev {
    const BLOCK_SIZE: usize = 256;
    const BLOCK_COUNT: u32 = 48;
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
    // Relocate every `((1 + 1) | 1) = 3` compactions, so a continuation
    // pair that is churned will migrate quickly.
    const BLOCK_CYCLES: i32 = 1;
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
fn continuation_pair_relocation_is_handled() {
    let mut storage = Dev::new();
    let mut sb = buf();
    Fs::format(&mut storage, &mut sb).unwrap();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    let mut a = buf();
    let mut b = buf();
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    // Build a directory spanning several pairs so later entries live in
    // continuation pairs (no DirStruct parent).
    const N: u32 = 24;
    for i in 0..N {
        let name = format!("/d/f{i:02}");
        fs.write_to_path(Path::new(&name).unwrap(), b"x", &mut a, &mut b).unwrap();
    }

    // Churn updates against entries that live deep in the chain. With
    // BLOCK_CYCLES = 1 their owning continuation pairs compact and relocate
    // repeatedly. Each write must succeed (no Corrupt from an unhandled
    // continuation relocation) and the value must round-trip.
    for round in 0..40u32 {
        for &i in &[N - 1, N - 3, N - 5, N - 7, N - 9] {
            let name = format!("/d/f{i:02}");
            let payload = [b'a' + (round % 26) as u8];
            fs.write_to_path(Path::new(&name).unwrap(), &payload, &mut a, &mut b)
                .unwrap_or_else(|e| panic!("round {round} update f{i:02} failed: {e:?}"));
            let mut out = [0u8; 1];
            let n =
                fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
            assert_eq!((n, out[0]), (1, payload[0]), "round {round} f{i:02} round-trip");
        }
    }

    // Every original entry still enumerates and reads back.
    let count_entries = |fs: &mut Fs<Dev>, a: &mut [u8], b: &mut [u8]| -> usize {
        let mut seen = 0usize;
        fs.list_dir(Path::new("/d").unwrap(), |_e| seen += 1, a, b).unwrap();
        seen
    };
    assert_eq!(count_entries(&mut fs, &mut a, &mut b), N as usize, "all entries survive churn");

    // A fresh remount sees the same consistent directory (gstate balanced;
    // no relocation left half-applied).
    let storage = fs.into_storage();
    let mut ba = buf();
    let mut bb = buf();
    let mut fs = Fs::mount(storage, &mut ba, &mut bb).unwrap();
    assert_eq!(count_entries(&mut fs, &mut a, &mut b), N as usize, "entries survive remount");
    for i in 0..N {
        let name = format!("/d/f{i:02}");
        let mut out = [0u8; 1];
        let n = fs.read_at_path(Path::new(&name).unwrap(), 0, &mut out, &mut a, &mut b).unwrap();
        assert_eq!(n, 1, "f{i:02} present after remount");
    }
}
