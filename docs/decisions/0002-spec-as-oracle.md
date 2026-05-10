# ADR-0002: The SPEC is the oracle, the C reference is the behavioral check

- **Status**: accepted
- **Date**: 2026-05-10

## Context

A pure Rust port has two candidate sources of truth: the official LittleFS v2 [SPEC.md](https://github.com/littlefs-project/littlefs/blob/master/SPEC.md), and the C reference implementation. Treating either as primary has predictable failure modes.

If the C reference is primary, the Rust port inherits the C API shape and the C internal data layout choices. The result is a mechanical translation that mirrors C idioms (pointer arithmetic, mutable in/out parameters, sentinel values for absent fields) at the cost of idiomatic Rust. Worse, undocumented C behaviors become load bearing in Rust; later spec clarifications that diverge from the C reference's exact behavior break the port.

If the SPEC is primary but the C reference is not consulted, the port is bit accurate only where the spec is bit accurate. The spec leaves several encoding details to the implementation (tag XOR seeding edge cases, the FCRC redundancy nibble layout, the exact compaction trigger). A port that guesses these wrong is unmountable by the C reference.

ferrodec arrived at the same shape with IEEE 754:2019: the standard is the oracle, but the General Decimal Arithmetic dectest vectors are the byte-for-byte check.

## Decision

The LittleFS v2 SPEC is the source of behavior. The Rust API is shaped by Rust idiom (typestate, sealed traits, ownership aware design), not by the C reference's signatures. Where the SPEC underspecifies, the C reference's bit level output is the tie breaker, captured as a committed golden vector in `tests/vectors/` and asserted in `tests/conformance.rs`.

A disagreement with the C reference that is unambiguous in the spec is filed as a C reference bug or ambiguity in `KNOWN_ISSUES.md`, with the SPEC paragraph cited. We do not silently match a C bug; we document it and choose.

## Consequences

**Wins.**

- The Rust API is shaped for Rust. Method signatures, error variants, and lifetimes follow Rust conventions, not C ones.
- Spec compliance is the testable property. When the C reference is updated, our test surface tells us whether the change was a spec clarification (we update too) or a C internal refactor (we ignore).
- Golden vectors are durable. A vector generated today still asserts the same bit pattern in five years, even after the C reference moves on.

**Costs.**

- Vector generation tooling has to exist. Phase 1's exit condition requires `tools/gen_vectors.sh` (or similar) that runs the C reference against a fixed scenario set and emits the committed image files.
- Spec ambiguities require judgment. When the C reference and a defensible spec reading diverge, we have to decide and record the decision. That decision lives in this ADR series.

**Explicitly out of scope.**

- LittleFS v1 compatibility. The format is not the same; the port targets v2 exclusively.

## Related

- ferrodec ADR-0010 (testing strategy) for the per file conformance expected counts pattern.
- ferrodec's `tests/conformance.rs` for the runner shape we will mirror.
