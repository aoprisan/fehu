//! Every transcendental operation and every RNG-to-float conversion in the crate
//! lives here, so results are bit-identical across native and `wasm32`.
//!
//! Rules: only `libm` for `exp`/`ln`/`sqrt`/`pow`/`cos`/`round`, only
//! `next_u64` from the RNG, and no `mul_add`/`powi` (LLVM intrinsics).

use rand_core::Rng as _;
use rand_xoshiro::Xoshiro256PlusPlus;

pub(crate) const LN_2: f64 = core::f64::consts::LN_2;
const TWO_PI: f64 = 2.0 * core::f64::consts::PI;
const U53: f64 = 1.0 / ((1u64 << 53) as f64);

#[inline]
pub(crate) fn exp(x: f64) -> f64 {
    libm::exp(x)
}

/// `e^x − 1`, accurate for small `x`.
#[inline]
pub(crate) fn expm1(x: f64) -> f64 {
    libm::expm1(x)
}

#[inline]
pub(crate) fn ln(x: f64) -> f64 {
    libm::log(x)
}

#[inline]
pub(crate) fn sqrt(x: f64) -> f64 {
    libm::sqrt(x)
}

#[inline]
pub(crate) fn round(x: f64) -> f64 {
    libm::round(x)
}

/// Uniform in `[0, 1)` from the top 53 bits of one `u64`.
#[inline]
pub(crate) fn uniform(rng: &mut Xoshiro256PlusPlus) -> f64 {
    (rng.next_u64() >> 11) as f64 * U53
}

/// Uniform in `(0, 1]` — never zero, so it is safe to take its logarithm.
#[inline]
fn uniform_open(rng: &mut Xoshiro256PlusPlus) -> f64 {
    ((rng.next_u64() >> 11) + 1) as f64 * U53
}

/// Standard normal via Box–Muller. Uses two `u64` draws and returns only the
/// cosine branch (no cached spare, so no extra state to serialise).
#[inline]
pub(crate) fn normal(rng: &mut Xoshiro256PlusPlus) -> f64 {
    let u1 = uniform_open(rng);
    let u2 = uniform(rng);
    sqrt(-2.0 * ln(u1)) * libm::cos(TWO_PI * u2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::SeedableRng;

    #[test]
    fn uniform_in_range() {
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(1);
        for _ in 0..10_000 {
            let u = uniform(&mut rng);
            assert!((0.0..1.0).contains(&u));
            let v = uniform_open(&mut rng);
            assert!(v > 0.0 && v <= 1.0);
        }
    }

    #[test]
    fn normal_moments() {
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(7);
        let n = 200_000;
        let (mut s, mut s2) = (0.0, 0.0);
        for _ in 0..n {
            let z = normal(&mut rng);
            s += z;
            s2 += z * z;
        }
        let mean = s / n as f64;
        let var = s2 / n as f64 - mean * mean;
        assert!(mean.abs() < 0.01, "mean {mean}");
        assert!((var - 1.0).abs() < 0.02, "var {var}");
    }
}
