# Integration guide

This file points downstream consumers (SMIL firmware, etc.) at the relevant entry points for each stage of integration. The full API rationale lives in module rustdoc; this is the working surface as of `main`.

## What works today

| Capability | API | Notes |
|---|---|---|
| Format a fresh device | `Fs::format(&mut storage, &mut scratch)` | One scratch buffer of `BLOCK_SIZE` |
| Mount an existing image | `Fs::mount(storage, &mut buf_a, &mut buf_b)` | Two scratch buffers of `BLOCK_SIZE` |
| Resolve an absolute path | `Fs::resolve(path, &mut buf_a, &mut buf_b)` | Returns `ResolvedPath { entry, struct_type, struct_body, pair }` |
| Check existence | `Fs::exists(path, ...)` | Wraps `resolve`, returns `bool` |
| Read inline file content | Use `resolved.struct_body` directly when `struct_type == InlineStruct` | Zero-copy slice into your buffer |
| Read CTZ file content | `Fs::read_ctz(&ctz_struct, &mut out, &mut scratch)` | After parsing `CtzStruct::from_bytes(resolved.struct_body)` |
| Write or update a small inline file | `Fs::write_inline_to_root(name, content, ...)` | Upsert semantics; appends if room, else compacts to the alternate |
| Write any-size file at root (auto-dispatch) | `Fs::write_to_root(name, content, ...)` | Picks inline (≤128 bytes) or CTZ; create-only on the CTZ side |
| Write any-size file at arbitrary path | `Fs::write_to_path(path, content, ...)` | Parent directory must exist |
| Write a large file as CTZ | `Fs::write_ctz_to_root(name, content, ...)` | Allocates blocks and writes the skip-list chain |
| Create a directory | `Fs::mkdir(path, ...)` | Allocates a fresh metadata pair, writes empty initial commit |
| Remove a file at root | `Fs::remove_from_root(name, ...)` | Splice-correct; deleted entries no longer resolve |
| Append to a file (atomic full-rewrite) | `Fs::append_to_path(path, additional, content_scratch, ...)` | Creates if missing; handles inline↔CTZ transitions |
| Remove a file at path | `Fs::remove_at_path(path, ...)` | Resolves parent, then removes |
| List a directory | `Fs::list_dir(path, callback, ...)` | Splice-correct, skips superblock; single-pair only (no HardTail chasing yet) |
| List root directory | `Fs::list_root(callback, ...)` | Skips the superblock; renumbers across splice |
| NOR-aligned program wrapper | `NorAlignedStorage::new(your_storage)` | Caches programs to `PROG_SIZE` windows |

## What's not yet supported

| Capability | Tracking |
|---|---|
| Stateful `File<'fs, S>` handle with `open / read / write / seek / set_len / sync` | Phase 2f.2 (the existing `append_to_path` covers append-only workloads atomically) |
| Streaming append for huge files (no full read-rewrite) | Phase 2f.2 (true incremental CTZ chain extension) |
| `rmdir` (with emptiness check + pair free) | Phase 2g |
| `rename` | Phase 2g |
| Multi-pair directory listing (HardTail chasing in `list_dir`) | Phase 2g |
| Power-loss fuzz / Kani harnesses | Phase 3 |

For the SMIL audit logger specifically: `create_dir`, `File::write` with append/truncate, `File::seek`, and `set_len` all map to Phases 2c–2f. The current write surface is sufficient for upsert-style configuration files (small keys + small values), not for log-style streaming writes.

## Suggested integration steps

1. **Smoke test the embedded build path.** Add `littlefs2-pure` as an optional dep of your firmware behind a feature flag. Verify it cross-compiles for your target (the CI matrix already covers `thumbv6m-none-eabi`, `thumbv8m.main-none-eabi`, and `thumbv8m.main-none-eabihf`). A `format` + `mount` round-trip against a RAM-backed `Storage` impl is one screenful and confirms the no_std story.

2. **Wrap your flash chip.** Implement the `Storage` trait against your device. If your flash needs aligned programs (most NOR does), wrap in `NorAlignedStorage`. The trait is sync; no async surface yet.

3. **Migrate read-only paths first.** Anywhere your code calls into the C-FFI `littlefs2` for `resolve`, `read`, or `exists`, it can swap to `littlefs2-pure` today without losing functionality.

4. **Migrate inline writes when ready.** Configuration-style writes (small named values, upserted occasionally) work with `write_inline_to_root`. Compaction is transparent. Each `remove_from_root` durably drops an entry.

5. **Wait on write-heavy paths.** Anything resembling a streaming log writer (the SMIL audit logger) needs the stateful `File` API. Track Phase 2f in `KNOWN_ISSUES.md`.

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

## Verification matrix

| Layer | What's tested |
|---|---|
| CRC / Tag bit layout | Property tests against bit-by-bit reference |
| Metadata commit / read | Property tests via independent builder ↔ reader round-trip |
| Mount / format / write | Integration tests with `MemStorage` |
| NOR-aligned program | Integration tests with `StrictNorStorage` (panics on 0→1 bit flips or misaligned programs) |
| Embedded cross-compile | CI matrix on three ARM targets |

C-reference conformance vectors (the bit-level oracle described in ADR-0004) are not yet wired up. Until they are, the bit-accuracy claim is "round-trips through our own reader." Cross-mount with the C reference is the next durability gate.
