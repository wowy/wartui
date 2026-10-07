//! Test helpers shared by the test binaries.

/// The multiplicative hash `MacIndex` builds, the index behind both `MacRing` and `Pending`.
/// Used only to find addresses that collide.
pub fn hash_for_test(address: &[u8; 6], bits: u32) -> usize {
    let hi = u32::from_be_bytes([address[0], address[1], address[2], address[3]]);
    let lo = u32::from_be_bytes([0, 0, address[4], address[5]]);
    let h = (hi ^ lo).wrapping_mul(0x9E37_79B1);
    (h >> (32 - bits)) as usize
}

/// A minimal deterministic PRNG (MMIX's LCG), so the random tests are reproducible
/// without a new dependency.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }
}
