//! How clean a captured tone is: what is left once a sine is taken off.
//!
//! The residual is measured in short windows. In each, a sine at the
//! tone's frequency is fitted whose amplitude and phase may move as a
//! polynomial in time. Degree 0 is a steady sine. A higher degree also
//! takes off a slow drift of the frequency, such as an adaptive resampler
//! makes while it follows a clock, and leaves the distortion and noise.

use std::f64::consts::TAU;

/// The RMS of a full-scale sine, as the reference of every level here.
const FULL_SCALE_RMS: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// The residual after the fitted sine, as RMS in dB below a full-scale
/// sine. `samples` are on a scale of 1.0 for full scale; `omega` is the
/// tone's frequency in radians per sample.
pub fn residual_dbfs(samples: &[f64], omega: f64, degree: usize) -> f64 {
    let rms = residual_rms(samples, omega, degree);
    20.0 * (rms / FULL_SCALE_RMS).log10()
}

/// The RMS of what is left after the fit.
pub fn residual_rms(samples: &[f64], omega: f64, degree: usize) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    // Columns: a constant, then t^k sin and t^k cos for k up to the degree,
    // with t from -1 to 1 over the window to keep the system well scaled.
    let terms = 1 + 2 * (degree + 1);
    let basis = |i: usize, row: &mut [f64]| {
        let t = if n > 1 {
            2.0 * i as f64 / (n - 1) as f64 - 1.0
        } else {
            0.0
        };
        let (s, c) = (omega * i as f64).sin_cos();
        row[0] = 1.0;
        let mut p = 1.0;
        for k in 0..=degree {
            row[1 + 2 * k] = p * s;
            row[2 + 2 * k] = p * c;
            p *= t;
        }
    };
    let mut ata = vec![0.0; terms * terms];
    let mut aty = vec![0.0; terms];
    let mut row = vec![0.0; terms];
    for (i, &y) in samples.iter().enumerate() {
        basis(i, &mut row);
        for r in 0..terms {
            aty[r] += row[r] * y;
            for c in 0..terms {
                ata[r * terms + c] += row[r] * row[c];
            }
        }
    }
    let Some(x) = solve(&mut ata, &mut aty, terms) else {
        return f64::NAN;
    };
    let mut err = 0.0;
    for (i, &y) in samples.iter().enumerate() {
        basis(i, &mut row);
        let fit: f64 = row.iter().zip(&x).map(|(a, b)| a * b).sum();
        err += (y - fit).powi(2);
    }
    (err / n as f64).sqrt()
}

/// Solves `a x = b` by Gaussian elimination with partial pivoting.
fn solve(a: &mut [f64], b: &mut [f64], n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let pivot =
            (col..n).max_by(|&i, &j| a[i * n + col].abs().total_cmp(&a[j * n + col].abs()))?;
        if a[pivot * n + col].abs() < 1e-300 {
            return None;
        }
        if pivot != col {
            for k in 0..n {
                a.swap(pivot * n + k, col * n + k);
            }
            b.swap(pivot, col);
        }
        for r in col + 1..n {
            let f = a[r * n + col] / a[col * n + col];
            for k in col..n {
                a[r * n + k] -= f * a[col * n + k];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = b[r];
        for k in r + 1..n {
            s -= a[r * n + k] * x[k];
        }
        x[r] = s / a[r * n + r];
    }
    Some(x)
}

/// The tone's frequency in radians per sample, fitted to `samples` by a
/// search for the least residual of a steady sine around `guess`, within
/// `span` radians per sample either side.
pub fn fit_omega(samples: &[f64], guess: f64, span: f64) -> f64 {
    // Golden-section search; the residual has one minimum this close.
    let ratio = (5f64.sqrt() - 1.0) / 2.0;
    let (mut lo, mut hi) = (guess - span, guess + span);
    let cost = |w: f64| residual_rms(samples, w, 0);
    let mut x1 = hi - ratio * (hi - lo);
    let mut x2 = lo + ratio * (hi - lo);
    let (mut f1, mut f2) = (cost(x1), cost(x2));
    for _ in 0..60 {
        if f1 < f2 {
            hi = x2;
            x2 = x1;
            f2 = f1;
            x1 = hi - ratio * (hi - lo);
            f1 = cost(x1);
        } else {
            lo = x1;
            x1 = x2;
            f1 = f2;
            x2 = lo + ratio * (hi - lo);
            f2 = cost(x2);
        }
    }
    0.5 * (lo + hi)
}

/// Radians per sample of `hz` at `rate`.
pub fn omega(hz: f64, rate: f64) -> f64 {
    TAU * hz / rate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, w: f64, amplitude: f64) -> Vec<f64> {
        (0..n)
            .map(|i| amplitude * (w * i as f64 + 0.3).sin())
            .collect()
    }

    #[test]
    fn a_pure_tone_leaves_nothing() {
        let w = omega(997.0, 48_000.0);
        let x = tone(4800, w, 0.5);
        assert!(residual_dbfs(&x, w, 0) < -200.0);
    }

    #[test]
    fn added_noise_is_what_is_left() {
        let w = omega(997.0, 48_000.0);
        let mut x = tone(4800, w, 0.5);
        // A second tone far from the first, at -80 dB of full scale RMS.
        let level = 1e-4;
        for (i, s) in x.iter_mut().enumerate() {
            *s += level * (omega(5000.0, 48_000.0) * i as f64).sin();
        }
        let r = residual_dbfs(&x, w, 0);
        assert!((r + 80.0).abs() < 0.5, "{r}");
    }

    #[test]
    fn a_slow_drift_needs_a_moving_sine() {
        // 0.2 Hz off the assumed frequency.
        let w = omega(997.0, 48_000.0);
        let x = tone(4800, omega(997.2, 48_000.0), 0.5);
        assert!(residual_dbfs(&x, w, 0) > -60.0);
        assert!(residual_dbfs(&x, w, 3) < -120.0);
    }

    #[test]
    fn the_frequency_is_found() {
        let truth = omega(997.03, 48_000.0);
        let x = tone(48_000, truth, 0.5);
        let found = fit_omega(&x, omega(997.0, 48_000.0), omega(1.0, 48_000.0));
        assert!((found - truth).abs() < 1e-9, "{found} {truth}");
    }
}
