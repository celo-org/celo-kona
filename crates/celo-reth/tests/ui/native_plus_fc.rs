// Mixing native-CELO and fee-currency amounts under `+` must not compile.
use celo_reth::units::{Fc, Native};

fn main() {
    let _ = Native::new(1) + Fc::new(2);
}
