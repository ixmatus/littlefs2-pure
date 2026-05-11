# Integration guide

This file points downstream consumers (SMIL firmware, etc.) at the relevant entry points for each stage of integration. The full API rationale lives in module rustdoc; this is the working surface as of `main`.

## What works today

| Capability | API | Notes |
|---|---|---|
| Format a fresh device | `Fs::format(&mut storage, &mut scratch)` | One scratch buffer of `BLOCK_SIZE` |
| Mount an existing image | `Fs::mount(storage, &mut buf_a, &mut buf_b)` | Two scratch buffers of `BLOCK_SIZE`; completes any in-flight cross-directory rename via gstate recovery |
| Resolve an absolute path | `Fs::resolve(path, &mut buf_a, &mut buf_b)` | Returns `ResolvedPath { entry, struct_type, struct_body, pair }` |
| Check existence | `Fs::exists(path, ...)` | Wraps `resolve`, returns `bool` |
| Read inline file content | Use `resolved.struct_body` directly when `struct_type == InlineStruct` | Zero-copy slice into your buffer |
| Read CTZ file content | `Fs::read_ctz(&ctz_struct, &mut out, &mut scratch)` | After parsing `CtzStruct::from_bytes(resolved.struct_body)` |
| Write or update a small inline file | `Fs::write_inline_to_root(name, content, ...)` | Upsert semantics; appends if room, else compacts to the alternate |
| Write any-size file at root (auto-dispatch) | `Fs::write_to_root(name, content, ...)` | Picks inline (≤128 bytes) or CTZ |
| Write any-size file at arbitrary path | `Fs::write_to_path(path, content, ...)` | Parent directory must exist |
| Write a large file as CTZ | `Fs::write_ctz_to_root(name, content, ...)` | Allocates blocks and writes the skip-list chain |
| Create a directory | `Fs::mkdir(path, ...)` | Allocates a fresh metadata pair, writes empty initial commit |
| Remove a file at root | `Fs::remove_from_root(name, ...)` | Splice-correct; deleted entries no longer resolve |
| Append to a file (streaming for CTZ) | `Fs::append_to_path(path, additional, content_scratch, ...)` | Creates if missing; fills the tail in place + allocates only overflow blocks for CTZ files; `content_scratch` only consulted for inline / inline-to-CTZ transitions |
| Read at offset (inline + CTZ) | `Fs::read_at_path(path, offset, &mut out, ...)` | Returns bytes copied; works on any layout |
| File size | `Fs::size_of(path, ...)` | Returns byte length (inline or CTZ) |
| Tail-block free room | `Fs::tail_room(path, ...)` | For CTZ files: bytes that can be appended without allocating a new block. Returns `0` for inline. |
| Truncate / extend | `Fs::truncate_path(path, new_size, content_scratch, ...)` | Shrink drops trailing bytes; extend zero-pads |
| Remove a file at path | `Fs::remove_at_path(path, ...)` | Rejects directories; use `rmdir` instead |
| Remove empty directory | `Fs::rmdir(path, ...)` | Errors with `NotEmpty` if the dir has contents |
| Rename within same directory | `Fs::rename_in_dir(old_path, new_path, ...)` | Same-parent only; appends a NAME tag at the existing id |
| Rename across directories | `Fs::rename(old_path, new_path, ...)` | Same-parent fast path delegates to `rename_in_dir`; cross-parent is Create-in-dst then Delete-in-src with balanced gstate so a crash between commits is recovered on mount |
| List a directory | `Fs::list_dir(path, callback, ...)` | Splice-correct; chases HardTails through up to 32 continuation pairs |
| List root directory | `Fs::list_root(callback, ...)` | Splice-correct; chases HardTails through up to 32 continuation pairs |
| Read user attribute | `Fs::get_attr(path, attr_id, &mut out, ...)` | Returns latest committed value; `0` on absent or delete-marker |
| Write user attribute | `Fs::set_attr(path, attr_id, value, ...)` | Values capped at `0x3FE` bytes |
| Remove user attribute | `Fs::remove_attr(path, attr_id, ...)` | Emits a delete-marker tag |
| Stateful file handle | `Fs::open(path, OpenOptions, ...) -> File<'fs, S>` | Batches many writes into one metadata-pair commit at `File::sync` / `close`. CTZ-backed files only (opening an inline file without `truncate` is rejected with a typed error). `read`, `write`, `seek`, `set_len` mirror `std::fs::File` shape over `u32` offsets. |
| Sync the storage layer | `Fs::sync()` | Equivalent to `storage_mut().sync()`. Every mutation already syncs as its final step. |
| NOR-aligned program wrapper | `NorAlignedStorage::new(your_storage)` | Caches programs to `PROG_SIZE` windows |

The kernel uses the `Storage::BLOCK_CYCLES` constant for inter-pair wear distribution: every `((BLOCK_CYCLES + 1) | 1)` compactions on a non-root pair, the new commit is redirected to a freshly allocated block and the parent's `DirStruct` flips to the new pair address (one extra erase plus one extra program per cycle). Set `BLOCK_CYCLES = -1` on your `Storage` impl to disable wear levelling entirely; the C reference default of `500` is the trait default. See `docs/decisions/0005-wear-leveling-pair-relocation.md` for the design.

## What's not yet supported

| Capability | Tracking |
|---|---|
| Mount-time orphan recovery for half-completed wear-levelling relocations | Benign miss only — user data is durable as soon as the alternate is programmed (before the fresh block); a crash leaves the relocation half-done but the FS state is consistent and the next predicate firing resumes wear levelling. |
| HardTail-chain pair relocation | Structurally unreachable through this writer (we don't emit `HardTail` tags). |
| Kani-in-CI integration | Harnesses are ready under `src/verify/`; CI integration awaits hosted-runner Kani availability. |
| Random in-place writes through `File::write` (writes at cursor `!=` size) | Out of scope for the streaming handle. Rewrite the file via `Fs::write_to_path` or shrink with `File::set_len` first. |

## Suggested integration steps

1. **Smoke test the embedded build path.** Add `littlefs2-pure` as an optional dep of your firmware behind a feature flag. Verify it cross-compiles for your target (the CI matrix already covers `thumbv6m-none-eabi`, `thumbv8m.main-none-eabi`, and `thumbv8m.main-none-eabihf`). A `format` + `mount` round-trip against a RAM-backed `Storage` impl is one screenful and confirms the no_std story.

2. **Wrap your flash chip.** Implement the `Storage` trait against your device. If your flash needs aligned programs (most NOR does), wrap in `NorAlignedStorage`. The trait is sync; no async surface yet. See "Worked example" below for a typical SPI-NOR adapter and "Where the buffers live" for RAM placement.

3. **Migrate read-only paths first.** Anywhere your code calls into the C-FFI `littlefs2` for `resolve`, `read`, or `exists`, it can swap to `littlefs2-pure` today without losing functionality.

4. **Migrate inline writes when ready.** Configuration-style writes (small named values, upserted occasionally) work with `write_inline_to_root`. Compaction is transparent. Each `remove_from_root` durably drops an entry.

5. **Streaming append is live.** Log-style writers (the SMIL audit logger) can call `append_to_path` repeatedly with sub-block payloads; the CTZ path fills the existing tail in place. A stateful `File` handle (single open, many writes, one commit on close) is the remaining ergonomic item but is purely an optimization on metadata-commit pressure, not a correctness blocker.

## Storage trait quick reference

```rust
pub trait Storage {
    type Error: core::fmt::Debug;
    const READ_SIZE: usize;
    const PROG_SIZE: usize;
    const BLOCK_SIZE: usize;
    const BLOCK_COUNT: u32;
    const BLOCK_CYCLES: i32 = 500;
    const CACHE_SIZE: usize;
    const LOOKAHEAD_SIZE: usize;

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    fn program(&mut self, block: u32, off: u32, data: &[u8]) -> Result<(), Self::Error>;
    fn erase(&mut self, block: u32) -> Result<(), Self::Error>;
    fn sync(&mut self) -> Result<(), Self::Error> { Ok(()) }
}
```

All offsets and sizes are bytes. `block * BLOCK_SIZE + off` is the device-absolute byte position. The kernel does no bounds checking; misaligned or out-of-bounds calls are precondition violations.

### Where the buffers live

The kernel does not own any large in-RAM scratch. Every operation that needs a block-sized working area takes two `&mut [u8]` buffers (`buf_a`, `buf_b`), each exactly `S::BLOCK_SIZE` bytes. The caller decides where those bytes live: stack frames, a static, or a heap allocation if you have one. Pattern:

```rust
let mut buf_a = [0u8; <YourStorage as Storage>::BLOCK_SIZE];
let mut buf_b = [0u8; <YourStorage as Storage>::BLOCK_SIZE];
let mut fs = Fs::mount(your_storage, &mut buf_a, &mut buf_b)?;
fs.write_to_path(Path::new("/cfg")?, b"v1", &mut buf_a, &mut buf_b)?;
```

The same pair can be reused across every call; the kernel re-reads from storage as needed and treats the previous contents as scratch. For a 4 KiB block geometry that is 8 KiB of working RAM total; for the 256-byte test geometry it is 512 bytes.

`CACHE_SIZE` and `LOOKAHEAD_SIZE` in the trait are **advisory** in this release: there is no internal cache and the allocator does a full BFS scan with its own 512-byte bitmap on every `alloc_blocks` call. Declare values that mirror your chip's spec; they are exposed for forward compatibility with a streaming-lookahead allocator. The current kernel ignores them. `BLOCK_CYCLES` *is* load-bearing: it controls how often inter-pair wear-levelling fires (positive value, C reference default `500`; `-1` disables).

### Worked example: wiring a SPI NOR flash (W25Q-style, 4 KiB sectors, 16-byte page program)

```rust
use littlefs2_pure::storage::Storage;

pub struct W25qFlash<SPI, CS> { /* SPI handle, chip-select pin */ }

impl<SPI, CS> Storage for W25qFlash<SPI, CS>
where SPI: /* embedded-hal SPI */, CS: /* OutputPin */
{
    type Error = W25qError;
    const READ_SIZE:  usize = 1;      // SPI read is byte-granular
    const PROG_SIZE:  usize = 16;     // page-program window (small, lets you append into a partially-erased page)
    const BLOCK_SIZE: usize = 4096;   // 4 KiB sector
    const BLOCK_COUNT: u32  = 4096;   // 16 MiB part (4096 sectors * 4 KiB)
    const CACHE_SIZE:    usize = 256; // advisory; not consumed by the kernel today
    const LOOKAHEAD_SIZE: usize = 16; // advisory; not consumed by the kernel today

    fn read   (&mut self, b: u32, off: u32, buf: &mut [u8]) -> Result<(), Self::Error> { /* 0x03 cmd */ }
    fn program(&mut self, b: u32, off: u32, data: &[u8])    -> Result<(), Self::Error> { /* 0x02 cmd */ }
    fn erase  (&mut self, b: u32)                            -> Result<(), Self::Error> { /* 0x20 sector-erase */ }
    fn sync   (&mut self)                                    -> Result<(), Self::Error> { /* WIP poll, then Ok */ }
}

// Wrap in NorAlignedStorage so the kernel's byte-granular programs go
// through PROG_SIZE-aligned windows. The wrapper holds a 16-byte
// (= PROG_SIZE) cache; reuse the same NorAlignedStorage across mounts.
let device = W25qFlash::new(spi, cs);
let mut storage = NorAlignedStorage::new(device).expect("PROG_SIZE divides BLOCK_SIZE");

// Mount-time RAM: two scratch buffers, one BLOCK_SIZE each. Static in
// embedded code so the linker reports the footprint at build time.
static mut BUF_A: [u8; 4096] = [0; 4096];
static mut BUF_B: [u8; 4096] = [0; 4096];

let mut fs = match Fs::mount(storage, unsafe { &mut BUF_A }, unsafe { &mut BUF_B }) {
    Ok(fs) => fs,
    Err(Error::Unformatted) => {
        // Fresh chip; format and retry.
        let mut scratch = [0u8; 4096];
        Fs::format(/* re-acquire storage */, &mut scratch)?;
        Fs::mount(/* ... */)?
    }
    Err(Error::Corrupt) => panic!("flash bit rot; trigger recovery path"),
    Err(other) => return Err(other.into()),
};
```

The total RAM cost on this part is `2 * 4096 + 16 (PROG_SIZE cache) ≈ 8.2 KiB`. The 512-byte allocator bitmap is on the stack of `alloc_blocks` only when a CTZ write or `mkdir` runs, then freed. Nothing in the kernel `static`-allocates; everything is per-call.

### Mount error matrix

`Fs::mount` returns distinct variants for each failure category so a boot path can branch by reason:

| Variant | Meaning | Suggested action |
|---|---|---|
| `Error::Io` | `storage.read` faulted. | Retry on transient (loose flex cable); escalate otherwise. |
| `Error::GeometryMismatch` | Buffers are the wrong size, **or** the on-disk superblock advertises a different `block_size`/`block_count` than `S` declares. | Programmer bug, or wrong-chip-for-image. Do **not** auto-format. |
| `Error::Unformatted` | Both root-pair blocks read as `0xFF` end-to-end. Fresh fab / post-full-erase. | `Fs::format` then `Fs::mount` again, *if* the boot owner is the formatter. |
| `Error::Corrupt` | At least one root-pair block has been programmed, but no successfully verified commit can be read. | Escalate to recovery; do **not** auto-format (that would wipe potentially-recoverable data). |
| `Error::NotLittleFs` | Root pair parses cleanly (valid CCRC commits present) but the LittleFS magic NAME tag is absent. | Wrong filesystem on this chip; escalate. |
| `Error::UnsupportedVersion(v)` | Magic + body present, but version word is newer than this crate. | Escalate; cannot read forward-version data. |

The `Unformatted` versus `Corrupt` distinction is the load-bearing one for production boot: pristine flash is the expected first-boot state, programmed-but-unparseable is a "page the on-call" state.

### Power-loss recovery envelope

What an interrupt at each stage leaves on disk and what survives a remount, given how the kernel sequences writes:

- **Single-page tear during `program`** — *recoverable*. The kernel programs the new commit bytes only after the previous commit's CCRC is durable. A torn page in the middle of a new commit fails its CCRC on the next mount; the reader falls back to the previous CCRC boundary, and the active-block selector picks whichever block has the most recently *verified* commit. The torn region reads back as some mix of new and erased bytes; the CCRC catches it and the reader rejects the partial commit.
- **Erase abort mid-`erase`** — *recoverable*. The metadata-pair design assumes one block at a time can be in flux. The kernel only erases the **alternate** block during compaction; the **active** block stays intact and remains mountable. After power return, the alternate reads as a mix of old and erased bytes (no valid commits), the active still has its previous commits, and the reader picks the active.
- **Power loss between cross-directory rename's two commits** — *recovered on next mount*. `Fs::rename` lands the destination Create before the source Delete; each commit carries a balanced `MoveState` tag whose 12-byte body XORs to zero once both land. A crash between them leaves the filesystem-global gstate non-zero. `Fs::mount` walks every reachable metadata pair (bounded by `alloc::MAX_QUEUED_PAIRS = 32`), XOR-accumulates every committed `MoveState`, and if the result is non-zero decodes the in-flight `(src_pair, src_id)` and emits the missing source-side Delete + balancing MoveState before returning the `Fs` handle. Callers never observe the duplicate-entry state.
- **Power loss during streaming `append_to_path`** — *recoverable to pre-append state*. The new tail-block bytes are programmed first (still erased, so legal NOR programs), then any new chain blocks, then a single `UpdateCtz` commit. The `UpdateCtz` is the only step that becomes visible to a remount; an interrupt before it lands leaves the file at its pre-append size and the new blocks unreferenced (reclaimed by the next allocator scan).
- **Power loss during a wear-levelling relocation** — *user data is durable, wear cycle may be missed*. The compactor programs the new commit to the existing alternate *first* (the durability boundary), then copies it to a freshly allocated block, then commits the parent's flipped `DirStruct`. A crash before the fresh program leaves a non-relocated successful commit (wear cycle missed; next predicate firing tries again). A crash after the fresh program but before the parent commit leaves the fresh block orphaned (reclaimed by the next allocator scan) and the FS continues to observe the new state via the old pair's alternate.
- **Both blocks of a metadata pair erased concurrently** — *unrecoverable*. The kernel never does this; it always preserves the active while the alternate is in flux. The only way to reach this state is an out-of-band wipe (the firmware or a test harness erasing both blocks). The remount sees `Error::Unformatted` if both blocks are pristine, `Error::Corrupt` otherwise; no path back to the previous data.
- **Erase + program of a metadata pair without proper write ordering** — *out of scope*. The kernel issues `erase`, then `program`, then `sync` in that order; the `Storage` adapter is required to honor those as separate steps. An adapter that buffers across them (e.g., a NOR controller with deep write-cache that holds the erase until the next sync) breaks the recovery model. `NorAlignedStorage` is the reference wrapper; custom adapters must preserve the same ordering guarantees.

The torn-write scenarios in `tests/power_loss.rs` exercise every program-call boundary in the inline and CTZ-streaming-append paths through `TornWriteStorage` and assert the FS mounts to either the pre-state or the post-state. The atomic-move scenarios in `tests/atomic_move.rs` exercise every program-call boundary in a cross-directory rename and assert mount-time recovery converges. Together these promote the recovery envelope above from "by construction" to "verified by sweep."

## Verification matrix

| Layer | What's tested |
|---|---|
| CRC / Tag bit layout | Property tests against bit-by-bit reference; Kani harness for totality |
| Metadata commit / read | Property tests via independent builder ↔ reader round-trip; Kani harness for `MetadataReader::new` panic-freedom |
| Revision-counter signed compare | Kani harness for `rev_scmp` totality, antisymmetry, and wrap-aware increment |
| Mount / format / write | Integration tests with `MemStorage` |
| NOR-aligned program | Integration tests with `StrictNorStorage` (panics on 0→1 bit flips or misaligned programs) |
| Embedded cross-compile | CI matrix on three ARM targets (`thumbv6m`, `thumbv8m`, `thumbv8m-hf`) |
| C-to-Rust conformance | Committed vectors generated by C littlefs (empty / inline / CTZ / nested-dir); `tests/conformance.rs` mounts each via our reader and asserts the expected entries |
| Rust-to-C round-trip | `tools/verify_image/` builds a C verifier that mounts images Rust wrote and validates expected content; `tests/roundtrip.rs` exercises inline, CTZ, and nested-dir scenarios |
| Power-loss safety | `tests/power_loss.rs` sweeps `TornWriteStorage` across every program-call boundary in inline write and CTZ streaming append; the FS mounts to pre-state or post-state, never mid-state |
| Atomic move state | `tests/atomic_move.rs` sweeps the same `TornWriteStorage` across every program-call boundary in cross-dir rename; mount-time recovery converges |
| Wear levelling | `tests/wear_leveling.rs`: root never relocates; subdir pair relocates after `BLOCK_CYCLES` boundary; data survives remount; nested relocation propagates through grandparent |
| Parser totality | `fuzz/` crate (libFuzzer, nightly-only) covers `MetadataReader::new`, `Tag::from_bits`, `Path::new`, `Superblock::from_bytes`, `CtzStruct::from_bytes` |

The bit-accuracy claim against the C reference is now bidirectional: byte for byte, what we write the C reference can read (`tests/roundtrip.rs`), and what the C reference writes we can read (`tests/conformance.rs`).
