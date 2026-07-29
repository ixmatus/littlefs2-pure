# ADR-0020: read back verification reads the device, not the adapter's cache

- **Status**: accepted; implemented (`lfs-6ym`)
- **Date**: 2026-07-28

## Context

Review H2 gave the eight metadata commit sites a read back: program the
region, re read it, CRC compare, and treat a mismatch as a worn block.
`lfs-ttr` extended the same discipline to the five file data sites, and
`lfs-n23` and `lfs-i59` (ADR-0014) taught the remaining fresh block paths
to relocate on a mismatch instead of failing. The point of all of it is
one failure mode: a chip that accepts a program, reports success, and
does not hold what it was told.

`NorAlignedStorage` is the adapter real NOR needs. It buffers writes into
one `PROG_SIZE` window and splices that window into `read`, so a caller
sees its own not yet flushed bytes. That splice is load bearing: the
kernel reads back a commit it has not synced yet all over the write path.

The two collide. `Storage::program` on the adapter walks the span in
windows, flushing each window when the *next* one loads, so the final
window of any programmed region is still dirty in RAM when the kernel's
verify read arrives, and `read` answers it from RAM.

### The exposure map

Measured, not assumed. An instrumented device under
`NorAlignedStorage<Trace>` at `BLOCK_SIZE = 256`, `PROG_SIZE = 16` logged
every inner call. `P` is a device program, `R` a device read; the verify
read is the wide one.

`mkdir /d`, the pair init commit (32 byte commit on block 2):

```
E    blk 2
E    blk 3
  R  blk 2 off 0 len 16     load window 0
P    blk 2 off 0 len 16     window 0 flushed by the load of window 1
  R  blk 2 off 16 len 16    load window 1
  R  blk 2 off 0 len 32     the verify read
P    blk 2 off 16 len 16    window 1 reaches the device AFTER the verify
S    sync
```

The parent's `CreateDir` commit on block 0 (64 bytes at offset 64):

```
  R  blk 0 off 64 len 16 / P blk 0 off 64 len 16
  R  blk 0 off 80 len 16 / P blk 0 off 80 len 16
  R  blk 0 off 96 len 16 / P blk 0 off 96 len 16
  R  blk 0 off 112 len 16
  R  blk 0 off 64 len 64    the verify read
P    blk 0 off 112 len 16   after the verify
```

A CTZ chain block (`write_to_path` of 300 bytes onto block 4):

```
  ... windows 0 through 224 loaded and flushed ...
  R  blk 4 off 240 len 16
  R  blk 4 off 0 len 256    the verify read
P    blk 4 off 240 len 16   after the verify
```

The tail fill, `verify_programmed_bytes` (append of 20 bytes at content
offset 96 of block 6):

```
  R  blk 6 off 96 len 16 / P blk 6 off 96 len 16
  R  blk 6 off 112 len 16
  R  blk 6 off 96 len 32    the verify read
P    blk 6 off 112 len 16   after the verify
```

The shrink relocation of `lfs-i59` and `Fs::format`'s superblock verify
have the same shape. So the map is uniform rather than site specific:

**Every verify site ends its program in a dirty window, so every verify
had a blind tail of exactly the last `PROG_SIZE` bytes. A verified region
that fits in one window was blind end to end.**

Two findings sharpen it.

First, `sync` does not close the blind spot. `flush` clears `dirty` but
leaves the window resident, and `read` splices a resident window whether
or not it is dirty. Measured against a device that drops one bit on
every program:

```
read before sync : [a5, a5, a5, ... ]
read after  sync : [a5, a5, a5, ... ]
device truth     : [00, a5, a5, ... ]
```

Second, the consequences are not confined to metadata. With one worn
program page under the adapter (a physically ordinary NOR failure: one
page of a block stops holding charge), the pre change kernel produced:

- `write_to_path` of 300 bytes returning `Ok`, with the committed
  content's first wrong byte at offset 240, the blind window exactly.
  Silent corruption of the caller's data.
- `mkdir` returning `Ok`, with the directory pair's active half on the
  worn block, and `resolve("/d")` returning `Corrupt` at the next mount.
- `append_to_path` returning `Ok` where `lfs-ttr` site 4 specifies `Io`,
  because that site has no relocation left to take.

## Decision

The kernel's verify helpers read through a new defaulted `Storage` trait
method, `read_device`, which means "tell me what the device holds"; a
write buffering adapter overrides it to flush whatever pending bytes
overlap the request and then read through, and an adapter wrapping
another `Storage` forwards to the inner `read_device`.

`verify_programmed` and `verify_programmed_bytes` in `src/fs.rs` are the
only callers, so every one of the fourteen verify sites inherits the fix
at once. `Storage::read` is untouched: callers that need to see their own
pending bytes still do.

`NorAlignedStorage::read_device` flushes only when the pending window is
dirty *and* overlaps the requested region. A pending window on another
block, or one that does not overlap, is left buffered.

This is the C reference's shape, arrived at from the other side.
`lfs_bd_flush` validates by calling `lfs_cache_drop(lfs, rcache)` and
then `lfs_bd_cmp(lfs, NULL, rcache, ...)`: it drops the read cache and
passes a *null program cache*, so the validating comparison bypasses both
caches and reads the device. C puts the bypass in the validation caller,
exactly where this ADR puts it, and makes `validate` a per call site flag
rather than a property of the cache layer.

### Rejected alternatives

**(a) Sync before each verify.** Rejected on evidence: it does not work.
The measurement above shows `read` still splicing after `sync`, because
the window stays resident. Making it work would need a cache invalidation
as well, which costs a full extra device read of the window on every
verify, and it would still be the wrong shape: it asks fourteen kernel
sites to know they might be sitting behind a caching adapter. It also
converts a `flush` into an `inner.sync()` on the hot path, which on real
hardware is the expensive call.

**(b) The adapter validates every window against the device at flush
time.** The closest to `lfs_bd_flush` structurally, and it has a real
merit: the adapter would own device fidelity end to end, covering
programs the kernel does not verify at all. Rejected on three counts.
It cannot report the failure: `Storage::Error` is an implementor chosen
associated type with no constructor the adapter can call, so a window
that lands corrupted has no error value to return short of requiring
`Error: Default` (a breaking bound on every existing impl). It validates
every flush, including the many the kernel never asked about, adding a
device read per window on the hot path where the chosen design adds none.
And it moves the accept or reject verdict out of the kernel, which is
where the worn block policy (exclude, retry, bound at
`MAX_BAD_BLOCK_RETRIES`, relocate or report `Io` per site) lives; the
adapter has no way to express "this block is worn, pick another".

**(c, chosen) An additive raw read escape hatch.** Taken as a defaulted
trait method rather than an inherent `NorAlignedStorage` method, because
the verify helpers are generic in `S: Storage` and cannot name a concrete
adapter, and because third party adapters (a caching SPI driver, a
logging shim) need the same hook. Defaulting it to `read` keeps every
existing implementation compiling and behaving exactly as before.

### Crash window analysis

The only timing change is that an overlapping dirty window is programmed
at the verify instead of at the following window switch or `sync`. That
window was already the most recent write with nothing pending behind it,
so no device operation is reordered relative to any other: the sequence
of inner programs is identical, and only its position in wall clock
moves. The set of images a tear can leave is therefore unchanged.

What does change is which kernel code is running when a tear surfaces.
A tear that used to be reported by `sync` is now reported by the verify,
which returns `false` rather than propagating, so the site takes its worn
block path: allocate, erase, program, verify. Every one of those writes
is refused by a dead device (the tear injectors refuse `program` and
`erase` from the trigger onward), so the path exhausts its bound and
returns `Io` with nothing committed, the same verdict by a longer route.
The H7 and V4 sweeps confirm this empirically rather than by argument.

## Consequences

**Wins.**

- The read back verification actually verifies. A device that lies about
  a program is caught at program time on every site, behind the alignment
  adapter as well as in front of it, instead of at the next mount.
- Silent corruption of user file data behind the adapter is closed. That
  was the worst case: a `write` returning `Ok` over bytes the device did
  not keep.
- The invariant lands where it belongs. The kernel says what it needs
  ("device truth"), the adapter says how to provide it, and the worn block
  policy stays in the kernel.
- No write amplification, measured rather than argued. `read_device`
  issues the same device read the old path issued, and the flush it may
  trigger is a program that was already owed. A counting device under
  `NorAlignedStorage` running `mkdir`, a 300 byte CTZ write, a 20 byte
  append, a shrink to 100, and an inline write reported identical totals
  before and after the change: `programs=70 erases=7 syncs=10 reads=126`.
- Additive under semver: a defaulted trait method plus an adapter
  override. Existing `Storage` implementations compile and behave
  unchanged.

**Costs.**

- The `Storage` trait grows a fifth method, and adapters that wrap
  another `Storage` must forward it or silently reintroduce the blind
  spot. That obligation is documented on the method and honored by the
  two wrapper adapters in the test harness.
- A verify now flushes the last window slightly earlier than before,
  which a future performance measurement that counts *when* programs
  happen (rather than how many) would notice.
- Implementations with an internal read cache that this crate does not
  know about still answer `read_device` from the default `read`. The
  default is right for the common case (no write buffering) and wrong for
  a caching driver whose author does not read the doc comment.

**Explicitly out of scope.**

- Programs the kernel does not verify at all. This ADR makes the existing
  verify sites honest; it does not add sites, and it does not make the
  adapter validate on its own account (alternative (b)).
- Read path fidelity in general. `Storage::read` still splices, by
  design, and a caller wanting device truth outside a verify must ask for
  it explicitly.
- Detecting a device that corrupts *after* a successful read back (bit
  rot at rest). That is the mount time CRC's job, unchanged here.

## Related

- ADR-0014, failure driven pair relocation: the worn block policy this
  verification feeds.
- ADR-0017, the append tail fill's erased region check: the site whose
  verdict is `Io` rather than relocation.
- Review H2 (metadata read back), `lfs-ttr` (file data read back),
  `lfs-n23` and `lfs-i59` (fresh block retry at `mkdir` and the shrink
  relocation).
- C reference `lfs_bd_flush`, `lfs_bd_cmp`, `lfs_bd_prog`'s `validate`
  flag: `tools/gen_vectors/littlefs/lfs.c`, and the registry entry
  `docs/references/c-littlefs-oracle.md`.
- Post mortem: `docs/references/failure-nor-verify-splice.md`.
- Implementation: `src/storage.rs` (`Storage::read_device`), `src/nor.rs`
  (`NorAlignedStorage::read_device`), `src/fs.rs` (`verify_programmed`,
  `verify_programmed_bytes`).
- Tests: `tests/review_6ym_nor_verify_fidelity.rs`.
