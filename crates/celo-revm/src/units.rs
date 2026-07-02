//! Newtypes for the two fee denominations used by CIP-64 transactions.
//!
//! CIP-64 ("fee abstraction") lets users pay gas in an ERC20 fee currency
//! ("FC") rather than native CELO. Three layers convert between
//! FC-denominated and native-CELO-denominated amounts using the on-chain
//! exchange rate: [`FeeCurrencyContext`](crate::fee_currency_context::FeeCurrencyContext)
//! in the EVM handler, `ExchangeRate` in the celo-reth pool validator, and
//! `scale_to_fc` / `scale_to_native` in the celo-reth RPC layer.
//!
//! Mixing the two denominations is a recurring bug class — most prominently
//! commit `f2b24192`, where an FC-denominated gas cost was added to a
//! native-CELO balance check and falsely rejected valid CIP-64 transactions
//! whose senders had ERC20 but little CELO. The newtypes here make that bug
//! class a compile error: native and FC values have distinct types, the only
//! way to cross the boundary is through one of the three conversion APIs
//! above, and the conversions in and out of the raw integer types are
//! explicit ([`Native::new`] / [`Native::into_inner`] and the analogous
//! `Fc` / `NativeU256` / `FcU256` constructors) — no `From<u128>` /
//! `From<U256>` impl picks a denomination by inferred type.
//!
//! Two width-flavors per denomination exist because the pool layer is
//! `u128`-native (hot-path trait methods return `u128`) and the RPC / EVM
//! handler layers need `U256` `checked_mul` to be overflow-safe against
//! adversarial on-chain rates.

use alloy_primitives::U256;
use core::fmt::{self, Display};

// ---------------------------------------------------------------------------
// u128-backed newtypes
// ---------------------------------------------------------------------------

/// A native-CELO amount, `u128`-backed.
///
/// Used in the pool layer where the hot-path `alloy_consensus::Transaction`
/// trait returns `u128`. For wider amounts (e.g. RPC-layer rate scaling),
/// use [`NativeU256`].
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Native(u128);

/// A fee-currency-denominated amount, `u128`-backed.
///
/// "Fee currency" is the per-transaction ERC20 token nominated by a CIP-64
/// `fee_currency` field; the amount is in the token's smallest unit. For
/// wider amounts (e.g. RPC-layer rate scaling), use [`FcU256`].
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fc(u128);

impl Native {
    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    pub const fn into_inner(self) -> u128 {
        self.0
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl Fc {
    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    pub const fn into_inner(self) -> u128 {
        self.0
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

// Display delegates to the inner value so tracing fields written as `%native`
// or `%fc` log a bare number, matching the pre-newtype output.
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

// Intentionally NO `impl From<Native> for u128` (or for `Fc`). Crossing the
// type boundary must go through `into_inner()` so it stays visible to a reader.
//
// Intentionally NO `impl Add`/`impl Sub`. Inheriting `u128`'s panic-on-overflow
// arithmetic at a controlled-boundary type is the wrong default — every actual
// call site uses `saturating_*` or `checked_*` anyway. Forcing the explicit
// form makes the overflow policy visible at each use.

// ---------------------------------------------------------------------------
// U256-backed newtypes
// ---------------------------------------------------------------------------

/// A native-CELO amount, `U256`-backed.
///
/// Used in the RPC layer where rate-scaling needs `checked_mul` on `U256`
/// to avoid overflow on adversarial on-chain rates. For hot-path pool
/// trait values, use [`Native`].
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativeU256(U256);

/// A fee-currency-denominated amount, `U256`-backed.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FcU256(U256);

impl NativeU256 {
    pub const fn new(v: U256) -> Self {
        Self(v)
    }

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

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl FcU256 {
    pub const fn new(v: U256) -> Self {
        Self(v)
    }

    pub const fn into_inner(self) -> U256 {
        self.0
    }

    pub const fn as_u256(&self) -> &U256 {
        &self.0
    }

    /// Narrow to an [`Fc`], saturating at `u128::MAX` on overflow.
    pub fn saturating_to_u128(self) -> Fc {
        Fc(u128::try_from(self.0).unwrap_or(u128::MAX))
    }

    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
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

// Intentionally NO `From<Native> for FcU256`, no `From<Fc> for NativeU256`,
// no operator overloads, and no implicit `From<NativeU256> for U256`. All
// cross-denomination conversions must go through `ExchangeRate` or the
// `FeeCurrencyContext` conversion APIs; arithmetic goes through the
// `saturating_*`/`checked_*` methods so the overflow policy stays explicit.

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
        assert_eq!(
            Native::new(2).saturating_add(Native::new(3)),
            Native::new(5)
        );
        assert_eq!(
            Native::new(5).checked_sub(Native::new(2)),
            Some(Native::new(3))
        );
        assert_eq!(Fc::new(2).saturating_add(Fc::new(3)), Fc::new(5));
        assert_eq!(Fc::new(5).checked_sub(Fc::new(2)), Some(Fc::new(3)));
    }

    #[test]
    fn saturating_arithmetic() {
        assert_eq!(
            Native::new(u128::MAX).saturating_add(Native::new(1)),
            Native::new(u128::MAX)
        );
        assert_eq!(
            Native::new(0).saturating_sub(Native::new(1)),
            Native::new(0)
        );
        assert_eq!(
            Fc::new(u128::MAX).saturating_add(Fc::new(1)),
            Fc::new(u128::MAX)
        );
        assert_eq!(Fc::new(0).saturating_sub(Fc::new(1)), Fc::new(0));
    }
}
