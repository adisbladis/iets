//! Positions of the bytes a string printer has to escape, 16 at a time.
//!
//! Builder scripts have a newline every ~20 bytes, so the iterator walks
//! every hit of a block from one mask instead of rescanning after each.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_min_epu8, _mm_movemask_epi8, _mm_or_si128,
    _mm_set1_epi8,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Escapes {
    /// `"`, `\`, `\t`, `\n`, `\r` (derivations.cc:printString)
    Aterm,
    /// `"`, `\` and every byte below 0x20
    Json,
}

impl Escapes {
    fn hit(self, b: u8) -> bool {
        match self {
            Escapes::Aterm => matches!(b, b'"' | b'\\' | b'\t' | b'\n' | b'\r'),
            Escapes::Json => b < 0x20 || b == b'"' || b == b'\\',
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn mask(self, block: &[u8; 16]) -> u32 {
        // SAFETY: SSE2 is part of x86_64; the load is unaligned and reads
        // exactly the 16 bytes of `block`.
        unsafe {
            let x = _mm_loadu_si128(block.as_ptr().cast::<__m128i>());
            let eq = |c: u8| _mm_cmpeq_epi8(x, _mm_set1_epi8(c as i8));
            let quote = _mm_or_si128(eq(b'"'), eq(b'\\'));
            let m = match self {
                Escapes::Aterm => _mm_or_si128(
                    quote,
                    _mm_or_si128(eq(b'\t'), _mm_or_si128(eq(b'\n'), eq(b'\r'))),
                ),
                // unsigned x < 0x20 iff min(x, 0x1f) == x
                Escapes::Json => _mm_or_si128(
                    quote,
                    _mm_cmpeq_epi8(_mm_min_epu8(x, _mm_set1_epi8(0x1f)), x),
                ),
            };
            _mm_movemask_epi8(m) as u32
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn mask(self, block: &[u8; 16]) -> u32 {
        block
            .iter()
            .enumerate()
            .fold(0, |m, (i, &b)| m | (u32::from(self.hit(b)) << i))
    }

    /// Ascending positions in `s` of the bytes to escape.
    pub fn positions(self, s: &[u8]) -> Positions<'_> {
        Positions {
            s,
            set: self,
            next: 0,
            base: 0,
            bits: 0,
        }
    }
}

pub struct Positions<'a> {
    s: &'a [u8],
    set: Escapes,
    /// start of the first byte not yet looked at
    next: usize,
    /// start of the block `bits` belongs to
    base: usize,
    bits: u32,
}

impl Iterator for Positions<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.bits != 0 {
                let i = self.base + self.bits.trailing_zeros() as usize;
                self.bits &= self.bits - 1;
                return Some(i);
            }
            let rest = self.s.get(self.next..)?;
            if let Some(block) = rest.first_chunk::<16>() {
                self.bits = self.set.mask(block);
                self.base = self.next;
                self.next += 16;
                continue;
            }
            let Some(i) = rest.iter().position(|&b| self.set.hit(b)) else {
                self.next = self.s.len();
                return None;
            };
            let i = self.next + i;
            self.next = i + 1;
            return Some(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Escapes;
    use rstest::rstest;

    fn slow(set: Escapes, s: &[u8]) -> Vec<usize> {
        (0..s.len()).filter(|&i| set.hit(s[i])).collect()
    }

    #[rstest]
    fn finds_every_byte_to_escape(#[values(Escapes::Aterm, Escapes::Json)] set: Escapes) {
        let mut s = [b'a'; 40];
        for at in 0..s.len() {
            for b in 0..=255u8 {
                s[at] = b;
                for end in at..=s.len() {
                    let got: Vec<usize> = set.positions(&s[..end]).collect();
                    assert_eq!(got, slow(set, &s[..end]), "{b:#x} at {at}");
                }
            }
            s[at] = 0xff;
        }
        let mut x = 1u64;
        for _ in 0..10_000 {
            let s: Vec<u8> = (0..x % 70)
                .map(|_| {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    // mostly bytes around the escaped ones
                    (x >> 59) as u8 * 5 + b'\t'
                })
                .collect();
            assert_eq!(set.positions(&s).collect::<Vec<_>>(), slow(set, &s));
        }
    }
}
