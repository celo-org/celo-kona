// Symmetric to native_plus_fc: the type system must reject the reversed order too.
use celo_reth::units::{Fc, Native};

fn main() {
    let _ = Fc::new(1) + Native::new(2);
}
