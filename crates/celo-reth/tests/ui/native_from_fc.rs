// A fee-currency amount cannot be silently re-interpreted as native CELO.
// Conversion must go through `ExchangeRate`.
use celo_revm::units::{Fc, Native};

fn main() {
    let _: Native = Fc::new(1).into();
}
