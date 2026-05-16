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

use core::fmt::Debug;
use core::result::Result;

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
/// before dereferencing them, but it relies on this trait contract as
/// the final backstop: an implementation that indexes a backing buffer
/// without its own bounds check turns a malformed image into memory
/// unsafety in the adapter. The reference [`crate::NorAlignedStorage`]
/// and all test adapters honor this.
///
/// # Geometry invariants
///
/// - `READ_SIZE`, `PROG_SIZE`, and `CACHE_SIZE` are powers of two.
/// - `BLOCK_SIZE` is a positive multiple of `PROG_SIZE` (and therefore of
///   `READ_SIZE`).
/// - Every call to [`read`](Self::read), [`program`](Self::program), and
///   [`erase`](Self::erase) operates on regions aligned to and sized as a
///   multiple of the respective unit.
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
