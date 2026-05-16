# ADR-0008: format bootstrap diverges byte-wise from the C reference

- **Status**: accepted
- **Date**: 2026-05-15

## Context

A post v1.0 review suggested a test pinning `Fs::format` byte-for-byte
against the C reference's empty-format vector
(`tests/vectors/01_empty_format.bin`). Writing that test surfaced that
the two are **not** byte-identical:

- The C reference initializes both blocks of the root metadata pair on
  format: block A at revision 1 and block B at revision 2, each a full
  superblock commit, plus a tail-area tag in block A. Blocks 2 to 7
  are erased.
- `Fs::format` writes only block A at revision 1, omits the tail-area
  tag, and leaves block B erased (its CCRC differs as a consequence).

Both images are valid LittleFS filesystems. The existing conformance
test mounts the C vector cleanly with our reader, and the roundtrip
suite has the C reference mount images our writer produces. The
divergence is in how an empty filesystem is bootstrapped, not in how
any on-disk structure is encoded once present.

`CLAUDE.md` stated, without qualification, that the on-disk format
must match the C reference byte for byte. Taken literally that makes
this a defect. Taken as the project actually verifies it (conformance
of structure encoding plus bidirectional interop), the format
bootstrap was never pinned and the unqualified claim was too strong.

## Decision

Treat the format bootstrap byte divergence as an accepted, documented
property of the 1.x line, not a defect to fix in a patch release.

Rationale: the byte-fidelity guarantee that earns its keep is that
**existing** on-disk structures (tags, CRCs, CTZ chains, the
superblock body, revision counters on commits we write) are encoded
exactly as the C reference encodes them, which conformance and
roundtrip prove. How many root-pair blocks an empty format
pre-initializes, and whether it pre-writes block B at revision 2, is
an implementation choice the spec does not pin; a single-block-rev-1
empty format is valid and mounts on both implementations. Rewriting
the format encoder to mirror the C reference byte-for-byte would
change every future on-disk image and demand full conformance
re-validation, which is a deliberate decision for a future version,
not hardening work for v1.0.1.

`CLAUDE.md`'s bit-accuracy note is narrowed to say what is actually
verified: structure encode/decode is byte-faithful and
conformance-pinned; the format bootstrap may differ from the C
reference while remaining semantically conformant, and that interop
is proven by conformance plus roundtrip rather than by byte equality.

The review's test #5 is kept, reworked to assert the semantic
invariant it can honestly pin: `Fs::format` produces an image that
mounts as a clean empty filesystem with the expected geometry and is
stable across a remount. It explicitly does not assert byte equality
with the C vector and points here.

## Consequences

**Wins.** The finding is recorded rather than hidden behind a weaker
test. The verified guarantee is now stated accurately, so a future
reader does not infer a byte-for-byte format promise the project does
not test. The reworked #5 still guards the format path against a
regression that would make an empty filesystem unmountable or
unstable.

**Costs.** A consumer that expected `Fs::format` output to be
bit-identical to the C reference's must not rely on that; only
structural encoding and bidirectional interop are guaranteed. If a
future version chooses byte-identical format, this ADR is the record
of why it was deferred and what changes (root-pair block B at rev 2,
the tail-area tag) it must add.

**Explicitly out of scope.** This ADR does not change `Fs::format`,
does not weaken the conformance or roundtrip gates, and does not
narrow the byte-fidelity of structure encoding, which remains
non-negotiable and pinned.

## Related

- `tests/review_additions.rs`: the reworked #5 semantic-invariant
  test.
- `tests/conformance.rs` (`vector_01_empty_format_mounts_clean`) and
  `tests/roundtrip.rs`: the interop evidence this decision rests on.
- `CLAUDE.md`: the narrowed bit-accuracy note.
- Post v1.0 review test-additions item #5.
