//! Runtime CPU dispatch for hot kernels and small vectorizable math.
//!
//! Kernels are written as plain loops over contiguous slices (fixed-width
//! chunks, no calls inside) so LLVM autovectorizes them; [`avx2_dispatch!`]
//! compiles a second copy with AVX2 enabled and picks it at run time. No
//! fused multiply-add is used, so both copies give bit-identical results.

/// Defines `fn $name(args) -> R` that calls `$imp(args)` compiled with AVX2
/// when the CPU supports it, and the baseline build otherwise.
macro_rules! avx2_dispatch {
    ($(#[$m:meta])* $vis:vis fn $name:ident($($a:ident : $t:ty),* $(,)?) $(-> $r:ty)? => $imp:ident) => {
        $(#[$m])*
        #[allow(clippy::too_many_arguments)]
        $vis fn $name($($a: $t),*) $(-> $r)? {
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2")]
                #[allow(clippy::too_many_arguments)]
                unsafe fn avx2($($a: $t),*) $(-> $r)? {
                    $imp($($a),*)
                }
                if std::arch::is_x86_feature_detected!("avx2") {
                    // SAFETY: the CPU supports AVX2 (checked just above).
                    return unsafe { avx2($($a),*) };
                }
            }
            $imp($($a),*)
        }
    };
}
pub(crate) use avx2_dispatch;

use std::f32::consts::{FRAC_PI_2, PI};

/// Rounds to the nearest integer without libm (`round` is a call on baseline x86-64).
#[inline(always)]
fn round_i(x: f32) -> i32 {
    ((x + 0.5 + 4096.0) as i32) - 4096
}

/// atan2 with about 1e-7 rad error, branch-free (selects only).
#[inline(always)]
pub fn atan2(y: f32, x: f32) -> f32 {
    const TAN_PI_12: f32 = 0.267_949_2;
    const SQRT3: f32 = 1.732_050_8;
    let (ax, ay) = (x.abs(), y.abs());
    let mx = ax.max(ay);
    let mn = ax.min(ay);
    let t = if mx > 0.0 { mn / mx } else { 0.0 };
    // atan(t) = pi/6 + atan((t sqrt3 - 1) / (sqrt3 + t)) for t > tan(pi/12).
    let big = t > TAN_PI_12;
    let u = if big { (t * SQRT3 - 1.0) / (SQRT3 + t) } else { t };
    let z = u * u;
    let p = u * (1.0 + z * (-1.0 / 3.0 + z * (1.0 / 5.0 + z * (-1.0 / 7.0 + z * (1.0 / 9.0 - z * (1.0 / 11.0))))));
    let mut r = if big { p + PI / 6.0 } else { p };
    r = if ay > ax { FRAC_PI_2 - r } else { r };
    r = if x < 0.0 { PI - r } else { r };
    if y < 0.0 {
        -r
    } else {
        r
    }
}

/// (sin x, cos x) for |x| up to a few thousand, about 1e-7 absolute error.
#[inline(always)]
pub fn sin_cos(x: f32) -> (f32, f32) {
    // Cody-Waite reduction by pi/2 in two parts.
    const P1: f32 = 1.570_312_5;
    const P2: f32 = 4.838_268e-4;
    let k = round_i(x * (2.0 / PI));
    let kf = k as f32;
    let r = (x - kf * P1) - kf * P2;
    let z = r * r;
    let s = r * (1.0 + z * (-1.0 / 6.0 + z * (1.0 / 120.0 + z * (-1.0 / 5040.0 + z * (1.0 / 362_880.0)))));
    let c = 1.0 + z * (-0.5 + z * (1.0 / 24.0 + z * (-1.0 / 720.0 + z * (1.0 / 40320.0 - z * (1.0 / 3_628_800.0)))));
    // Quadrant q: sin = [s, c, -s, -c][q], cos = [c, -s, -c, s][q].
    let q = k & 3;
    let swap = q & 1 == 1;
    let (sv, cv) = if swap { (c, s) } else { (s, c) };
    let sin = if q & 2 != 0 { -sv } else { sv };
    let cos = if (q + 1) & 2 != 0 { -cv } else { cv };
    (sin, cos)
}

/// exp(x) for x <= 0 (0 below -87), about 2e-7 relative error.
#[inline(always)]
pub fn exp_neg(x: f32) -> f32 {
    const LN2_HI: f32 = 0.693_359_4;
    const LN2_LO: f32 = -2.121_944_4e-4;
    let x = x.max(-87.0);
    let n = round_i(x * std::f32::consts::LOG2_E);
    let nf = n as f32;
    let r = (x - nf * LN2_HI) - nf * LN2_LO;
    let p = 1.0 + r * (1.0 + r * (0.5 + r * (1.0 / 6.0 + r * (1.0 / 24.0 + r * (1.0 / 120.0 + r * (1.0 / 720.0))))));
    p * f32::from_bits(((n + 127).max(1) as u32) << 23)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_math_accuracy() {
        let mut worst = (0f32, 0f32, 0f32);
        for i in -2000i32..=2000 {
            for j in [-3.0f32, -0.7, -0.01, 0.0, 0.02, 0.5, 4.0] {
                let (y, x) = (i as f32 / 300.0, j);
                worst.0 = worst.0.max((atan2(y, x) - y.atan2(x)).abs());
            }
            let x = i as f32 / 100.0;
            let (s, c) = sin_cos(x);
            worst.1 = worst.1.max((s - x.sin()).abs()).max((c - x.cos()).abs());
            let e = -(i.abs() as f32) / 40.0;
            worst.2 = worst.2.max((exp_neg(e) - e.exp()).abs() / e.exp().max(1e-30));
        }
        assert!(worst.0 < 4e-7, "atan2 {}", worst.0);
        assert!(worst.1 < 4e-7, "sin_cos {}", worst.1);
        assert!(worst.2 < 1e-6, "exp {}", worst.2);
        assert_eq!(atan2(0.0, 0.0), 0.0);
        assert!((atan2(0.0, -1.0) - PI).abs() < 1e-6);
    }
}
