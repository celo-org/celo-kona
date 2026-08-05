use celo_revm::units::{Fc, Native};

fn main() {
    let _ = Fc::new(1) + Native::new(2);
}
