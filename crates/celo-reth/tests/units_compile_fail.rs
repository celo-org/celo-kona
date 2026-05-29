//! Compile-fail regression guard for the [`celo_reth::units`] safety property.
//!
//! These fixtures pin the mixed-denomination defenses that the rest of the
//! refactor (PRs 2–5) relies on. Without them, a future contributor could
//! silently re-open the f2b24192 bug class by adding (for example)
//! `impl Add<Fc> for Native`, and the type system would no longer reject
//! mixing FC-denominated and native-CELO amounts.
//!
//! Regenerate expected stderr files (after a deliberate API change) with:
//!
//! ```text
//! TRYBUILD=overwrite cargo test -p celo-reth --test units_compile_fail
//! ```

#[test]
fn units_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/native_plus_fc.rs");
    t.compile_fail("tests/ui/fc_plus_native.rs");
    t.compile_fail("tests/ui/native_into_u128_implicit.rs");
    t.compile_fail("tests/ui/native_from_fc.rs");
}
