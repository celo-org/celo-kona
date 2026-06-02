//! Compile-fail guard for the mixed-denomination defenses of
//! [`celo_revm::units`]. Without these fixtures, a future contributor could
//! silently re-open the bug class behind commit `f2b24192` by adding (for
//! example) `impl Add<Fc> for Native` or even a same-denomination
//! `impl Add for Native` — the latter is deliberately absent so callers
//! pick a `saturating_*`/`checked_*` method and the overflow policy stays
//! visible at each use.
//!
//! Regenerate expected stderr after a deliberate API change with:
//! `TRYBUILD=overwrite cargo test -p celo-reth --test units_compile_fail`.

#[test]
fn units_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/native_plus_fc.rs");
    t.compile_fail("tests/ui/fc_plus_native.rs");
    t.compile_fail("tests/ui/native_into_u128_implicit.rs");
    t.compile_fail("tests/ui/native_from_fc.rs");
}
