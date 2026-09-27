//! Maps keyed by guest addresses: one multiply per key (std's SipHash showed up at ~5 %
//! of translation time).

#[derive(Default, Clone, Copy)]
pub struct AddrHasher(u64);

impl std::hash::Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0.rotate_left(32) ^ v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn write_u8(&mut self, v: u8) {
        self.0 = (self.0.rotate_left(8) ^ v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

pub type AddrHash = std::hash::BuildHasherDefault<AddrHasher>;
pub type HashMap<K, V> = std::collections::HashMap<K, V, AddrHash>;
