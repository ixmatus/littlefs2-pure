# ADR-0019: the storage geometry preconditions are a compile time gate on mount and format

- **Status**: accepted; **implemented** (backlog item `lfs-cw1`, design
  observation D9 of the 2026-06 deep adversarial review).
- **Date**: 2026-07-28

## Context

The kernel takes its geometry from the associated constants on the
[`Storage`] trait and computes with them. It rounds a metadata commit up
to the next `PROG_SIZE` boundary and needs the result to still fit the
block. It stages a ragged read through the `READ_SIZE` grid and needs a
block sized buffer to cover a whole number of grid windows. It subtracts
a CTZ skip pointer header from `BLOCK_SIZE` to get a block's content
capacity.

That last one is the sharp edge. The widest skip pointer header is
`4 * (ctz(0x8000_0000) + 1) = 128` bytes, so `content_bytes_in_block`
underflows on any device advertising fewer than 128 bytes per block. A
debug build aborts on the subtraction. A release build with
`overflow-checks = false`, which is this crate's release profile, wraps
to 4294967232 and hands that number to a caller that sizes a read from
it. The C reference asserts the identical floor on the first line of
mount (`tools/gen_vectors/littlefs/lfs.c:4189`, under the comment "check
that the block size is large enough to fit all ctz pointers"), so an
image formatted below the floor is also an image the oracle refuses to
mount.

Until this decision the floor was documented, proven, and unenforced.
The Kani harness `ctz_content_bytes_in_block_underflows_below_the_floor`
proved the floor exactly tight and recorded, in its own doc comment,
that "today nothing in the crate enforces the floor: no mount path
checks `S::BLOCK_SIZE >= 128`, so the guarantee rests on the storage
adapter being sane." Measured at the 6c09615 baseline: `Fs::format` and
`Fs::mount` both returned `Ok` on a 64 byte block device, in debug and
in release.

The review logged the shape of the fix as design observation D9:
"post monomorphization const assertions in `Fs::mount`/`format` paths;
misuse becomes a compile error, zero runtime cost. 1.x compatible."

## Decision

**Every geometry precondition the kernel's arithmetic depends on is
stated once in `src/geometry.rs`, and `Fs::mount` and `Fs::format`
enforce it at compile time by naming `Geometry::<S>::CHECK`.**

The predicate is `geometry::fault_for`, a `const fn` over four loose
values, wrapped by `geometry::fault::<S>()`, which supplies a `Storage`
impl's constants. Two gates read that one predicate:

- `Geometry::<S>::CHECK` is a `const` that panics during const
  evaluation when the predicate fails. Naming it inside a generic
  function makes the failure a post monomorphization error (E0080) at
  the call site that instantiated it, with the violated relation as the
  message. This is the gate that fires for every caller. `Fs::mount` is
  the only constructor of an `Fs`, so the whole handle surface inherits
  the check from those two functions.
- `geometry::validate::<S>()` is the same predicate at runtime,
  reporting `Error::GeometryMismatch`. `Fs::mount` and `Fs::format` call
  it too, ahead of any arithmetic that could wrap.

`ctz::read_ctz_at` is the one public reader that computes a per block
content capacity from a raw `Storage` without a mount; it gets the
runtime form of the floor check rather than the const form, so it stays
a total function over any geometry a caller hands it.

The enforced relations are exactly those the kernel computes with:
`READ_SIZE != 0`, `PROG_SIZE != 0`, `PROG_SIZE % READ_SIZE == 0`,
`BLOCK_SIZE >= 128`, `BLOCK_SIZE % PROG_SIZE == 0`,
`BLOCK_SIZE <= u32::MAX`, `BLOCK_COUNT >= 2`.

## Consequences

**Wins.** A device the kernel cannot compute with no longer produces a
filesystem; it produces a build error naming the relation. The class of
bug that motivated the work (silent wrap in release, abort in debug, an
image the C reference will not mount) becomes unrepresentable rather
than merely undocumented. The runtime cost is zero: for a type that
compiles, `validate` folds to `Ok(())` and vanishes. Downstream code can
assert its own adapter once, away from any call site, with
`const _: () = Geometry::<MyFlash>::CHECK;`. The gate closes the
assumption the CTZ Kani harnesses make: `geometry_proofs` proves no sub
floor block size is admitted and that every admitted geometry makes the
capacity subtraction total, so `ctz_proofs`'s `assume(block_size >= 128)`
is now discharged rather than trusted.

**Costs.** A post monomorphization error is not a pretty error. It
points at the const inside this crate and names the instantiating call
site in a `note:`, rather than pointing at the offending `Storage` impl.

It also arrives at code generation rather than type check, which has a
measured consequence: `cargo build`, `cargo test`, and the
`compile_fail` doctests report it, while `cargo check` and
`cargo clippy` compile the same program without complaint, because
neither generates code for the instantiation. A bad geometry is still
caught before anything runs, but not by the fastest command in the
loop. A downstream user who wants the check at `cargo check` speed
writes `const _: () = Geometry::<MyFlash>::CHECK;` next to their
adapter, which is a plain const item rather than a post monomorphization
one. Moving the gate into the type system proper (a sealed
`ConformingStorage` supertrait, or a `Geometry` witness argument) would
fix this, and would also change the public signature of `mount` and
`format`; that is a 2.0 shape, not a 1.x one.

The gate cannot be exercised end to end from a test: a test that called
`Fs::mount` on a 64 byte device would not compile, which is the point.
The compile failure is pinned instead by `compile_fail,E0080` doctests
on `Geometry::CHECK` (which `cargo test --doc` runs, paired with a
positive control at exactly 128 bytes so the doctest cannot pass for an
unrelated compile error), and the runtime predicate is exercised
directly in `tests/geometry_floor.rs` and the unit tests in
`src/geometry.rs`.

Tightening an unenforced contract can break a downstream build that
compiled before. That is intended here: the geometries newly rejected
are exactly the geometries whose arithmetic was already wrong.

**Explicitly out of scope.** Three relations the C reference asserts
stay unenforced, deliberately:

- The cache relations (`cache_size % read_size == 0`,
  `cache_size % prog_size == 0`, `block_size % cache_size == 0`).
  `CACHE_SIZE` is advisory in this release; no kernel path reads it.
  Rejecting a device over a constant nothing consumes would break
  working adapters for no safety gain. They become enforceable the day
  an internal cache lands.
- `LOOKAHEAD_SIZE` sanity, for the same reason.
- `BLOCK_CYCLES != 0`. This crate defines `<= 0` as wear levelling
  disabled, so zero is meaningful here and rejecting it would contradict
  the crate's own contract. The doc versus code divergence on that
  constant ("negative disables" versus `<= 0`) remains an open question
  from the review.

Also out of scope: the power of two claim the `Storage` trait docs make
about `READ_SIZE`, `PROG_SIZE`, and `CACHE_SIZE`. The C reference does
not require it and no arithmetic in this crate depends on it
(`read_range` grids on any nonzero unit), so the trait text is a
description of real NOR flash rather than a precondition. The trait docs
now say so.

Finally, this gate says nothing about whether a *conforming* geometry is
a *good* one. A 128 byte block filesystem is legal and tested here, and
is still a poor idea for anything but a test.

## Related

- Backlog item `lfs-cw1`; design observation D9 in
  `docs/reviews/2026-06-10-deep-adversarial-review.md`.
- `src/geometry.rs` (the predicate and both gates), `src/fs.rs`
  (`Fs::mount`, `Fs::format`), `src/ctz.rs` (`read_ctz_at`),
  `src/storage.rs` (the trait's invariant list).
- `src/verify/geometry_proofs.rs` (the gate's own harnesses) and
  `src/verify/ctz_proofs.rs` (the floor harnesses whose assumption this
  discharges).
- `tests/geometry_floor.rs`.
- `docs/references/c-littlefs-oracle.md` for the vendored oracle pin the
  `lfs.c` line numbers resolve against.
