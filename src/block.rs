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

    /// `true` when `self` and `other` name the same two physical blocks,
    /// whichever order each lists them in.
    ///
    /// Order carries no meaning on disk. A pair's active half is the block
    /// with the higher revision counter, not the block listed first, so
    /// `{2, 3}` and `{3, 2}` address one metadata pair and read back one
    /// committed state. The C reference says the same thing twice over: its
    /// only pair equality primitive is `lfs_pair_issync`, which accepts
    /// either order, and `lfs_dir_fetchmatch` re-sorts every fetched pair by
    /// revision before anything else consumes it.
    ///
    /// Derived `PartialEq` on [`BlockPair`] compares the ordered `(a, b)`
    /// tuple and is therefore the wrong test for identity of an address
    /// decoded from disk. Any visited set, dedup key, or reachability
    /// membership test over such addresses must use this instead: an image
    /// that names one pair under both orders otherwise walks it twice, and a
    /// walk that XOR-folds per-pair state cancels that pair's contribution
    /// to zero (review L7, `lfs-a8j`).
    #[inline]
    pub fn is_same_pair(self, other: Self) -> bool {
        self.sorted() == other.sorted()
    }

    /// `true` when this pair is the all ones "no pair here" sentinel.
    ///
    /// The C reference spells this `lfs_pair_isnull` (`lfs.c:292`) and
    /// deliberately tests the two halves with OR, not AND: a pair with
    /// either half equal to `LFS_BLOCK_NULL` is null. Half of a pair
    /// address is never independently meaningful, so a partially all ones
    /// body is malformed rather than half valid, and the C reader folds
    /// both cases into the same answer. This predicate matches that
    /// definition exactly so the two readers agree on every input.
    ///
    /// In a *tail* body the sentinel means "the thread ends here"; see
    /// [`crate::meta::MetadataReader::tail`]. In a `DirStruct` body it
    /// means nothing at all: the C writer never emits one, so the kernel
    /// treats an all ones `DirStruct` as an out of range address and
    /// rejects it like any other.
    #[inline]
    pub const fn is_null(self) -> bool {
        self.a.is_none() || self.b.is_none()
    }
}

/// `true` when `pairs` already holds `pair` as a physical block set.
///
/// The order-insensitive counterpart of `slice::contains` for pair
/// addresses decoded from disk. See [`BlockPair::is_same_pair`] for why
/// the derived equality is the wrong test at these sites.
///
/// Internal to the kernel's walkers: it exists to keep every visited-set
/// membership test in one shape, not as a downstream utility.
#[inline]
pub(crate) fn contains_pair(pairs: &[BlockPair], pair: BlockPair) -> bool {
    pairs.iter().any(|p| p.is_same_pair(pair))
}
