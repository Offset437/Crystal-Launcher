//! Crystal's legacy chat RSA and custom alphabet codecs.
//! POL sends a 32-byte little-endian modulus with exponent 65535. Numeric
//! 300 contains a big-endian RSA block; its first eight message bytes become
//! the session key in reverse order. Format evidence: installed POL core
//! 73b1864b, 1002bb40, 10016182, 1002bde0, and 10065de0.

use num_bigint::{BigInt, BigUint};
use num_integer::Integer;
use zeroize::Zeroizing;

pub trait RandomBytes {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsaError {
    RandomSource,
    MalformedPayload,
    InvalidPadding,
}
pub type Result<T> = std::result::Result<T, RsaError>;

pub const PUBLIC_EXPONENT: u32 = 65535;
pub const MODULUS_BYTES: usize = 32;
pub const SESSION_KEY_BYTES: usize = 8;
const ALPHABET: &[u8; 64] = b"TSG8IncW3HFKokOg79qzeCmZs2yBYEQVAUxR5rbwi4P@jMDLtpvad0f_J1hlN6uX";

const fn decode_table() -> [u8; 256] {
    let mut table = [u8::MAX; 256];
    let mut index = 0;
    while index < ALPHABET.len() {
        table[ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    table
}
const DECODE: [u8; 256] = decode_table();

pub fn b64_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(4).div_ceil(3));
    let mut carry = 0u16;
    let mut bits = 0u32;
    for &byte in bytes {
        carry = (carry << 8) | u16::from(byte);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            text.push(ALPHABET[((carry >> bits) & 63) as usize] as char);
        }
        carry &= (1u16 << bits) - 1;
    }
    if bits != 0 {
        text.push(ALPHABET[(carry << (6 - bits)) as usize] as char);
    }
    text
}

/// Native decoding emits three bytes per quartet, including a short last
/// quartet padded with alphabet-zero. Unknown symbols also mean zero.
pub fn b64_decode(text: &[u8], count: usize) -> Vec<u8> {
    let source = &text[..count.min(text.len())];
    let mut bytes = Vec::with_capacity(source.len().div_ceil(4) * 3);
    for group in source.chunks(4) {
        let mut word = 0u32;
        for slot in 0..4 {
            let value = group.get(slot).map_or(0, |&byte| DECODE[byte as usize]);
            word = (word << 6) | u32::from(if value == u8::MAX { 0 } else { value });
        }
        bytes.extend_from_slice(&word.to_be_bytes()[1..]);
    }
    bytes
}

struct Factors {
    p: BigUint,
    q: BigUint,
    dp: BigUint,
    dq: BigUint,
    q_inverse: BigUint,
}

/// This protocol requires obsolete 256-bit RSA. BigUint cannot guarantee
/// erasure of private arithmetic storage; temporary byte buffers are wiped.
pub struct KeyPair {
    n: BigUint,
    d: BigUint,
    factors: Option<Factors>,
}

impl KeyPair {
    pub fn generate(random: &mut dyn RandomBytes) -> Result<Self> {
        let p = prime(random)?;
        let q = (0..64)
            .find_map(|_| match prime(random) {
                Ok(candidate) if candidate != p => Some(Ok(candidate)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .ok_or(RsaError::RandomSource)??;
        let p_minus = &p - 1u32;
        let q_minus = &q - 1u32;
        let d = inverse(&BigUint::from(PUBLIC_EXPONENT), &(&p_minus * &q_minus))
            .ok_or(RsaError::RandomSource)?;
        let factors = Factors {
            dp: &d % &p_minus,
            dq: &d % &q_minus,
            q_inverse: inverse(&q, &p).ok_or(RsaError::RandomSource)?,
            p: p.clone(),
            q: q.clone(),
        };
        Ok(Self {
            n: p * q,
            d,
            factors: Some(factors),
        })
    }

    pub fn modulus_wire_bytes(&self) -> [u8; 32] {
        fixed_le(&self.n)
    }
    pub fn realname(&self) -> String {
        b64_encode(&self.modulus_wire_bytes())
    }

    pub fn stream_iv(&self) -> (u32, u32) {
        let limbs = self.n.to_u32_digits();
        (
            limbs.first().copied().unwrap_or(0),
            limbs.get(1).copied().unwrap_or(0),
        )
    }

    fn private_operation(&self, cipher: &BigUint) -> BigUint {
        if let Some(factors) = &self.factors {
            let left = cipher.modpow(&factors.dp, &factors.p);
            let right = cipher.modpow(&factors.dq, &factors.q);
            let adjustment =
                (&left + &factors.p - (&right % &factors.p)) * &factors.q_inverse % &factors.p;
            right + &factors.q * adjustment
        } else {
            cipher.modpow(&self.d, &self.n)
        }
    }

    pub fn recover_session_key(&self, payload: &[u8]) -> Result<[u8; 8]> {
        // Only these 43 symbols carry ciphertext. A following line checksum
        // is not part of the encrypted block and need not use this alphabet.
        let encoded = payload.get(..43).ok_or(RsaError::MalformedPayload)?;
        if encoded.iter().any(|&byte| DECODE[byte as usize] == u8::MAX) {
            return Err(RsaError::MalformedPayload);
        }
        let bytes = Zeroizing::new(b64_decode(encoded, encoded.len()));
        let ciphertext = BigUint::from_bytes_be(&bytes[..32]);
        if ciphertext >= self.n {
            return Err(RsaError::MalformedPayload);
        }
        let integer = self.private_operation(&ciphertext);
        let natural = Zeroizing::new(integer.to_bytes_be());
        if natural.len() > 32 {
            return Err(RsaError::MalformedPayload);
        }
        let mut padded = Zeroizing::new([0; 32]);
        padded[32 - natural.len()..].copy_from_slice(&natural);
        let contents = message(&padded)?;
        let key_bytes = contents.get(..8).ok_or(RsaError::MalformedPayload)?;
        Ok(std::array::from_fn(|index| key_bytes[7 - index]))
    }
}

/// POL skips the first two bytes without validating their values. Retain
/// that interoperability behavior; it is not a general-purpose PKCS#1 API.
fn message(block: &[u8; 32]) -> Result<&[u8]> {
    let separator = block[2..]
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(RsaError::InvalidPadding)?
        + 2;
    Ok(&block[separator + 1..])
}

fn fixed_le(value: &BigUint) -> [u8; 32] {
    let bytes = Zeroizing::new(value.to_bytes_le());
    std::array::from_fn(|index| bytes.get(index).copied().unwrap_or(0))
}

fn inverse(value: &BigUint, modulus: &BigUint) -> Option<BigUint> {
    let mut remainders = (modulus.clone(), value % modulus);
    let mut coefficients = (BigInt::from(0), BigInt::from(1));
    while remainders.1 != BigUint::from(0u32) {
        let quotient = &remainders.0 / &remainders.1;
        let remainder = &remainders.0 % &remainders.1;
        let coefficient = &coefficients.0 - BigInt::from(quotient) * &coefficients.1;
        remainders = (remainders.1, remainder);
        coefficients = (coefficients.1, coefficient);
    }
    if remainders.0 != BigUint::from(1u32) {
        return None;
    }
    coefficients
        .0
        .mod_floor(&BigInt::from(modulus.clone()))
        .to_biguint()
}

fn prime(random: &mut dyn RandomBytes) -> Result<BigUint> {
    let exponent = BigUint::from(PUBLIC_EXPONENT);
    for _ in 0..64 {
        let mut bytes = Zeroizing::new([0; 16]);
        random.fill(&mut bytes[..])?;
        bytes[0] |= 1;
        bytes[15] = (bytes[15] & 15) | 128;
        let mut candidate = BigUint::from_bytes_le(&bytes[..]);
        for _ in 0..4096 {
            if (&candidate - 1u32).gcd(&exponent) == BigUint::from(1u32)
                && probable_prime(&candidate, random)?
            {
                return Ok(candidate);
            }
            candidate += 2u32;
            if candidate.bits() > 128 {
                break;
            }
        }
    }
    Err(RsaError::RandomSource)
}

fn probable_prime(number: &BigUint, random: &mut dyn RandomBytes) -> Result<bool> {
    const SMALL: [u32; 12] = [3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41];
    if SMALL
        .iter()
        .any(|prime| number % *prime == BigUint::from(0u32))
    {
        return Ok(false);
    }
    let last = number - 1u32;
    let twos = last.trailing_zeros().ok_or(RsaError::RandomSource)?;
    let odd = &last >> twos as usize;
    for _ in 0..24 {
        let mut draw = Zeroizing::new([0; 16]);
        let mut witness = None;
        for _ in 0..64 {
            random.fill(&mut draw[..])?;
            let value = BigUint::from_bytes_le(&draw[..]);
            if value >= BigUint::from(2u32) && value < last {
                witness = Some(value);
                break;
            }
        }
        let mut residue = witness.ok_or(RsaError::RandomSource)?.modpow(&odd, number);
        let mut passed = residue == BigUint::from(1u32) || residue == last;
        for _ in 1..twos {
            if passed {
                break;
            }
            residue = &residue * &residue % number;
            passed = residue == last;
        }
        if !passed {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Python integer arithmetic and standard base64 independently generate these records.
    const VECTORS: &str = include_str!("fixtures/rsa.tsv");

    fn rows(tag: &str) -> Vec<Vec<&'static str>> {
        VECTORS
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .filter(|row| row[0] == tag)
            .collect()
    }

    fn number(decimal: &str) -> BigUint {
        BigUint::parse_bytes(decimal.as_bytes(), 10).unwrap()
    }

    fn fixture(row: &[&str], crt: bool) -> KeyPair {
        let p = number(row[1]);
        let q = number(row[2]);
        let d = number(row[3]);
        let factors = crt.then(|| Factors {
            dp: &d % (&p - 1u32),
            dq: &d % (&q - 1u32),
            // Fermat's theorem supplies an independent inverse for these fixture primes.
            q_inverse: q.modpow(&(&p - 2u32), &p),
            p: p.clone(),
            q: q.clone(),
        });
        KeyPair {
            n: p * q,
            d,
            factors,
        }
    }

    #[test]
    fn native_alphabet_codec_matches_standard_base64_translation_and_padding() {
        for row in rows("BASE64") {
            let input = hex::decode(row[1]).unwrap();
            assert_eq!(b64_encode(&input), row[2], "length {}", input.len());
            assert_eq!(
                b64_decode(row[2].as_bytes(), row[2].len()),
                hex::decode(row[3]).unwrap()
            );
        }
        for row in rows("DECODE") {
            assert_eq!(
                b64_decode(&hex::decode(row[1]).unwrap(), row[2].parse().unwrap()),
                hex::decode(row[3]).unwrap()
            );
        }
    }

    #[test]
    fn public_modulus_iv_and_realname_match_python_integer_records() {
        for row in rows("RSA") {
            let key = fixture(&row, false);
            let wire = hex::decode(row[4]).unwrap();
            assert_eq!(key.modulus_wire_bytes().as_slice(), wire);
            assert_eq!(key.realname(), row[5]);
            assert_eq!(
                key.stream_iv(),
                (
                    u32::from_le_bytes(wire[..4].try_into().unwrap()),
                    u32::from_le_bytes(wire[4..8].try_into().unwrap())
                )
            );
        }
    }

    #[test]
    fn server_ciphertexts_match_python_pow_with_and_without_crt() {
        for row in rows("RSA") {
            for crt in [false, true] {
                let key = fixture(&row, crt);
                let decoded = key.recover_session_key(row[7].as_bytes());
                match row[8] {
                    "MalformedPayload" => assert_eq!(decoded, Err(RsaError::MalformedPayload)),
                    "InvalidPadding" => assert_eq!(decoded, Err(RsaError::InvalidPadding)),
                    expected => {
                        assert_eq!(decoded.unwrap().as_slice(), hex::decode(expected).unwrap());
                        let with_suffix = format!("{} !Crystal-synthetic-checksum", row[7]);
                        assert_eq!(
                            key.recover_session_key(with_suffix.as_bytes())
                                .unwrap()
                                .as_slice(),
                            hex::decode(expected).unwrap()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn ciphertext_prefix_symbols_and_modulus_range_are_checked_before_decryption() {
        let records = rows("RSA");
        let row = &records[0];
        let key = fixture(row, true);
        let encoded = row[7].as_bytes();
        for end in 0..43 {
            assert_eq!(
                key.recover_session_key(&encoded[..end]),
                Err(RsaError::MalformedPayload)
            );
        }
        for position in 0..43 {
            let mut damaged = encoded.to_vec();
            damaged[position] = b'!';
            assert_eq!(
                key.recover_session_key(&damaged),
                Err(RsaError::MalformedPayload)
            );
        }
        for invalid in [&key.n, &(&key.n + 1u32)] {
            let mut block = vec![0; 32];
            let bytes = invalid.to_bytes_be();
            block[32 - bytes.len()..].copy_from_slice(&bytes);
            assert_eq!(
                key.recover_session_key(b64_encode(&block).as_bytes()),
                Err(RsaError::MalformedPayload)
            );
        }
    }

    // Deterministic test entropy only; production supplies its own RandomBytes.
    struct TestEntropy(u64);

    impl RandomBytes for TestEntropy {
        fn fill(&mut self, bytes: &mut [u8]) -> Result<()> {
            for byte in bytes {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                *byte = (self.0 >> 29) as u8;
            }
            Ok(())
        }
    }

    #[test]
    fn generated_keys_satisfy_inverse_math_and_crt_matches_full_exponentiation() {
        for seed in [0x7139e264ab5d0821, 0xf123876a94705cbd] {
            let key = KeyPair::generate(&mut TestEntropy(seed)).unwrap();
            let factors = key.factors.as_ref().unwrap();
            assert_eq!(factors.p.bits(), 128);
            assert_eq!(factors.q.bits(), 128);
            assert_ne!(factors.p, factors.q);
            let phi = (&factors.p - 1u32) * (&factors.q - 1u32);
            assert_eq!((&key.d * PUBLIC_EXPONENT) % phi, BigUint::from(1u32));
            for cipher in [
                BigUint::from(0u32),
                BigUint::from(1u32),
                &key.n / 2u32,
                &key.n - 1u32,
            ] {
                assert_eq!(
                    key.private_operation(&cipher),
                    cipher.modpow(&key.d, &key.n)
                );
            }
            let block: Vec<_> = [0, 0x42]
                .into_iter()
                .chain(std::iter::repeat_n(0x8b, 21))
                .chain([0, 0x93, 0x15, 0xe7, 0x49, 0xbb, 0x2d, 0xdf, 0x61])
                .collect();
            let encrypted =
                BigUint::from_bytes_be(&block).modpow(&BigUint::from(PUBLIC_EXPONENT), &key.n);
            let bytes = encrypted.to_bytes_be();
            let mut fixed = vec![0; 32];
            fixed[32 - bytes.len()..].copy_from_slice(&bytes);
            assert_eq!(
                key.recover_session_key(b64_encode(&fixed).as_bytes())
                    .unwrap(),
                [0x61, 0xdf, 0x2d, 0xbb, 0x49, 0xe7, 0x15, 0x93]
            );
        }
    }

    #[test]
    fn unavailable_or_unusable_entropy_returns_a_random_source_error() {
        struct Unavailable;
        impl RandomBytes for Unavailable {
            fn fill(&mut self, _: &mut [u8]) -> Result<()> {
                Err(RsaError::RandomSource)
            }
        }
        struct Zeros;
        impl RandomBytes for Zeros {
            fn fill(&mut self, bytes: &mut [u8]) -> Result<()> {
                bytes.fill(0);
                Ok(())
            }
        }
        assert!(matches!(
            KeyPair::generate(&mut Unavailable),
            Err(RsaError::RandomSource)
        ));
        assert!(matches!(
            KeyPair::generate(&mut Zeros),
            Err(RsaError::RandomSource)
        ));
    }
}
