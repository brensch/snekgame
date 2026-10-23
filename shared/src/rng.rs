//! The game's only source of randomness, small enough to be identical on
//! every platform.

/// Small deterministic PRNG (xorshift64*). The server seeds each game; the
/// engine is fully deterministic given the seed + move stream, so a stored
/// game can be re-simulated from its inputs if ever needed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator for this seed. Equal seeds give equal streams forever.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        // Avoid the all-zero state.
        Self(seed | 1)
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, n). Returns 0 when n is 0, though callers never ask
    /// for a pick from nothing.
    pub fn below(&mut self, n: usize) -> usize {
        // usize is at most 64 bits on every target we build for, and the
        // remainder is below n, so both conversions always succeed.
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        let r = self.next_u64().checked_rem(n).unwrap_or(0);
        usize::try_from(r).unwrap_or(0)
    }

    /// True with probability p (0.0..=1.0).
    #[expect(
        clippy::cast_precision_loss,
        reason = "53 bits, exactly an f64's mantissa: both conversions are exact"
    )]
    pub fn chance(&mut self, p: f64) -> bool {
        // uniform [0,1)
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}
