//! The simulator's one source of chance: xoshiro256** seeded by SplitMix64,
//! so that a seed gives the same run on every machine and build.

use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Rng([u64; 4]);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        Rng([next(), next(), next(), next()])
    }

    pub fn u64(&mut self) -> u64 {
        let s = &mut self.0;
        let out = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        out
    }

    /// A number in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        ((self.u64() as u128 * n as u128) >> 64) as u64
    }

    /// A number in `a..=b`.
    pub fn between(&mut self, a: u64, b: u64) -> u64 {
        a + self.below(b - a + 1)
    }

    /// True with probability `p`, in parts per million.
    pub fn ppm(&mut self, p: u64) -> bool {
        self.below(1_000_000) < p
    }

    /// A random part of `d`, as `jitter` in [`crate::service`] takes it.
    pub fn part_of(&mut self, d: Duration) -> Duration {
        d.mul_f64(self.below(1_000_000) as f64 / 1_000_000.0)
    }

    /// A duration in `a..=b` milliseconds.
    pub fn ms(&mut self, a: u64, b: u64) -> Duration {
        Duration::from_millis(self.between(a, b))
    }

    pub fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.below(of.len() as u64) as usize]
    }

    /// A new generator drawn from this one, for a part of the run whose
    /// draws must not shift the others'.
    pub fn fork(&mut self) -> Rng {
        Rng::new(self.u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_gives_the_same_numbers() {
        let a: Vec<u64> = { let mut r = Rng::new(7); (0..5).map(|_| r.u64()).collect() };
        let b: Vec<u64> = { let mut r = Rng::new(7); (0..5).map(|_| r.u64()).collect() };
        assert_eq!(a, b);
        assert_ne!(a, { let mut r = Rng::new(8); (0..5).map(|_| r.u64()).collect::<Vec<_>>() });
        let mut r = Rng::new(1);
        assert!((0..1000).all(|_| r.below(10) < 10));
        assert!((0..1000).map(|_| r.between(3, 5)).all(|x| (3..=5).contains(&x)));
    }
}
