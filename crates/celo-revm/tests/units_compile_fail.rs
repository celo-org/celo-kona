//! Compile-fail guard for the mixed-denomination defenses of
//! [`celo_revm::units`]. Without these fixtures, a future contributor could
//! silently re-open the bug class behind commit `f2b24192` by adding (for
//! example) `impl Add<Fc> for Native` or even a same-denomination
//! `impl Add for Native` — the latter is deliberately absent so callers
//! pick a `saturating_*`/`checked_*` method and the overflow policy stays
//! visible at each use.
//!
//! Regenerate expected stderr after a deliberate API change with:
//! `TRYBUILD=overwrite cargo test -p celo-revm --test units_compile_fail`.
//!
//! This lives in `celo-revm`, not `celo-reth`, on purpose. trybuild resolves
//! its scratch project from scratch under `--offline`, and it inherits the
//! dependencies of the crate under test. From `celo-reth` that pulled in the
//! `reth-*` git dependencies, which the workspace declares against
//! `paradigmxyz/reth` and redirects via `[patch]` to `celo-org/reth` — so the
//! unpatched source is absent from `Cargo.lock` and never fetched, and the
//! offline resolve failed on any clean checkout. `celo-revm` has no `reth-*`
//! dependencies, so the scratch resolve stays satisfiable.

#[test]
fn units_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/native_plus_fc.rs");
    t.compile_fail("tests/ui/fc_plus_native.rs");
    t.compile_fail("tests/ui/native_into_u128_implicit.rs");
    t.compile_fail("tests/ui/native_from_fc.rs");
}
