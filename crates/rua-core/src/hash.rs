//! A small, fast hasher for table keys.
//!
//! The standard library's SipHash is a good default for a general purpose map:
//! it resists collision attacks on untrusted keys. Table keys here are program
//! identifiers and small values, and hashing them showed up as 9% of run time,
//! so this uses the multiply-rotate hash rustc uses internally instead.

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while rest.len() >= 8 {
            let (head, tail) = rest.split_at(8);
            self.add(u64::from_ne_bytes(head.try_into().expect("8 bytes")));
            rest = tail;
        }
        if rest.len() >= 4 {
            let (head, tail) = rest.split_at(4);
            self.add(u32::from_ne_bytes(head.try_into().expect("4 bytes")) as u64);
            rest = tail;
        }
        for b in rest {
            self.add(*b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n as u64);
    }
    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n as u64);
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }
    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuild = BuildHasherDefault<FxHasher>;
pub type FxMap<K, V> = std::collections::HashMap<K, V, FxBuild>;

/// The hash of a string, computed once when the string is made.
///
/// Every table lookup with a string key used to hash the bytes again. Symbol
/// heavy programs — an interpreter written in rua, say — do that in their
/// inner loop, so `RStr` carries the result instead.
pub fn str_hash(s: &str) -> u64 {
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.write_u8(0xff);
    h.finish()
}
