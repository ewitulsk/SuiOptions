//! Seeded, dependency-free randomness for the flow generator (doc 08
//! §8.7): PCG32 streams, keyed sub-streams for common random numbers,
//! and the inverse-CDF samplers the generator uses so every draw is a
//! monotone function of one uniform. Same seed ⇒ identical draws;
//! parameter variants share the uniforms and differ only in how they
//! are transformed.

/// PCG32 (O'Neill, `pcg32_random_r`): 64-bit state, 32-bit output.
#[derive(Clone, Debug)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

const MULT: u64 = 6_364_136_223_846_793_005;

impl Pcg32 {
    /// One generator per `(seed, stream)`; distinct streams are
    /// statistically independent sequences of the same seed.
    pub fn new(seed: u64, stream: u64) -> Self {
        let mut g = Self { state: 0, inc: (stream << 1) | 1 };
        g.next_u32();
        g.state = g.state.wrapping_add(seed);
        g.next_u32();
        g
    }

    /// A sub-stream keyed by arbitrary bytes: the common-random-numbers
    /// handle. Everything about "the k-th call RFQ of minute m" is drawn
    /// from `Pcg32::keyed(seed, &[m, type, k])`, so a variant that
    /// changes the bid or the arrival rate still sees the same writer.
    pub fn keyed(seed: u64, key: &[u64]) -> Self {
        let mut bytes = Vec::with_capacity(key.len() * 8);
        for k in key {
            bytes.extend_from_slice(&k.to_le_bytes());
        }
        let h = crate::fnv1a(&bytes);
        // Different key → different stream AND different seed offset.
        Self::new(seed ^ h.rotate_left(17), h >> 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULT).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// Uniform on the open interval (0, 1), 32 bits of resolution.
    pub fn uniform(&mut self) -> f64 {
        (self.next_u32() as f64 + 0.5) / 4_294_967_296.0
    }

    /// Standard normal by inverse CDF of one uniform (monotone — keeps
    /// the common-random-numbers property that Box–Muller would break).
    pub fn normal(&mut self) -> f64 {
        inverse_normal(self.uniform())
    }

    /// Lognormal with the given median and log-sd.
    pub fn lognormal(&mut self, median: f64, log_sd: f64) -> f64 {
        median * (log_sd * self.normal()).exp()
    }
}

/// Acklam's rational approximation of Φ⁻¹, |rel err| < 1.2e-9.
pub fn inverse_normal(p: f64) -> f64 {
    debug_assert!(p > 0.0 && p < 1.0);
    const A: [f64; 6] = [
        -3.969683028665376e+01, 2.209460984245205e+02, -2.759285104469687e+02,
        1.38357751867269e+02, -3.066479806614716e+01, 2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01, 1.615858368580409e+02, -1.556989798598866e+02,
        6.680131188771972e+01, -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e+00,
        -2.549732539343734e+00, 4.374664141464968e+00, 2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e+00, 3.754408661907416e+00,
    ];
    const P_LOW: f64 = 0.02425;
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - P_LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

/// Poisson count by CDF inversion of one uniform: monotone in `lambda`,
/// so a variant with a higher arrival rate sees a superset of arrivals.
/// Large means use the normal approximation (also monotone).
pub fn poisson_inverse(u: f64, lambda: f64) -> u32 {
    if lambda <= 0.0 {
        return 0;
    }
    if lambda > 400.0 {
        let k = lambda + lambda.sqrt() * inverse_normal(u);
        return k.round().max(0.0) as u32;
    }
    let mut p = (-lambda).exp();
    let mut cdf = p;
    let mut k = 0u32;
    while u > cdf && k < 100_000 {
        k += 1;
        p *= lambda / k as f64;
        cdf += p;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcg_is_deterministic_and_streams_differ() {
        let mut a = Pcg32::new(42, 1);
        let mut b = Pcg32::new(42, 1);
        let mut c = Pcg32::new(42, 2);
        let xa: Vec<u32> = (0..8).map(|_| a.next_u32()).collect();
        let xb: Vec<u32> = (0..8).map(|_| b.next_u32()).collect();
        let xc: Vec<u32> = (0..8).map(|_| c.next_u32()).collect();
        assert_eq!(xa, xb);
        assert_ne!(xa, xc);
        let mut k1 = Pcg32::keyed(7, &[1, 0, 3]);
        let mut k2 = Pcg32::keyed(7, &[1, 0, 3]);
        let mut k3 = Pcg32::keyed(7, &[1, 1, 3]);
        assert_eq!(k1.uniform(), k2.uniform());
        assert_ne!(k1.uniform(), k3.uniform());
    }

    #[test]
    fn samplers_have_the_right_moments() {
        let mut g = Pcg32::new(1, 9);
        let n = 20_000;
        let (mut s, mut s2) = (0.0, 0.0);
        for _ in 0..n {
            let z = g.normal();
            s += z;
            s2 += z * z;
        }
        let mean = s / n as f64;
        let var = s2 / n as f64 - mean * mean;
        assert!(mean.abs() < 0.03, "{mean}");
        assert!((var - 1.0).abs() < 0.05, "{var}");
        assert!((inverse_normal(0.975) - 1.959964).abs() < 1e-5);
        let counts: u64 = (0..n).map(|_| poisson_inverse(g.uniform(), 3.5) as u64).sum();
        let lam_hat = counts as f64 / n as f64;
        assert!((lam_hat - 3.5).abs() < 0.08, "{lam_hat}");
        // Monotone in lambda for a fixed uniform.
        for u in [0.05, 0.5, 0.95] {
            assert!(poisson_inverse(u, 2.0) <= poisson_inverse(u, 4.0));
            assert!(poisson_inverse(u, 300.0) <= poisson_inverse(u, 500.0));
        }
    }
}
