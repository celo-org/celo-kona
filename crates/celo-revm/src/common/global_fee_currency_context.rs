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
    static ref CONTEXT_SET: AtomicBool = AtomicBool::new(false);
}

/// Sets the global fee currency context.
pub fn set_fee_currency_context(context: FeeCurrencyContext) {
    #[cfg(feature = "std")]
    {
        let mut global_fee_currency_context = FEE_CURRENCY_CONTEXT.write().unwrap();
        *global_fee_currency_context = Some(context);
    }
    #[cfg(not(feature = "std"))]
    {
        let mut global_fee_currency_context = FEE_CURRENCY_CONTEXT.write();
        *global_fee_currency_context = Some(context);
    }
    CONTEXT_SET.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Gets a copy of the global fee currency context if it has been set.
pub fn get_fee_currency_context() -> Option<FeeCurrencyContext> {
    if !CONTEXT_SET.load(core::sync::atomic::Ordering::Relaxed) {
        return None;
    }

    #[cfg(feature = "std")]
    {
        let global_fee_currency_context = FEE_CURRENCY_CONTEXT.read().unwrap();
        global_fee_currency_context.clone()
    }
    #[cfg(not(feature = "std"))]
    {
        let global_fee_currency_context = FEE_CURRENCY_CONTEXT.read();
        global_fee_currency_context.clone()
    }
}

/// Clears the global fee currency context.
pub fn clear_fee_currency_context() {
    #[cfg(feature = "std")]
    {
        let mut global_fee_currency_context = FEE_CURRENCY_CONTEXT.write().unwrap();
        *global_fee_currency_context = None;
    }
    #[cfg(not(feature = "std"))]
    {
        let mut global_fee_currency_context = FEE_CURRENCY_CONTEXT.write();
        *global_fee_currency_context = None;
    }
    CONTEXT_SET.store(false, core::sync::atomic::Ordering::Relaxed);
}
