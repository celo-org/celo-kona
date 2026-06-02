// Crossing the type boundary must be visible: there is no implicit
// coercion from `Native` to `u128`. Callers must spell out `.into_inner()`.
use celo_revm::units::Native;

fn main() {
    let _: u128 = Native::new(1);
}
