//! Hashing for maps keyed by guest addresses and API ids: one multiply (the default
//! SipHash showed up at ~9 % of run time in function lookups).

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
        self.0 = (v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

pub type AddrHash = std::hash::BuildHasherDefault<AddrHasher>;

/// Maps and sets with a fixed hasher, used throughout the runtime: their iteration
/// order then depends only on what was inserted, so HLE code that iterates them
/// (timers, windows, objects) behaves the same in every host. std's default hasher
/// is seeded randomly on native targets but not on wasm32-unknown-unknown, which made
/// harness runs differ from browser runs and from each other.
pub type FixedHash = std::hash::BuildHasherDefault<std::collections::hash_map::DefaultHasher>;
pub type HashMap<K, V, S = FixedHash> = std::collections::HashMap<K, V, S>;
pub type HashSet<K, S = FixedHash> = std::collections::HashSet<K, S>;
