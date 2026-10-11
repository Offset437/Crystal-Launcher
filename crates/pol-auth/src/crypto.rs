//! Crystal's POL checksum and legacy cipher implementation.
//! MD5 follows RFC 1321. The POL key expansion deliberately reuses the
//! cleared digest context; P uses overlapping windows, and every table pair
//! is replaced by encryption of zero. OFB leaves CR/LF bytes unmodified.
//! These deviations are format requirements observed in installed POL core
//! 73b1864b, routines 10064300, 10064220, 10063ef0, and 10019c60.

use zeroize::{Zeroize, Zeroizing};

const STATE_WORDS: usize = 4;
const BLOCK_BYTES: usize = 64;
const P_WORDS: usize = 18;
const S_WORDS: usize = 1024;
const MD5_IV: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
const T: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];
const ROTATIONS: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];

fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let words = Zeroizing::new(std::array::from_fn::<_, 16, _>(|i| {
        u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap())
    }));
    let [mut a, mut b, mut c, mut d] = *state;
    for step in 0..64 {
        let group = step / 16;
        let (mix, index) = match group {
            0 => ((b & c) | (!b & d), step),
            1 => ((b & d) | (c & !d), (5 * step + 1) % 16),
            2 => (b ^ c ^ d, (3 * step + 5) % 16),
            _ => (c ^ (b | !d), (7 * step) % 16),
        };
        let next = b.wrapping_add(
            a.wrapping_add(mix)
                .wrapping_add(words[index])
                .wrapping_add(T[step])
                .rotate_left(ROTATIONS[group][step % 4]),
        );
        (a, b, c, d) = (d, next, b, c);
    }
    for (destination, value) in state.iter_mut().zip([a, b, c, d]) {
        *destination = destination.wrapping_add(value);
    }
    a.zeroize();
    b.zeroize();
    c.zeroize();
    d.zeroize();
}

struct Md5 {
    state: [u32; 4],
    len_bits: u64,
    buf: Zeroizing<[u8; 64]>,
    used: usize,
}

impl Md5 {
    fn new() -> Self {
        Self {
            state: MD5_IV,
            len_bits: 0,
            buf: Zeroizing::new([0; 64]),
            used: 0,
        }
    }

    fn update(&mut self, input: &[u8]) {
        self.len_bits = self
            .len_bits
            .wrapping_add((input.len() as u64).wrapping_mul(8));
        let mut remaining = input;
        if self.used != 0 {
            let fill = (64 - self.used).min(remaining.len());
            self.buf[self.used..self.used + fill].copy_from_slice(&remaining[..fill]);
            self.used += fill;
            remaining = &remaining[fill..];
            if self.used < 64 {
                return;
            }
            compress(&mut self.state, &self.buf);
            self.buf.zeroize();
            self.used = 0;
        }
        let mut blocks = remaining.chunks_exact(64);
        for block in &mut blocks {
            compress(&mut self.state, block.try_into().unwrap());
        }
        let tail = blocks.remainder();
        self.buf[..tail.len()].copy_from_slice(tail);
        self.used = tail.len();
    }

    fn finalize(&mut self) -> [u8; 16] {
        let mut end = Zeroizing::new([0; 128]);
        end[..self.used].copy_from_slice(&self.buf[..self.used]);
        end[self.used] = 0x80;
        let length = if self.used < 56 { 64 } else { 128 };
        end[length - 8..length].copy_from_slice(&self.len_bits.to_le_bytes());
        for block in end[..length].chunks_exact(64) {
            compress(&mut self.state, block.try_into().unwrap());
        }
        let mut digest = [0; 16];
        for (destination, word) in digest.chunks_exact_mut(4).zip(self.state) {
            destination.copy_from_slice(&word.to_le_bytes());
        }
        // POL reuses MD5 after finalization with a zero IV, not MD5_IV.
        self.state.zeroize();
        self.buf.zeroize();
        self.len_bits = 0;
        self.used = 0;
        digest
    }
}

impl Drop for Md5 {
    fn drop(&mut self) {
        self.state.zeroize();
        self.len_bits.zeroize();
        self.used.zeroize();
    }
}

pub fn md5(bytes: &[u8]) -> [u8; 16] {
    md5_parts(&[bytes])
}

pub(crate) fn md5_parts(parts: &[&[u8]]) -> [u8; 16] {
    let mut hash = Md5::new();
    for part in parts {
        hash.update(part);
    }
    hash.finalize()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamReset {
    Boundary,
    Continue,
}

#[derive(Clone)]
pub struct PolBlowfish {
    p: [u32; P_WORDS],
    s: [u32; S_WORDS],
    seed: [u32; 2],
    feedback: [u32; 2],
    cursor: usize,
}

impl PolBlowfish {
    pub fn new(key: &[u8], iv: (u32, u32)) -> Self {
        assert!(key.len() < 4096, "POL session key exceeds expansion buffer");
        let mut expansion = Zeroizing::new([0; 4096]);
        expansion[..key.len()].copy_from_slice(key);
        let mut written = key.len();
        let mut digest = Md5::new();
        while written < expansion.len() {
            digest.update(&expansion[..written]);
            let block = Zeroizing::new(digest.finalize());
            let count = block.len().min(expansion.len() - written);
            expansion[written..written + count].copy_from_slice(&block[..count]);
            written += count;
        }
        let s = std::array::from_fn(|i| {
            u32::from_be_bytes(expansion[i * 4..i * 4 + 4].try_into().unwrap())
        });
        let p = std::array::from_fn(|i| {
            u32::from_be_bytes(expansion[i..i + 4].try_into().unwrap()) ^ s[i]
        });
        let mut cipher = Self {
            p,
            s,
            seed: [iv.0, iv.1],
            feedback: [0; 2],
            cursor: 0,
        };
        // Table updates are ordered: each encryption sees previous updates.
        for pair in 0..(P_WORDS + S_WORDS) / 2 {
            let result = cipher.encrypt_block(0, 0);
            let (table, position): (&mut [u32], usize) = if pair < P_WORDS / 2 {
                (&mut cipher.p, pair * 2)
            } else {
                (&mut cipher.s, pair * 2 - P_WORDS)
            };
            table[position..position + 2].copy_from_slice(&[result.0, result.1]);
        }
        cipher
    }

    fn round(&self, word: u32) -> u32 {
        let bytes = word.to_be_bytes();
        let mut value = self.s[bytes[0] as usize].wrapping_add(self.s[256 + bytes[1] as usize]);
        value ^= self.s[512 + bytes[2] as usize];
        value.wrapping_add(self.s[768 + bytes[3] as usize])
    }

    pub fn encrypt_block(&self, left: u32, right: u32) -> (u32, u32) {
        let mut left = left ^ self.p[0];
        let mut right = right;
        for step in (1..17).step_by(2) {
            right ^= self.round(left) ^ self.p[step];
            left ^= self.round(right) ^ self.p[step + 1];
        }
        (right ^ self.p[17], left)
    }

    pub fn stream(&mut self, bytes: &[u8], reset: StreamReset) -> Vec<u8> {
        if reset == StreamReset::Boundary {
            self.feedback = self.seed;
            self.cursor = 0;
        }
        let mut output = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.cursor % 8 == 0 {
                let next = self.encrypt_block(self.feedback[0], self.feedback[1]);
                self.feedback = [next.0, next.1];
                self.cursor = 0;
            }
            let pad = (self.feedback[self.cursor / 4] >> (8 * (self.cursor % 4))) as u8;
            self.cursor += 1;
            let transformed = byte ^ pad;
            output.push(
                if matches!(byte, b'\r' | b'\n') || matches!(transformed, b'\r' | b'\n') {
                    byte
                } else {
                    transformed
                },
            );
        }
        output
    }
}

impl Drop for PolBlowfish {
    fn drop(&mut self) {
        self.p.zeroize();
        self.s.zeroize();
        self.seed.zeroize();
        self.feedback.zeroize();
        self.cursor.zeroize();
    }
}

pub fn checksum(bytes: &[u8], seed: u32) -> u32 {
    let mut words = bytes.chunks_exact(4);
    let sum = words.by_ref().fold(seed, |sum, word| {
        sum.wrapping_add(u32::from_le_bytes(word.try_into().unwrap()))
    });
    let remaining = words.remainder();
    let mut last = [0; 4];
    last[4 - remaining.len()..].copy_from_slice(remaining);
    sum.wrapping_add(u32::from_le_bytes(last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::Digest;

    // Independent Python/hashlib fixtures; no live account or server input.
    const VECTORS: &str = include_str!("fixtures/crypto.tsv");

    fn rows(tag: &str) -> Vec<Vec<&'static str>> {
        VECTORS
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .filter(|row| row[0] == tag)
            .collect()
    }

    fn sample(length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| (index * 73 + index / 11 + 167) as u8)
            .collect()
    }

    fn cipher(row: &[&str]) -> PolBlowfish {
        PolBlowfish::new(
            &hex::decode(row[1]).unwrap(),
            (row[2].parse().unwrap(), row[3].parse().unwrap()),
        )
    }

    #[test]
    fn digest_padding_and_fragmentation_match_hashlib_at_block_boundaries() {
        let vectors = rows("MD5");
        assert!(!vectors.is_empty());
        for row in vectors {
            let input = sample(row[1].parse().unwrap());
            let digest: [u8; 16] = hex::decode(row[2]).unwrap().try_into().unwrap();
            assert_eq!(md5(&input), digest, "length {}", input.len());
            for width in [1, 13, 55, 64, 97, 511] {
                let fragments: Vec<_> = input.chunks(width).collect();
                assert_eq!(
                    md5_parts(&fragments),
                    digest,
                    "length {}, fragment {width}",
                    input.len()
                );
            }
        }
    }

    #[test]
    fn finalized_digest_context_is_wiped_and_reuses_the_native_zero_iv() {
        for row in rows("ZERO_MD5") {
            let mut context = Md5::new();
            context.update(&sample(37));
            context.finalize();
            assert_eq!(context.state, [0; 4]);
            assert_eq!(*context.buf, [0; 64]);
            assert_eq!(context.used, 0);
            assert_eq!(context.len_bits, 0);
            context.update(&sample(row[1].parse().unwrap()));
            assert_eq!(hex::encode(context.finalize()), row[2]);
            assert_eq!(context.state, [0; 4]);
            assert_eq!(*context.buf, [0; 64]);
        }
    }

    #[test]
    fn full_key_schedule_and_block_outputs_match_the_separate_feistel_oracle() {
        for row in rows("CIPHER") {
            let cipher = cipher(&row);
            let schedule: Vec<_> = cipher
                .p
                .iter()
                .chain(&cipher.s)
                .flat_map(|word| word.to_le_bytes())
                .collect();
            assert_eq!(hex::encode(sha1::Sha1::digest(&schedule)), row[8]);
            assert_eq!(
                cipher.encrypt_block(row[4].parse().unwrap(), row[5].parse().unwrap()),
                (row[6].parse().unwrap(), row[7].parse().unwrap())
            );
        }
    }

    #[test]
    fn every_stream_split_preserves_reference_bytes_and_message_resets() {
        for row in rows("CIPHER") {
            let initial = cipher(&row);
            let message = hex::decode(row[9]).unwrap();
            let expected = hex::decode(row[10]).unwrap();
            assert_eq!(
                initial.clone().stream(&message, StreamReset::Boundary),
                expected
            );
            assert_eq!(
                initial.clone().stream(&expected, StreamReset::Boundary),
                message
            );
            for split in 0..=message.len() {
                let mut fragmented = initial.clone();
                let mut actual = fragmented.stream(&message[..split], StreamReset::Boundary);
                actual.extend(fragmented.stream(&message[split..], StreamReset::Continue));
                assert_eq!(actual, expected, "split {split}");
                assert_eq!(fragmented.stream(&message, StreamReset::Boundary), expected);
            }
            for (position, &original) in message.iter().enumerate() {
                if matches!(original, 10 | 13) {
                    assert_eq!(expected[position], original);
                }
            }
        }
    }

    #[test]
    fn checksum_matches_wide_positional_arithmetic_for_all_tail_sizes() {
        let bytes = sample(277);
        for length in 0..=bytes.len() {
            let whole = length / 4 * 4;
            for seed in [0, u32::MAX, 0x7abc321d] {
                let value =
                    bytes[..length]
                        .iter()
                        .enumerate()
                        .fold(seed as u128, |sum, (index, &byte)| {
                            let position = if index < whole {
                                index % 4
                            } else {
                                4 - length % 4 + index - whole
                            };
                            sum + ((byte as u128) << (position * 8))
                        });
                assert_eq!(
                    checksum(&bytes[..length], seed),
                    value as u32,
                    "length {length}"
                );
            }
        }
    }

    #[test]
    fn expansion_buffer_limit_rejects_an_oversized_key() {
        assert!(
            std::panic::catch_unwind(|| PolBlowfish::new(&vec![0x9d; 4096], (17, 23))).is_err()
        );
    }
}
