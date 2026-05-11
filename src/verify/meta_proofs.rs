//! Totality and agreement of the metadata-pair primitives.
//!
//! `rev_scmp` is `(int32_t)(a - b)` in C; in Rust it is the
//! `wrapping_sub` cast to `i32`. The active-block selector in
//! `MetadataPair::parse` and the C reference's `lfs_dir_fetchmatch`
//! both depend on the sign of this value, so getting it right under
//! every wraparound case is load-bearing.

use crate::meta::rev_scmp;

/// `rev_scmp` is total: every `(a, b)` returns an `i32`. By
/// construction (no division, no `unwrap`, no panicking arithmetic
/// since `wrapping_sub` cannot overflow), but Kani pins it.
#[kani::proof]
fn rev_scmp_total() {
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    let _ = rev_scmp(a, b);
}

/// `rev_scmp(a, b) == 0` iff `a == b`. Reflexivity.
#[kani::proof]
fn rev_scmp_zero_iff_equal() {
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    let r = rev_scmp(a, b);
    if a == b {
        assert_eq!(r, 0);
    } else {
        assert_ne!(r, 0);
    }
}

/// `rev_scmp(a, b) == -rev_scmp(b, a)` for every pair where neither
/// the value nor its negation is `i32::MIN`. The exclusion handles
/// the only case where `-r` wraps. The C reference behaves the same
/// way.
#[kani::proof]
fn rev_scmp_antisymmetric() {
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    let r_ab = rev_scmp(a, b);
    let r_ba = rev_scmp(b, a);
    kani::assume(r_ab != i32::MIN);
    kani::assume(r_ba != i32::MIN);
    assert_eq!(r_ab, -r_ba);
}

/// Pin the wrap-aware semantics: increment-by-one is "newer", even
/// across the u32 boundary. `a == b + 1` => `rev_scmp(a, b) > 0`.
#[kani::proof]
fn rev_scmp_increment_is_newer() {
    let b: u32 = kani::any();
    let a = b.wrapping_add(1);
    assert!(rev_scmp(a, b) > 0);
    assert!(rev_scmp(b, a) < 0);
}
