//! Block coordinates.
//!
//! A LittleFS image is addressed in fixed size erase blocks. Each block has a
//! `u32` index. Metadata is stored in *pairs* of blocks: the active block and
//! its alternate, between which the filesystem rotates for wear leveling. A
//! [`BlockPair`] is the address of that two block window.

use core::fmt;

/// A single erase block index.
///
/// Zero is a valid block address: `(0, 1)` is the root metadata pair.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct BlockAddress(pub u32);

impl BlockAddress {
    /// Construct a block address from a raw `u32`.
    #[inline]
    pub const fn new(addr: u32) -> Self {
        Self(addr)
    }

    /// The all ones sentinel used by the C reference to mark "no block here"
    /// in linked structures. Equal to `u32::MAX`.
    pub const NONE: Self = Self(u32::MAX);

    /// Returns `true` if this address is the `NONE` sentinel.
    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == u32::MAX
    }

    /// The underlying `u32`.
    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for BlockAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            f.write_str("BlockAddress(NONE)")
        } else {
            write!(f, "BlockAddress({})", self.0)
        }
    }
}

impl fmt::Display for BlockAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            f.write_str("none")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// A metadata pair: two block addresses, between which the filesystem
/// rotates. The order is not significant on disk; revision counters decide
/// which is active.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockPair {
    /// First block of the pair.
    pub a: BlockAddress,
    /// Second block of the pair.
    pub b: BlockAddress,
}

impl BlockPair {
    /// Construct a pair from two addresses. The order does not have to be
    /// sorted; on disk the active block is identified by its revision
    /// counter, not by position.
    #[inline]
    pub const fn new(a: BlockAddress, b: BlockAddress) -> Self {
        Self { a, b }
    }

    /// Returns the two addresses as a tuple, sorted ascending. Useful for
    /// hashing or comparing pairs whose physical order on disk may differ.
    #[inline]
    pub fn sorted(self) -> (BlockAddress, BlockAddress) {
        if self.a <= self.b {
            (self.a, self.b)
        } else {
            (self.b, self.a)
        }
    }
}
