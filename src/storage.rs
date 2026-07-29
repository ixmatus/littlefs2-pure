//! The hardware boundary.
//!
//! Everything above the [`Storage`] trait is generic in `S: Storage` and
//! knows nothing about flash chips, memory mapped files, or `embedded-hal`
//! versions. Host code that wants to back a filesystem with an OS file wraps
//! the file in a `Storage` impl; that wrapper is the only place that needs
//! `std`.
//!
//! The associated constants advertise the geometry. The methods read,
//! program, and erase aligned regions and report I/O failures through the
//! associated [`Storage::Error`] type. Misalignment is a precondition
//! violation, not an error return: callers in the kernel always align before
//! calling, and the trait may panic or return an unspecified error on
//! misaligned input. Embedded callers should treat misalignment as a bug.
//!
//! [`read_range`] is the kernel's helper for the other half of that
//! bargain: it reads an arbitrary byte range out of a block while
//! issuing only reads that sit on the `READ_SIZE` grid. Every kernel
//! read of an on disk structure whose extent is not naturally aligned
//! (CTZ skip pointers, CTZ block content) goes through it.

use core::fmt::Debug;
use core::result::Result;

use crate::error::Error;

/// The hardware abstraction for a LittleFS backing store.
///
/// All offsets and sizes are in bytes. The block coordinate `(block, off)`
/// addresses byte `block * BLOCK_SIZE + off`.
///
/// # Out-of-range addresses
///
/// An implementation **must** return [`Self::Error`] (never read or
/// write out-of-bounds memory, never panic) for any
/// [`read`](Self::read), [`program`](Self::program), or
/// [`erase`](Self::erase) whose `block` is `>= BLOCK_COUNT` or whose
/// `(block, off, len)` extent runs past the device. The kernel aligns
/// and range-checks every call it originates, but block addresses
/// decoded from on-disk structures (directory pair pointers, CTZ skip
/// pointers, tail links) can be arbitrary in a corrupt or adversarial
/// image. The kernel defensively rejects out-of-range pair addresses
/// and CTZ skip pointers before dereferencing them, classifying such
/// an address as [`crate::Error::Corrupt`], but it still relies on
/// this trait contract as the final backstop: an implementation that
/// indexes a backing buffer without its own bounds check turns a
/// malformed image into memory unsafety in the adapter. The reference
/// [`crate::NorAlignedStorage`] and all test adapters honor this.
///
/// # Geometry invariants
///
/// - `READ_SIZE`, `PROG_SIZE`, and `CACHE_SIZE` are powers of two.
/// - `BLOCK_SIZE` is a positive multiple of `PROG_SIZE` (and therefore of
///   `READ_SIZE`).
/// - `BLOCK_SIZE` is at least
///   [`geometry::BLOCK_SIZE_MIN`](crate::geometry::BLOCK_SIZE_MIN), the
///   128 bytes a CTZ skip pointer header can occupy.
/// - Every call to [`read`](Self::read), [`program`](Self::program), and
///   [`erase`](Self::erase) operates on regions aligned to and sized as a
///   multiple of the respective unit.
///
/// [`crate::geometry`] states which of these the crate enforces and
/// which it only documents. The enforced subset is checked at compile
/// time on the way into [`crate::Fs::mount`] and [`crate::Fs::format`];
/// the power of two claim is a description of real NOR flash rather
/// than a precondition, and no code path depends on it.
pub trait Storage {
    /// The error type for the underlying device.
    type Error: Debug;

    /// Minimum read granularity, in bytes. Typical NOR flash: 1, 16, or 256.
    const READ_SIZE: usize;

    /// Minimum program granularity, in bytes. Typical NOR flash: 1, 16, or
    /// 256. Must be at least as large as `READ_SIZE` per the LittleFS spec.
    const PROG_SIZE: usize;

    /// Size of one erase block, in bytes. Typical NOR flash: 4 KiB.
    const BLOCK_SIZE: usize;

    /// Total number of erase blocks on the device.
    const BLOCK_COUNT: u32;

    /// Wear leveling rotation interval, in number of metadata commits before
    /// rotating across the metadata pair. `500` is the C reference default.
    /// Set to a negative value to disable wear leveling.
    const BLOCK_CYCLES: i32 = 500;

    /// Working cache size in bytes, used by both metadata and file
    /// operations. Must be a multiple of `PROG_SIZE` and a factor of
    /// `BLOCK_SIZE`.
    ///
    /// **Advisory in this release.** The kernel currently passes a
    /// caller-provided pair of `BLOCK_SIZE`-byte buffers to every
    /// metadata operation (`buf_a`, `buf_b`); there is no internal
    /// `CACHE_SIZE`-sized scratch. The constant is exposed so storage
    /// adapters mirror the LittleFS spec, and so a future internal
    /// cache (forward-looking) can honor caller-provided sizing without a
    /// breaking change.
    const CACHE_SIZE: usize;

    /// Lookahead buffer size in bytes for the block allocator. Each bit
    /// tracks one block; the buffer is rotated as the filesystem walks for
    /// free blocks. Must be a multiple of 8.
    ///
    /// **Advisory in this release.** The kernel's allocator
    /// ([`crate::alloc::alloc_blocks`]) currently does a full BFS scan
    /// of the filesystem on every call, with a stack-allocated 4096-bit
    /// (512-byte) bitmap internal to the function. There is no
    /// caller-visible lookahead buffer. The constant is exposed so a
    /// streaming-lookahead allocator (forward-looking) can honor caller
    /// sizing without a breaking change.
    const LOOKAHEAD_SIZE: usize;

    /// Read `buf.len()` bytes starting at `(block, off)` into `buf`.
    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Program `data.len()` bytes into the region starting at `(block, off)`.
    /// The target region must have been erased since the last program of the
    /// same bytes; rewriting without an intervening erase is undefined.
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), Self::Error>;

    /// Erase one block. After erase the block reads as all `0xFF` bytes.
    fn erase(&mut self, block: u32) -> Result<(), Self::Error>;

    /// Flush any internal caches in the backing store. The kernel calls this
    /// after a commit, before claiming the commit is durable.
    fn sync(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Read an arbitrary byte range from one block, issuing only reads that
/// satisfy the [`Storage`] alignment precondition.
///
/// On disk structures are not laid out on the read grid: a CTZ skip
/// pointer is four bytes at offset `4*k`, a chain block's content starts
/// just past its pointer header, and a caller may ask for any file
/// offset and length. Handing those extents straight to
/// [`Storage::read`] violates the contract, which requires a
/// `READ_SIZE` aligned offset and a `READ_SIZE` multiple of bytes. A
/// tolerant adapter (any RAM backed test double) accepts them anyway,
/// which is exactly why the violation survives in code that is only
/// ever exercised against such an adapter; a device that enforces the
/// grid faults instead.
///
/// This helper is the kernel's read through window. The range it is
/// asked for is covered by a run of grid windows; the helper fetches
/// each of those windows exactly once, never a byte of the device more
/// than once, and never a window that holds no requested byte. Three
/// cases serve that, in order of preference:
///
/// - **Already on the grid.** A range that starts on a boundary and
///   spans a whole number of windows is read straight into `out`: one
///   device read, no copy, `window` untouched. This is what a full
///   block read costs, before and after this helper existed.
/// - **One shot.** Otherwise, when `window` can hold the covering run
///   (the kernel's block sized scratch always can), the run is fetched
///   in a single device read and the requested bytes are copied out.
///   Still one read, whatever the misalignment. This is the case the
///   flash operation count cares about, since a device read costs a
///   command and an address before it costs a byte.
/// - **Fragments.** Otherwise (a window sized at the bare `READ_SIZE`
///   minimum, or a range longer than the window), the leading and
///   trailing partial windows are staged through `window` and the
///   aligned interior between them is read straight into `out`. At
///   most three reads, whatever the range's length.
///
/// `window` is scratch owned by the caller; it must hold at least
/// `S::READ_SIZE` bytes (any block sized buffer qualifies, since
/// `BLOCK_SIZE` is a multiple of `READ_SIZE`). Its contents after the
/// call are unspecified. Nothing is cached across calls, so the helper
/// cannot serve a stale byte after an intervening program or erase.
///
/// Reads never run past the end of the block: every read covers grid
/// windows holding a byte the caller asked for, and `BLOCK_SIZE` is a
/// multiple of `READ_SIZE`.
///
/// # Errors
///
/// - [`Error::GeometryMismatch`] if `window` is shorter than
///   `S::READ_SIZE`, or the geometry reports a zero `READ_SIZE`.
/// - [`Error::Io`] if the device rejects a read.
pub fn read_range<S: Storage>(
    storage: &mut S,
    block: u32,
    off: u32,
    out: &mut [u8],
    window: &mut [u8],
) -> Result<(), Error> {
    let unit = S::READ_SIZE;
    if unit == 0 || window.len() < unit {
        return Err(Error::GeometryMismatch);
    }
    if out.is_empty() {
        return Ok(());
    }
    let total = out.len();
    let mut pos = off as usize;
    let mut done = 0usize;
    let skew = pos % unit;

    // Already on the grid: the destination is itself a legal read
    // target, so nothing is staged and nothing is copied.
    if skew == 0 && total % unit == 0 {
        return storage.read(block, pos as u32, out).map_err(|_| Error::Io);
    }

    // One shot: the run of grid windows covering the whole range fits
    // in the staging buffer, so a single read serves it. The checked
    // arithmetic keeps the rounding total: a range so large that its
    // end cannot be rounded up fits no window anyway, and falls
    // through to the fragment path.
    let covering = pos
        .checked_add(total)
        .and_then(|end| end.checked_next_multiple_of(unit))
        .map(|top| top - (pos - skew));
    if let Some(covering) = covering.filter(|c| *c <= window.len()) {
        storage.read(block, (pos - skew) as u32, &mut window[..covering]).map_err(|_| Error::Io)?;
        out.copy_from_slice(&window[skew..skew + total]);
        return Ok(());
    }

    // Leading fragment: the grid window that contains `off`.
    if skew != 0 {
        storage.read(block, (pos - skew) as u32, &mut window[..unit]).map_err(|_| Error::Io)?;
        let take = (unit - skew).min(total);
        out[..take].copy_from_slice(&window[skew..skew + take]);
        done = take;
        pos += take;
    }

    // Aligned interior, straight into the destination.
    let whole = (total - done) / unit * unit;
    if whole != 0 {
        storage.read(block, pos as u32, &mut out[done..done + whole]).map_err(|_| Error::Io)?;
        done += whole;
        pos += whole;
    }

    // Trailing fragment: fewer than `unit` bytes left, so one more
    // grid window covers them.
    if done < total {
        storage.read(block, pos as u32, &mut window[..unit]).map_err(|_| Error::Io)?;
        out[done..].copy_from_slice(&window[..total - done]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Block size for the exhaustive sweep. Small enough that every
    /// `(offset, length)` extent of every geometry can be enumerated.
    const BLOCK: usize = 64;

    /// A device that enforces the read grid and counts device reads,
    /// parameterized by its `READ_SIZE`.
    struct Grid<const UNIT: usize> {
        data: [u8; BLOCK],
        reads: usize,
    }

    impl<const UNIT: usize> Storage for Grid<UNIT> {
        type Error = ();
        const READ_SIZE: usize = UNIT;
        const PROG_SIZE: usize = UNIT;
        const BLOCK_SIZE: usize = BLOCK;
        const BLOCK_COUNT: u32 = 1;
        const CACHE_SIZE: usize = BLOCK;
        const LOOKAHEAD_SIZE: usize = 8;

        fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), ()> {
            assert_eq!(block, 0);
            assert_eq!(off as usize % UNIT, 0, "offset {off} off the grid of {UNIT}");
            assert_eq!(buf.len() % UNIT, 0, "length {} off the grid of {UNIT}", buf.len());
            let end = off as usize + buf.len();
            assert!(end <= BLOCK, "read of {} bytes at {off} runs past the block", buf.len());
            self.reads += 1;
            buf.copy_from_slice(&self.data[off as usize..end]);
            Ok(())
        }

        fn program(&mut self, _block: u32, _off: u32, _data: &[u8]) -> Result<(), ()> {
            Err(())
        }

        fn erase(&mut self, _block: u32) -> Result<(), ()> {
            Err(())
        }
    }

    fn pattern() -> [u8; BLOCK] {
        let mut data = [0u8; BLOCK];
        let mut i = 0;
        while i < BLOCK {
            data[i] = (i * 7 + 3) as u8;
            i += 1;
        }
        data
    }

    /// Every extent of every geometry, checked for content and for the
    /// read count ceiling, under both staging strategies:
    /// `window_len == BLOCK` takes the one shot path, `window_len ==
    /// UNIT` (the minimum the contract allows) takes the fragment path
    /// for any range wider than a single grid window.
    fn sweep<const UNIT: usize>(window_len: usize) {
        let data = pattern();
        for off in 0..BLOCK {
            for len in 0..=(BLOCK - off) {
                let mut device = Grid::<UNIT> { data, reads: 0 };
                let mut out = [0u8; BLOCK];
                let mut window = [0u8; BLOCK];
                read_range(&mut device, 0, off as u32, &mut out[..len], &mut window[..window_len])
                    .unwrap();
                assert_eq!(
                    &out[..len],
                    &data[off..off + len],
                    "unit {UNIT} window {window_len}: wrong bytes at offset {off} length {len}"
                );
                let ceiling = if window_len >= BLOCK { 1 } else { 3 };
                let floor = usize::from(len > 0);
                assert!(
                    device.reads <= ceiling && device.reads >= floor,
                    "unit {UNIT} window {window_len}: offset {off} length {len} took {} reads",
                    device.reads
                );
            }
        }
    }

    #[test]
    fn read_range_reassembles_every_extent_from_aligned_reads() {
        sweep::<1>(BLOCK);
        sweep::<2>(BLOCK);
        sweep::<4>(BLOCK);
        sweep::<8>(BLOCK);
        sweep::<16>(BLOCK);
        sweep::<32>(BLOCK);
        sweep::<64>(BLOCK);
    }

    #[test]
    fn read_range_reassembles_every_extent_with_a_minimal_window() {
        sweep::<1>(1);
        sweep::<2>(2);
        sweep::<4>(4);
        sweep::<8>(8);
        sweep::<16>(16);
        sweep::<32>(32);
        sweep::<64>(64);
    }

    #[test]
    fn read_range_reads_nothing_for_an_empty_destination() {
        let mut device = Grid::<16> { data: pattern(), reads: 0 };
        let mut window = [0u8; BLOCK];
        read_range(&mut device, 0, 7, &mut [], &mut window).unwrap();
        assert_eq!(device.reads, 0);
    }

    #[test]
    fn read_range_rejects_a_window_below_the_read_unit() {
        let mut device = Grid::<16> { data: pattern(), reads: 0 };
        let mut window = [0u8; 8];
        let mut out = [0u8; 4];
        assert_eq!(
            read_range(&mut device, 0, 0, &mut out, &mut window).unwrap_err(),
            Error::GeometryMismatch
        );
        assert_eq!(device.reads, 0);
    }

    /// A fully aligned bulk read still costs exactly one device read, so
    /// the window path does not tax the common case.
    #[test]
    fn read_range_keeps_the_aligned_bulk_read_single() {
        let mut device = Grid::<16> { data: pattern(), reads: 0 };
        let mut window = [0u8; BLOCK];
        let mut out = [0u8; BLOCK];
        read_range(&mut device, 0, 0, &mut out, &mut window).unwrap();
        assert_eq!(device.reads, 1);
        assert_eq!(out, pattern());
    }
}
