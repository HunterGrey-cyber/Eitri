//! A tiny FNV-1a so the golden file needs no hashing dependency.
#![allow(dead_code)]

pub const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x1000_0000_01b3;

#[derive(Clone, Copy)]
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(OFFSET)
    }
}

impl Fnv {
    pub fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(PRIME);
        }
    }

    pub fn hex(self) -> String {
        format!("{:016x}", self.0)
    }
}
