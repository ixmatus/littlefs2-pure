//! Adapter that aligns programs to `PROG_SIZE` boundaries.
//!
//! Real NOR flash chips require programs to start at a `PROG_SIZE`
//! aligned address and to write a `PROG_SIZE` multiple. The kernel's
//! [`crate::Fs::write_inline_to_root`] emits programs at byte
//! granularity (a tag stream rarely aligns), so on actual hardware the
//! caller needs an adapter that buffers writes to the program-size
//! grid.
//!
//! [`NorAlignedStorage`] wraps another [`Storage`] implementation, caches
//! the most recently touched program-size window in a buffer, and only
//! flushes a complete window to the underlying device. Misaligned
//! programs are merged with the cached window; a sync or a touch of a
//! different window flushes.
//!
//! # Constraints
//!
//! - `S::PROG_SIZE` must divide `S::BLOCK_SIZE`. (Standard for any
//!   real flash chip; LittleFS already requires it.)
//! - The wrapper assumes the trailing bytes of any program window are
//!   in erased state (`0xFF`). This is true for any block since its
//!   last `erase`; programs do not return bits from 0 to 1 on NOR.
//! - The cached window must be flushed before erase (otherwise the
//!   pending bytes would be lost). The wrapper does this
//!   automatically.
//!
//! # Geometry pass-through
//!
//! The wrapper exposes the inner storage's `READ_SIZE`,
//! `BLOCK_SIZE`, `BLOCK_COUNT`, `CACHE_SIZE`, and
//! `LOOKAHEAD_SIZE` constants verbatim. `PROG_SIZE` is the inner
//! value (the alignment unit the wrapper enforces). The wrapper does
//! not change the device geometry; it only intermediates writes.

use crate::storage::Storage;

/// Maximum supported program-window size. The cache buffer is
/// stack-allocated as `[u8; MAX_PROG_SIZE]`. Typical NOR flash uses 16
/// or 256 bytes; 512 is generous.
pub const MAX_PROG_SIZE: usize = 512;

/// Adapter wrapping a [`Storage`] implementation to enforce
/// program-size aligned writes.
///
/// Construct with [`NorAlignedStorage::new`]. The wrapper has no
/// allocations; it owns a fixed-size cache buffer and the inner
/// device.
///
/// **Use the wrapper, not the inner device, as the [`Storage`]
/// implementation passed to [`crate::Fs`].** Otherwise the underlying
/// device may reject misaligned programs.
pub struct NorAlignedStorage<S: Storage> {
    inner: S,
    /// The current cached program window's block address (if any).
    cached_block: Option<u32>,
    /// The current cached program window's start offset within the
    /// block. Always a multiple of `S::PROG_SIZE`.
    cached_off: u32,
    /// The cached bytes for the current window.
    cache: [u8; MAX_PROG_SIZE],
    /// Whether the cache has been modified since it was loaded.
    dirty: bool,
}

impl<S: Storage> NorAlignedStorage<S> {
    /// Wrap `inner`. Asserts that `S::PROG_SIZE <= MAX_PROG_SIZE` and
    /// that `PROG_SIZE` divides `BLOCK_SIZE`; mounting will fail
    /// loudly if these don't hold.
    ///
    /// Returns `None` if those invariants are violated.
    pub fn new(inner: S) -> Option<Self> {
        if S::PROG_SIZE == 0 || S::PROG_SIZE > MAX_PROG_SIZE {
            return None;
        }
        if S::BLOCK_SIZE % S::PROG_SIZE != 0 {
            return None;
        }
        Some(Self {
            inner,
            cached_block: None,
            cached_off: 0,
            cache: [0xFFu8; MAX_PROG_SIZE],
            dirty: false,
        })
    }

    /// Consume the wrapper and return the inner storage (flushing any
    /// pending cache first).
    pub fn into_inner(mut self) -> Result<S, S::Error> {
        self.flush()?;
        Ok(self.inner)
    }

    /// Borrow the inner storage. Does not flush the cache; callers
    /// that need a consistent on-device view should `sync` first.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    fn flush(&mut self) -> Result<(), S::Error> {
        if !self.dirty {
            return Ok(());
        }
        let block = self.cached_block.expect("dirty cache must have a target");
        self.inner.program(block, self.cached_off, &self.cache[..S::PROG_SIZE])?;
        self.dirty = false;
        Ok(())
    }

    fn load_window(&mut self, block: u32, window_off: u32) -> Result<(), S::Error> {
        if self.cached_block == Some(block) && self.cached_off == window_off && !self.dirty {
            return Ok(()); // already loaded
        }
        if self.cached_block != Some(block) || self.cached_off != window_off {
            self.flush()?;
        }
        // Read the current window contents (will be 0xFF if never
        // programmed since erase, the typical case).
        self.inner.read(block, window_off, &mut self.cache[..S::PROG_SIZE])?;
        self.cached_block = Some(block);
        self.cached_off = window_off;
        self.dirty = false;
        Ok(())
    }
}

impl<S: Storage> Storage for NorAlignedStorage<S> {
    type Error = S::Error;
    const READ_SIZE: usize = S::READ_SIZE;
    const PROG_SIZE: usize = S::PROG_SIZE;
    const BLOCK_SIZE: usize = S::BLOCK_SIZE;
    const BLOCK_COUNT: u32 = S::BLOCK_COUNT;
    const BLOCK_CYCLES: i32 = S::BLOCK_CYCLES;
    const CACHE_SIZE: usize = S::CACHE_SIZE;
    const LOOKAHEAD_SIZE: usize = S::LOOKAHEAD_SIZE;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        // If the requested range overlaps the cached window, splice in
        // the cached bytes so reads see uncommitted writes.
        self.inner.read(block, off, buf)?;
        if let Some(cached_block) = self.cached_block {
            if cached_block == block {
                let cache_start = self.cached_off as u64;
                let cache_end = cache_start + S::PROG_SIZE as u64;
                let read_start = off as u64;
                let read_end = read_start + buf.len() as u64;
                let overlap_start = read_start.max(cache_start);
                let overlap_end = read_end.min(cache_end);
                if overlap_start < overlap_end {
                    let buf_lo = (overlap_start - read_start) as usize;
                    let buf_hi = (overlap_end - read_start) as usize;
                    let cache_lo = (overlap_start - cache_start) as usize;
                    let cache_hi = (overlap_end - cache_start) as usize;
                    buf[buf_lo..buf_hi].copy_from_slice(&self.cache[cache_lo..cache_hi]);
                }
            }
        }
        Ok(())
    }

    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), Self::Error> {
        // Walk the data span in program-size windows, loading each
        // window into the cache, merging the requested bytes, and
        // flushing on the next window's load (or sync).
        let prog_size = S::PROG_SIZE as u32;
        let mut written = 0usize;
        while written < data.len() {
            let cur_off = off + written as u32;
            let window_off = (cur_off / prog_size) * prog_size;
            let within = (cur_off - window_off) as usize;
            self.load_window(block, window_off)?;
            let remaining = S::PROG_SIZE - within;
            let take = remaining.min(data.len() - written);
            // NOR program semantic: only 1 -> 0 transitions are
            // permitted. Enforce via AND so any caller bug (write to
            // unerased region) corrupts the cache rather than panicking.
            for i in 0..take {
                self.cache[within + i] &= data[written + i];
            }
            self.dirty = true;
            written += take;
        }
        Ok(())
    }

    fn erase(&mut self, block: u32) -> Result<(), Self::Error> {
        // Drop any cached window targeting this block; its contents are
        // about to become 0xFF.
        if self.cached_block == Some(block) {
            self.cached_block = None;
            self.dirty = false;
        }
        self.inner.erase(block)
    }

    fn sync(&mut self) -> Result<(), Self::Error> {
        self.flush()?;
        self.inner.sync()
    }
}
