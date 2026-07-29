//! Preconditions on a [`Storage`] implementation's geometry.
//!
//! The kernel takes its geometry from the associated constants on the
//! [`Storage`] trait and computes with them. It rounds a metadata commit
//! up to the next `PROG_SIZE` boundary and requires the result to still
//! fit the block. It stages a ragged read through the `READ_SIZE` grid
//! and requires a block sized buffer to cover a whole number of grid
//! windows. It subtracts a CTZ skip pointer header from `BLOCK_SIZE` to
//! get a block's content capacity. Each step assumes a relation among
//! the constants, and each relation is stated once here.
//!
//! A violated relation does not merely give a wrong answer. The widest
//! CTZ skip pointer header is 128 bytes, so
//! [`content_bytes_in_block`](crate::ctz::content_bytes_in_block)
//! underflows on a device advertising fewer than 128 bytes per block:
//! a debug build aborts on the subtraction, and a release build with
//! `overflow-checks = false` wraps to nearly `u32::MAX`, whereupon the
//! caller computes a read extent from that number. The Kani harness
//! `ctz_content_bytes_in_block_underflows_below_the_floor` proves the
//! floor is exactly 128 and that every value below it faults.
//!
//! The C reference asserts the same family of relations on the first
//! line of mount and aborts the program when one fails
//! (`tools/gen_vectors/littlefs/lfs.c:4176` through `4192`, the
//! vendored oracle at the pinned upstream revision):
//!
//! ```text
//! // check that the block size is large enough to fit all ctz pointers
//! LFS_ASSERT(lfs->cfg->block_size >= 128);
//! ```
//!
//! An image this crate formats onto a device below that floor is
//! therefore an image the C reference refuses to mount, so the floor is
//! an interoperability obligation as well as an arithmetic one.
//!
//! # Which gate fires
//!
//! Every constant in the predicate is an associated `const`, so the
//! whole predicate is known at compile time and the honest place to
//! enforce it is the compiler.
//!
//! - [`Geometry::CHECK`] evaluates the predicate in a `const`.
//!   [`Fs::mount`](crate::Fs::mount) and [`Fs::format`](crate::Fs::format)
//!   name that const, so a device whose geometry the kernel cannot
//!   compute with fails to compile the moment it reaches either entry
//!   point. **This is the gate that fires for every caller.** A
//!   filesystem handle exists only by way of `Fs::mount`, so the check
//!   covers the whole [`Fs`](crate::Fs) surface, not just the two
//!   functions that name it.
//! - [`validate`] is the same predicate evaluated at runtime, reporting
//!   [`Error::GeometryMismatch`]. `Fs::mount` and `Fs::format` call it
//!   too, ahead of any arithmetic that could wrap. For a type that
//!   compiles, the branch is constant folded away and never taken; it
//!   stays because a backstop whose whole cost is a folded constant is
//!   cheap insurance against the const reference being dropped in a
//!   later edit, and because a caller may want to probe a geometry
//!   without a compile error.
//!
//! Downstream code can assert its own device once, away from any call
//! site, with `const _: () = Geometry::<MyFlash>::CHECK;`. Written that
//! way the check is a plain const item rather than a post
//! monomorphization one, so it also fires under `cargo check`.
//!
//! The gate inside a generic function does not: a post monomorphization
//! const error is raised when the constant is required for code
//! generation, so `cargo build`, `cargo test`, and `cargo doc` on the
//! `compile_fail` examples report it, while `cargo check` and
//! `cargo clippy` (which stop at metadata) do not. A bad geometry is
//! therefore caught before anything runs, but not necessarily by the
//! fastest command in the loop.
//!
//! # What is deliberately not enforced
//!
//! [`Storage::CACHE_SIZE`] and [`Storage::LOOKAHEAD_SIZE`] are advisory
//! in this release: no kernel path reads either constant (their trait
//! docs say so). Rejecting a device over a constant nothing consumes
//! would break working downstream adapters for no gain, so the C
//! reference's cache relations (`cache_size % read_size == 0`,
//! `cache_size % prog_size == 0`, `block_size % cache_size == 0`) go
//! unchecked here. They become enforceable the day an internal cache
//! lands, and that is the day to add them.
//!
//! [`Storage::BLOCK_CYCLES`] is likewise unchecked. The C reference
//! asserts `block_cycles != 0`; this crate defines `<= 0` as "wear
//! levelling disabled" (see [`crate::Fs`]), so zero is meaningful here
//! and rejecting it would contradict the crate's own documented
//! contract.
//!
//! Nothing checks that `READ_SIZE`, `PROG_SIZE`, and `CACHE_SIZE` are
//! powers of two, although the [`Storage`] trait docs describe them
//! that way. The C reference does not require it either, and no
//! arithmetic in this crate depends on it: [`crate::storage::read_range`]
//! grids on any nonzero unit. The trait text records the shape of real
//! NOR flash, not a precondition, and enforcing it would reject a
//! conforming device.

use core::marker::PhantomData;

use crate::error::Error;
use crate::storage::Storage;

/// The smallest `BLOCK_SIZE` a LittleFS device may advertise, in bytes.
///
/// A CTZ chain block at index `i > 0` carries `ctz(i) + 1` skip
/// pointers of four bytes each at its head; the rest of the block is
/// file content. The widest header belongs to index `0x8000_0000`,
/// whose 31 trailing zeros give 32 pointers, so `4 * (31 + 1) = 128`
/// bytes. A block smaller than that cannot hold the header of a block
/// index the format can address, and the content capacity subtraction
/// underflows. The C reference asserts the identical floor at mount
/// (`lfs.c:4189`).
pub const BLOCK_SIZE_MIN: usize = 128;

/// The smallest `BLOCK_COUNT` a mountable device may advertise.
///
/// The root metadata pair is fixed at blocks 0 and 1
/// ([`crate::ROOT_BLOCK_PAIR`]), so a device with fewer than two blocks
/// has nowhere to put a filesystem at all.
pub const BLOCK_COUNT_MIN: u32 = 2;

/// The geometry precondition a [`Storage`] implementation violates.
///
/// Returned by [`fault`], one variant per relation, in the order
/// [`fault`] checks them. A device satisfying every relation yields
/// `None` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeometryFault {
    /// `READ_SIZE` is zero. Every read grid computation divides by it.
    ZeroReadSize,
    /// `PROG_SIZE` is zero. Commit padding rounds up to it.
    ZeroProgSize,
    /// `PROG_SIZE` is not a whole number of `READ_SIZE` units, so a
    /// commit boundary need not land on the read grid and the read
    /// back that verifies a freshly programmed commit would issue a
    /// misaligned read.
    ProgSizeOffTheReadGrid,
    /// `BLOCK_SIZE` is below [`BLOCK_SIZE_MIN`], so a CTZ skip pointer
    /// header can be wider than the block that holds it.
    BlockSizeBelowFloor,
    /// `BLOCK_SIZE` is not a whole number of `PROG_SIZE` units, so a
    /// commit padded up to the program grid can run past the end of
    /// its block.
    BlockSizeOffTheProgGrid,
    /// `BLOCK_SIZE` does not fit a `u32`, the width the on disk
    /// superblock stores it in, so formatting would record a truncated
    /// geometry.
    BlockSizePastTheWordCeiling,
    /// `BLOCK_COUNT` is below [`BLOCK_COUNT_MIN`], so the fixed root
    /// metadata pair does not fit on the device.
    BlockCountBelowRootPair,
}

impl GeometryFault {
    /// A short description, for a caller that logs the fault rather
    /// than matching on it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZeroReadSize => "Storage::READ_SIZE is zero",
            Self::ZeroProgSize => "Storage::PROG_SIZE is zero",
            Self::ProgSizeOffTheReadGrid => {
                "Storage::PROG_SIZE is not a multiple of Storage::READ_SIZE"
            }
            Self::BlockSizeBelowFloor => "Storage::BLOCK_SIZE is below the 128 byte floor",
            Self::BlockSizeOffTheProgGrid => {
                "Storage::BLOCK_SIZE is not a multiple of Storage::PROG_SIZE"
            }
            Self::BlockSizePastTheWordCeiling => "Storage::BLOCK_SIZE does not fit a u32",
            Self::BlockCountBelowRootPair => "Storage::BLOCK_COUNT is below 2",
        }
    }
}

/// The first geometry precondition `S` violates, or `None` when `S` is
/// usable.
///
/// This is the single statement of the predicate; [`validate`] and
/// [`Geometry::CHECK`] both read it, so the runtime gate and the
/// compile time gate cannot drift apart.
///
/// The relations, in the order checked:
///
/// 1. `READ_SIZE != 0`
/// 2. `PROG_SIZE != 0`
/// 3. `PROG_SIZE % READ_SIZE == 0`
/// 4. `BLOCK_SIZE >= BLOCK_SIZE_MIN`
/// 5. `BLOCK_SIZE % PROG_SIZE == 0`
/// 6. `BLOCK_SIZE <= u32::MAX`
/// 7. `BLOCK_COUNT >= BLOCK_COUNT_MIN`
///
/// Relations 3 and 5 together give `BLOCK_SIZE % READ_SIZE == 0`, the
/// form [`crate::storage::read_range`] relies on to keep every staged
/// read inside the block.
#[must_use]
pub const fn fault<S: Storage>() -> Option<GeometryFault> {
    fault_for(S::READ_SIZE, S::PROG_SIZE, S::BLOCK_SIZE, S::BLOCK_COUNT)
}

/// [`fault`] over four loose values rather than a type's associated
/// constants.
///
/// The predicate lives here so it can be reasoned about at the value
/// level: `src/verify/geometry_proofs.rs` runs this function on
/// symbolic inputs, which is impossible through the type parameter
/// form. [`fault`] is the one line that binds a `Storage` impl to it,
/// so there is still a single statement of the relations.
#[must_use]
pub const fn fault_for(
    read_size: usize,
    prog_size: usize,
    block_size: usize,
    block_count: u32,
) -> Option<GeometryFault> {
    if read_size == 0 {
        return Some(GeometryFault::ZeroReadSize);
    }
    if prog_size == 0 {
        return Some(GeometryFault::ZeroProgSize);
    }
    if prog_size % read_size != 0 {
        return Some(GeometryFault::ProgSizeOffTheReadGrid);
    }
    if block_size < BLOCK_SIZE_MIN {
        return Some(GeometryFault::BlockSizeBelowFloor);
    }
    if block_size % prog_size != 0 {
        return Some(GeometryFault::BlockSizeOffTheProgGrid);
    }
    if block_size > u32::MAX as usize {
        return Some(GeometryFault::BlockSizePastTheWordCeiling);
    }
    if block_count < BLOCK_COUNT_MIN {
        return Some(GeometryFault::BlockCountBelowRootPair);
    }
    None
}

/// The runtime form of [`fault`]: `Ok(())` when `S` is usable,
/// [`Error::GeometryMismatch`] when it is not.
///
/// [`crate::Fs::mount`] and [`crate::Fs::format`] call this before any
/// arithmetic that could wrap on a bad geometry. Call it directly to
/// probe a candidate device without a compile error; use
/// [`fault`] instead when the reason matters.
pub const fn validate<S: Storage>() -> Result<(), Error> {
    match fault::<S>() {
        None => Ok(()),
        Some(_) => Err(Error::GeometryMismatch),
    }
}

/// Compile time gate on a [`Storage`] implementation's geometry.
///
/// The type carries no data and is never constructed; it exists to hang
/// [`Geometry::CHECK`] on a generic parameter.
#[derive(Debug)]
pub struct Geometry<S: Storage> {
    _device: PhantomData<S>,
}

impl<S: Storage> Geometry<S> {
    /// Evaluates to `()` when `S` satisfies every geometry
    /// precondition, and fails to compile when it does not.
    ///
    /// The failure is a post monomorphization const evaluation error:
    /// it appears when some code path instantiates the const for a
    /// concrete `S`, and it names the violated relation in the panic
    /// message. [`crate::Fs::mount`] and [`crate::Fs::format`] name the
    /// const, so any program that mounts or formats a device gets the
    /// check for free.
    ///
    /// A 128 byte device compiles:
    ///
    /// ```no_run
    /// use littlefs2_pure::{Fs, Storage};
    ///
    /// struct Flash;
    /// impl Storage for Flash {
    ///     type Error = ();
    ///     const READ_SIZE: usize = 16;
    ///     const PROG_SIZE: usize = 16;
    ///     const BLOCK_SIZE: usize = 128;
    ///     const BLOCK_COUNT: u32 = 16;
    ///     const CACHE_SIZE: usize = 64;
    ///     const LOOKAHEAD_SIZE: usize = 8;
    ///     fn read(&mut self, _b: u32, _o: u32, _buf: &mut [u8]) -> Result<(), ()> { Ok(()) }
    ///     fn program(&mut self, _b: u32, _o: u32, _d: &[u8]) -> Result<(), ()> { Ok(()) }
    ///     fn erase(&mut self, _b: u32) -> Result<(), ()> { Ok(()) }
    /// }
    ///
    /// let mut scratch = [0u8; 128];
    /// Fs::format(&mut Flash, &mut scratch).unwrap();
    /// ```
    ///
    /// The same program with a 64 byte block does not, and the error is
    /// the const evaluation failure (E0080) rather than a mount time
    /// surprise:
    ///
    /// ```compile_fail,E0080
    /// use littlefs2_pure::{Fs, Storage};
    ///
    /// struct Flash;
    /// impl Storage for Flash {
    ///     type Error = ();
    ///     const READ_SIZE: usize = 16;
    ///     const PROG_SIZE: usize = 16;
    ///     const BLOCK_SIZE: usize = 64; // below the 128 byte floor
    ///     const BLOCK_COUNT: u32 = 16;
    ///     const CACHE_SIZE: usize = 64;
    ///     const LOOKAHEAD_SIZE: usize = 8;
    ///     fn read(&mut self, _b: u32, _o: u32, _buf: &mut [u8]) -> Result<(), ()> { Ok(()) }
    ///     fn program(&mut self, _b: u32, _o: u32, _d: &[u8]) -> Result<(), ()> { Ok(()) }
    ///     fn erase(&mut self, _b: u32) -> Result<(), ()> { Ok(()) }
    /// }
    ///
    /// let mut scratch = [0u8; 64];
    /// Fs::format(&mut Flash, &mut scratch).unwrap();
    /// ```
    ///
    /// Mounting is gated identically:
    ///
    /// ```compile_fail,E0080
    /// use littlefs2_pure::{Fs, Storage};
    ///
    /// struct Flash;
    /// impl Storage for Flash {
    ///     type Error = ();
    ///     const READ_SIZE: usize = 16;
    ///     const PROG_SIZE: usize = 16;
    ///     const BLOCK_SIZE: usize = 64; // below the 128 byte floor
    ///     const BLOCK_COUNT: u32 = 16;
    ///     const CACHE_SIZE: usize = 64;
    ///     const LOOKAHEAD_SIZE: usize = 8;
    ///     fn read(&mut self, _b: u32, _o: u32, _buf: &mut [u8]) -> Result<(), ()> { Ok(()) }
    ///     fn program(&mut self, _b: u32, _o: u32, _d: &[u8]) -> Result<(), ()> { Ok(()) }
    ///     fn erase(&mut self, _b: u32) -> Result<(), ()> { Ok(()) }
    /// }
    ///
    /// let mut a = [0u8; 64];
    /// let mut b = [0u8; 64];
    /// Fs::mount(Flash, &mut a, &mut b).unwrap();
    /// ```
    pub const CHECK: () = match fault::<S>() {
        None => (),
        Some(GeometryFault::ZeroReadSize) => panic!("Storage::READ_SIZE is zero"),
        Some(GeometryFault::ZeroProgSize) => panic!("Storage::PROG_SIZE is zero"),
        Some(GeometryFault::ProgSizeOffTheReadGrid) => {
            panic!("Storage::PROG_SIZE is not a multiple of Storage::READ_SIZE")
        }
        Some(GeometryFault::BlockSizeBelowFloor) => panic!(
            "Storage::BLOCK_SIZE is below the 128 byte LittleFS floor: a CTZ skip pointer \
             header is up to 4 * 32 = 128 bytes and would not fit the block"
        ),
        Some(GeometryFault::BlockSizeOffTheProgGrid) => {
            panic!("Storage::BLOCK_SIZE is not a multiple of Storage::PROG_SIZE")
        }
        Some(GeometryFault::BlockSizePastTheWordCeiling) => {
            panic!("Storage::BLOCK_SIZE does not fit a u32")
        }
        Some(GeometryFault::BlockCountBelowRootPair) => {
            panic!("Storage::BLOCK_COUNT is below 2: the root metadata pair needs blocks 0 and 1")
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A geometry double whose every constant is a generic parameter,
    /// so one impl covers the whole fault table. Only the constants
    /// matter; the I/O methods are never called.
    struct Dev<const READ: usize, const PROG: usize, const BLOCK: usize, const COUNT: u32>;

    impl<const READ: usize, const PROG: usize, const BLOCK: usize, const COUNT: u32> Storage
        for Dev<READ, PROG, BLOCK, COUNT>
    {
        type Error = ();
        const READ_SIZE: usize = READ;
        const PROG_SIZE: usize = PROG;
        const BLOCK_SIZE: usize = BLOCK;
        const BLOCK_COUNT: u32 = COUNT;
        const CACHE_SIZE: usize = BLOCK;
        const LOOKAHEAD_SIZE: usize = 8;

        fn read(&mut self, _block: u32, _off: u32, _buf: &mut [u8]) -> Result<(), ()> {
            Err(())
        }

        fn program(&mut self, _block: u32, _off: u32, _data: &[u8]) -> Result<(), ()> {
            Err(())
        }

        fn erase(&mut self, _block: u32) -> Result<(), ()> {
            Err(())
        }
    }

    #[test]
    fn a_conforming_geometry_has_no_fault() {
        assert_eq!(fault::<Dev<16, 16, 128, 2>>(), None);
        assert_eq!(fault::<Dev<1, 1, 128, 2>>(), None);
        assert_eq!(fault::<Dev<16, 256, 4096, 1024>>(), None);
        assert_eq!(fault::<Dev<1, 16, 512, 64>>(), None);
        assert!(validate::<Dev<16, 16, 256, 8>>().is_ok());
    }

    #[test]
    fn every_relation_has_a_witness() {
        assert_eq!(fault::<Dev<0, 16, 256, 8>>(), Some(GeometryFault::ZeroReadSize));
        assert_eq!(fault::<Dev<16, 0, 256, 8>>(), Some(GeometryFault::ZeroProgSize));
        assert_eq!(fault::<Dev<16, 24, 256, 8>>(), Some(GeometryFault::ProgSizeOffTheReadGrid));
        assert_eq!(fault::<Dev<16, 16, 64, 8>>(), Some(GeometryFault::BlockSizeBelowFloor));
        assert_eq!(fault::<Dev<16, 16, 200, 8>>(), Some(GeometryFault::BlockSizeOffTheProgGrid));
        assert_eq!(fault::<Dev<16, 16, 256, 1>>(), Some(GeometryFault::BlockCountBelowRootPair));
    }

    /// The floor is inclusive: 128 passes, 127 does not. 127 is not a
    /// multiple of the program size either, so the pair below uses a
    /// program size of 1 to isolate the floor from the grid relation.
    #[test]
    fn the_floor_is_inclusive_and_tight() {
        assert_eq!(fault::<Dev<1, 1, 128, 8>>(), None);
        assert_eq!(fault::<Dev<1, 1, 127, 8>>(), Some(GeometryFault::BlockSizeBelowFloor));
    }

    /// Every fault maps to the same runtime error; the variant is for
    /// the caller that wants the reason.
    #[test]
    fn validate_reports_geometry_mismatch_for_every_fault() {
        assert_eq!(validate::<Dev<0, 16, 256, 8>>(), Err(Error::GeometryMismatch));
        assert_eq!(validate::<Dev<16, 16, 64, 8>>(), Err(Error::GeometryMismatch));
        assert_eq!(validate::<Dev<16, 16, 200, 8>>(), Err(Error::GeometryMismatch));
        assert_eq!(validate::<Dev<16, 16, 256, 1>>(), Err(Error::GeometryMismatch));
    }

    /// The description table stays in step with the variant list.
    #[test]
    fn every_fault_describes_itself() {
        for f in [
            GeometryFault::ZeroReadSize,
            GeometryFault::ZeroProgSize,
            GeometryFault::ProgSizeOffTheReadGrid,
            GeometryFault::BlockSizeBelowFloor,
            GeometryFault::BlockSizeOffTheProgGrid,
            GeometryFault::BlockSizePastTheWordCeiling,
            GeometryFault::BlockCountBelowRootPair,
        ] {
            assert!(!f.as_str().is_empty());
        }
    }
}
