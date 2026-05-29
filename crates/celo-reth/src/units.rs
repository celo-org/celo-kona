//! Newtypes for the two fee denominations used by CIP-64 transactions.
//!
//! CIP-64 ("fee abstraction") lets users pay gas in an ERC20 fee currency
//! ("FC") rather than native CELO. The pool and RPC layers must constantly
//! convert between FC-denominated and native-CELO-denominated amounts using
//! the on-chain exchange rate. Mixing the two denominations is a recurring
//! bug class — most prominently commit `f2b24192` ("don't include
//! FC-denominated gas in CIP-64 native_cost after rate conversion"), where
//! an FC-denominated gas cost was added to a native-CELO balance check and
//! falsely demoted valid CIP-64 transactions whose senders had ERC20 but
//! little CELO.
//!
//! The newtypes in this module make that bug class a compile error: native
//! and FC values have distinct types, the only way to cross the boundary is
//! through [`crate::pool::ExchangeRate`], and there is no implicit
//! `From<Native> for u128` — boundaries must be visible via
//! [`Native::into_inner`].
//!
//! Two width-flavors per denomination exist because the pool layer
//! ([`crate::pool`]) is `u128`-native (hot-path trait methods return `u128`)
//! and the RPC layer ([`crate::rpc`]) is `U256`-native (rate scaling needs
//! `checked_mul` on `U256` to avoid overflow on adversarial on-chain rates).
//! Forcing one width across both layers would either lose per-call
//! performance or lose overflow safety.
//!
//! **Forward-compatibility:** these types may relocate to `celo-revm` if
//! [`celo_revm::fee_currency_context::FeeCurrencyContext`] is later unified
//! with [`crate::pool::ExchangeRate`]. Today the EVM handler has a parallel
//! `U256`-based implementation; the unification is out of scope.

use alloy_primitives::U256;
use core::{
    fmt::{self, Display},
    ops::{Add, Sub},
};

// ---------------------------------------------------------------------------
// u128-backed newtypes
// ---------------------------------------------------------------------------

/// A native-CELO amount, `u128`-backed.
///
/// Used in the pool layer where the hot-path `alloy_consensus::Transaction`
/// trait returns `u128`. For wider amounts (e.g. RPC-layer rate scaling),
/// use [`NativeU256`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Native(u128);

/// A fee-currency-denominated amount, `u128`-backed.
///
/// "Fee currency" is the per-transaction ERC20 token nominated by a CIP-64
/// `fee_currency` field; the amount is in the token's smallest unit. For
/// wider amounts (e.g. RPC-layer rate scaling), use [`FcU256`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fc(u128);

impl Native {
    /// Construct from a raw `u128` already known to be native-CELO denominated.
    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    /// Unwrap to the raw `u128` value.
    ///
    /// Use at trait boundaries (e.g. `Transaction::max_fee_per_gas`) where
    /// the outer signature is in terms of `u128`.
    pub const fn into_inner(self) -> u128 {
        self.0
    }

    /// Saturating addition. Both operands are native-CELO; result is native-CELO.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Saturating subtraction. Both operands are native-CELO; result is native-CELO.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl Fc {
    /// Construct from a raw `u128` already known to be FC-denominated.
    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    /// Unwrap to the raw `u128` value.
    pub const fn into_inner(self) -> u128 {
        self.0
    }

    /// Saturating addition. Both operands are FC; result is FC.
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// Saturating subtraction. Both operands are FC; result is FC.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl From<u128> for Native {
    fn from(v: u128) -> Self {
        Self(v)
    }
}

impl From<u128> for Fc {
    fn from(v: u128) -> Self {
        Self(v)
    }
}

// Display delegates to the inner value so that tracing fields written as
// `%native` or `%fc` log a bare number (matching what the raw `u128` would
// have produced), not `Native(123)`.
impl Display for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Display for Fc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

// Note: there is intentionally NO `impl From<Native> for u128` (or for `Fc`).
// Crossing the type boundary must go through `into_inner()` so it stays
// visible to a reader of the code.

// Same-denomination arithmetic. Mixed-denomination arithmetic is
// deliberately absent — that's the whole point of the type system here.
impl Add for Native {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for Native {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Add for Fc {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub for Fc {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

// ---------------------------------------------------------------------------
// U256-backed newtypes
// ---------------------------------------------------------------------------

/// A native-CELO amount, `U256`-backed.
///
/// Used in the RPC layer where rate-scaling needs `checked_mul` on `U256`
/// to avoid overflow on adversarial on-chain rates. For hot-path pool
/// trait values, use [`Native`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeU256(U256);

/// A fee-currency-denominated amount, `U256`-backed.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FcU256(U256);

impl NativeU256 {
    /// Construct from a raw `U256` already known to be native-CELO denominated.
    pub const fn new(v: U256) -> Self {
        Self(v)
    }

    /// Unwrap to the raw `U256` value.
    pub const fn into_inner(self) -> U256 {
        self.0
    }

    /// Borrow the raw `U256` value.
    ///
    /// Used at borrow-returning trait boundaries such as
    /// `reth_transaction_pool::PoolTransaction::cost`, which is declared as
    /// `fn cost(&self) -> &U256`.
    pub const fn as_u256(&self) -> &U256 {
        &self.0
    }

    /// Narrow to a [`Native`], saturating at `u128::MAX` on overflow.
    pub fn saturating_to_u128(self) -> Native {
        Native(u128::try_from(self.0).unwrap_or(u128::MAX))
    }

    /// Saturating subtraction. Both operands are native-CELO; result is native-CELO.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl FcU256 {
    /// Construct from a raw `U256` already known to be FC-denominated.
    pub const fn new(v: U256) -> Self {
        Self(v)
    }

    /// Unwrap to the raw `U256` value.
    pub const fn into_inner(self) -> U256 {
        self.0
    }

    /// Borrow the raw `U256` value.
    pub const fn as_u256(&self) -> &U256 {
        &self.0
    }

    /// Narrow to an [`Fc`], saturating at `u128::MAX` on overflow.
    pub fn saturating_to_u128(self) -> Fc {
        Fc(u128::try_from(self.0).unwrap_or(u128::MAX))
    }

    /// Saturating subtraction. Both operands are FC; result is FC.
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

// Same-denomination addition for U256-backed types. Used by the RPC layer to
// combine an FC-denominated base fee and an FC-denominated tip into a max-fee
// default (and similarly for native-denominated sums). Mixed-denomination
// addition is deliberately absent.
impl Add for NativeU256 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Add for FcU256 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl From<U256> for NativeU256 {
    fn from(v: U256) -> Self {
        Self(v)
    }
}

impl From<U256> for FcU256 {
    fn from(v: U256) -> Self {
        Self(v)
    }
}

impl Display for NativeU256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Display for FcU256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

// Widening (lossless): u128-backed → U256-backed within the same denomination.
impl From<Native> for NativeU256 {
    fn from(v: Native) -> Self {
        Self(U256::from(v.0))
    }
}

impl From<Fc> for FcU256 {
    fn from(v: Fc) -> Self {
        Self(U256::from(v.0))
    }
}

// Note: there is intentionally NO `From<Native> for FcU256`, no
// `From<Fc> for NativeU256`, no mixed Add/Sub, and no implicit
// `From<NativeU256> for U256`. All cross-denomination conversions must
// go through [`crate::pool::ExchangeRate`].

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_u128_backed() {
        assert_eq!(Native::new(5).into_inner(), 5);
        assert_eq!(Fc::new(5).into_inner(), 5);
    }

    #[test]
    fn roundtrip_u256_backed() {
        let v = U256::from(5u64);
        assert_eq!(NativeU256::new(v).into_inner(), v);
        assert_eq!(FcU256::new(v).into_inner(), v);
    }

    #[test]
    fn widening_preserves_value() {
        let n256: NativeU256 = Native::new(42).into();
        assert_eq!(n256.into_inner(), U256::from(42u64));

        let f256: FcU256 = Fc::new(42).into();
        assert_eq!(f256.into_inner(), U256::from(42u64));
    }

    #[test]
    fn narrowing_saturates_on_overflow() {
        let huge = NativeU256::new(U256::from(u128::MAX) + U256::from(1u64));
        assert_eq!(huge.saturating_to_u128(), Native::new(u128::MAX));

        let huge_fc = FcU256::new(U256::from(u128::MAX) + U256::from(1u64));
        assert_eq!(huge_fc.saturating_to_u128(), Fc::new(u128::MAX));
    }

    #[test]
    fn narrowing_lossless_when_in_range() {
        let small = NativeU256::new(U256::from(100u64));
        assert_eq!(small.saturating_to_u128(), Native::new(100));

        let small_fc = FcU256::new(U256::from(100u64));
        assert_eq!(small_fc.saturating_to_u128(), Fc::new(100));
    }

    #[test]
    fn same_denomination_arithmetic() {
        assert_eq!(Native::new(2) + Native::new(3), Native::new(5));
        assert_eq!(Native::new(5) - Native::new(2), Native::new(3));
        assert_eq!(Fc::new(2) + Fc::new(3), Fc::new(5));
        assert_eq!(Fc::new(5) - Fc::new(2), Fc::new(3));
    }

    #[test]
    fn saturating_arithmetic() {
        assert_eq!(Native::new(u128::MAX).saturating_add(Native::new(1)), Native::new(u128::MAX));
        assert_eq!(Native::new(0).saturating_sub(Native::new(1)), Native::new(0));
        assert_eq!(Fc::new(u128::MAX).saturating_add(Fc::new(1)), Fc::new(u128::MAX));
        assert_eq!(Fc::new(0).saturating_sub(Fc::new(1)), Fc::new(0));
    }
}
