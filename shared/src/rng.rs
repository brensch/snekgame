/// Small deterministic PRNG (xorshift64*). The server seeds each game; the
/// engine is fully deterministic given the seed + move stream, so a stored
/// game can be re-simulated from its inputs if ever needed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state.
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform in [0, n). n must be > 0.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability p (0.0..=1.0).
    pub fn chance(&mut self, p: f64) -> bool {
        // uniform [0,1)
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}
