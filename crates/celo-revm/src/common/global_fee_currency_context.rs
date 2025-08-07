//! Global context for storing fee currency context across thread boundaries.

use crate::common::fee_currency_context::FeeCurrencyContext;
use core::sync::atomic::AtomicBool;
use lazy_static::lazy_static;

#[cfg(feature = "std")]
use std::sync::RwLock;

#[cfg(not(feature = "std"))]
use spin::RwLock;

lazy_static! {
    static ref FEE_CURRENCY_CONTEXT: RwLock<Option<FeeCurrencyContext>> = RwLock::new(None);
    static ref FEE_CURRENCY_CONTEXT_SET: AtomicBool = AtomicBool::new(false);
}

/// Sets the global fee currency context.
pub fn set_fee_currency_context(context: FeeCurrencyContext) {
    #[cfg(feature = "std")]
    {
        let mut fee_currency_context = FEE_CURRENCY_CONTEXT.write().unwrap();
        *fee_currency_context = Some(context);
    }
    #[cfg(not(feature = "std"))]
    {
        let mut fee_currency_context = FEE_CURRENCY_CONTEXT.write();
        *fee_currency_context = Some(context);
    }
    FEE_CURRENCY_CONTEXT_SET.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Gets a copy of the global fee currency context if it has been set.
pub fn get_fee_currency_context() -> Option<FeeCurrencyContext> {
    if !FEE_CURRENCY_CONTEXT_SET.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }

    #[cfg(feature = "std")]
    {
        let fee_currency_context = FEE_CURRENCY_CONTEXT.read().unwrap();
        fee_currency_context.clone()
    }
    #[cfg(not(feature = "std"))]
    {
        let fee_currency_context = FEE_CURRENCY_CONTEXT.read();
        fee_currency_context.clone()
    }
}

/// Clears the global fee currency context.
pub fn clear_fee_currency_context() {
    #[cfg(feature = "std")]
    {
        let mut fee_currency_context = FEE_CURRENCY_CONTEXT.write().unwrap();
        *fee_currency_context = None;
    }
    #[cfg(not(feature = "std"))]
    {
        let mut fee_currency_context = FEE_CURRENCY_CONTEXT.write();
        *fee_currency_context = None;
    }
    FEE_CURRENCY_CONTEXT_SET.store(false, core::sync::atomic::Ordering::Relaxed);
}
