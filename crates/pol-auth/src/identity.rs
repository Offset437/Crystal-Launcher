//! Crystal's textual member IDs, packed nicknames, and chat line framing.
//! Wire constants and baseline suffix fields are checked against installed
//! POL core 73b1864b, 10019e20/10019f20/1001a390/100198f0.
//! This module is offline; it neither authenticates nor invents machine data.

use crate::{crypto, Error};
use zeroize::Zeroizing;

const DIGITS: &[u8; 36] = b"EFKAOYMJVNGTDSWBQLPCIRHZXU6328401795";
const PLACES: [u64; 8] = [
    36u64.pow(7),
    36u64.pow(6),
    36u64.pow(5),
    36u64.pow(4),
    36u64.pow(3),
    1296,
    36,
    1,
];
const ID_SPACE: u64 = 36u64.pow(8);

pub fn id_decode(text: &[u8]) -> Result<u64, Error> {
    if text.len() != PLACES.len() {
        return Err(Error::InvalidFrame);
    }
    text.iter()
        .zip(PLACES)
        .try_fold(0, |number, (&letter, place)| {
            let digit = DIGITS
                .iter()
                .position(|&value| value == letter)
                .ok_or(Error::InvalidFrame)?;
            Ok(number + place * digit as u64)
        })
}

pub fn id_encode(number: u64) -> Result<[u8; 8], Error> {
    if number >= ID_SPACE {
        return Err(Error::InvalidFrame);
    }
    Ok(std::array::from_fn(|slot| {
        DIGITS[(number / PLACES[slot] % 36) as usize]
    }))
}

/// Compose powers of the native affine recurrence modulo 2^32. Advancing
/// by a byte-derived count takes logarithmic time and preserves wraparound.
fn recurrence(mut steps: u32) -> u32 {
    let (mut multiplier, mut increment) = (0x425f0cbdu32, 0x7f4fu32);
    let mut value = 0u32;
    while steps != 0 {
        if steps & 1 != 0 {
            value = value.wrapping_mul(multiplier).wrapping_add(increment);
        }
        increment = increment.wrapping_mul(multiplier.wrapping_add(1));
        multiplier = multiplier.wrapping_mul(multiplier);
        steps >>= 1;
    }
    value
}

pub fn pol_id_mask(id: &[u8; 8]) -> u64 {
    let mut material = Zeroizing::new(*id);
    let mut previous = material[0];
    for byte in &mut material[1..] {
        let mixed = *byte ^ previous;
        *byte = mixed | ((mixed & 2) << 6);
        previous = *byte;
    }
    let (sum, product) =
        material
            .windows(2)
            .fold((1u64, 0x7048860ddf79u64), |(sum, product), pair| {
                (
                    sum.wrapping_add(pair[1] as u64),
                    product.wrapping_mul(u64::from(pair[0]) * u64::from(pair[1])),
                )
            });
    let steps = material
        .iter()
        .map(|byte| u32::from(byte.saturating_sub(b'0')))
        .sum();
    sum.wrapping_add(product) ^ u64::from(recurrence(steps))
}

pub fn id_to_nick(account: u64) -> [u8; 9] {
    let mut packed = Zeroizing::new((account & ((1u64 << 41) - 1)).to_le_bytes());
    packed[5] |= 0xc0;
    for slot in (0..5).rev() {
        packed[slot] ^= packed[slot + 1];
    }
    packed[5] &= 0x3f;
    let identifier = id_encode(u64::from_le_bytes(*packed)).expect("41-bit nickname range");
    std::array::from_fn(|index| {
        if index == 0 {
            b'U'
        } else {
            identifier[index - 1]
        }
    })
}

/// Derived suffix material is secret-bearing and intentionally has no Debug.
pub struct NickSuffix {
    bytes: Zeroizing<[u8; 24]>,
}

impl NickSuffix {
    /// Uses the traced loader-zero defaults for a standalone Windows session.
    /// Other initialization routes must supply their actual callback/material.
    pub fn windows_baseline(region: u8, textual: &[u8]) -> Result<Self, Error> {
        let id: &[u8; 8] = textual.try_into().map_err(|_| Error::InvalidFrame)?;
        id_decode(id)?;
        let mut fingerprint = Zeroizing::new([0; 82]);
        fingerprint[64..72].copy_from_slice(&pol_id_mask(id).to_le_bytes());
        Self::from_material(region, &[0; 8], &fingerprint)
    }

    pub fn from_material(
        region: u8,
        callback: &[u8; 8],
        fingerprint: &[u8; 82],
    ) -> Result<Self, Error> {
        if region > 99 {
            return Err(Error::InvalidFrame);
        }
        let fingerprint = Zeroizing::new(crypto::md5(fingerprint));
        let mut bytes = Zeroizing::new([0; 24]);
        bytes[3] = 2;
        bytes[4] = 1;
        bytes[5] = region;
        bytes[8..16].copy_from_slice(callback);
        bytes[16..].copy_from_slice(&fingerprint[..8]);
        Ok(Self { bytes })
    }

    pub fn from_derived(material: [u8; 24]) -> Result<Self, Error> {
        let bytes = Zeroizing::new(material);
        let valid = bytes[..8] == [0, 0, 0, 2, 1, bytes[5], 0, 0] && bytes[5] <= 99;
        if !valid {
            return Err(Error::InvalidFrame);
        }
        Ok(Self { bytes })
    }

    pub fn base64(&self) -> Zeroizing<[u8; 32]> {
        let encoded = Zeroizing::new(crate::rsa::b64_encode(&self.bytes[..]));
        Zeroizing::new(
            encoded
                .as_bytes()
                .try_into()
                .expect("24 bytes encode to 32 symbols"),
        )
    }
}

pub fn nick_digest(challenge: &[u8], credential: &[u8]) -> Result<Zeroizing<[u8; 16]>, Error> {
    if credential.len() > 15 || [challenge, credential].iter().any(|part| part.contains(&0)) {
        return Err(Error::InvalidFrame);
    }
    // Feed fragments directly; no heap copy of the combined credential.
    Ok(Zeroizing::new(crypto::md5_parts(&[challenge, credential])))
}

pub fn build_nick(
    account: u64,
    challenge: &[u8],
    credential: &[u8],
    suffix: &NickSuffix,
) -> Result<Zeroizing<Vec<u8>>, Error> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = nick_digest(challenge, credential)?;
    let hexadecimal = Zeroizing::new(std::array::from_fn::<_, 32, _>(|index| {
        let nibble = if index % 2 == 0 {
            digest[index / 2] >> 4
        } else {
            digest[index / 2] & 15
        };
        HEX[nibble as usize]
    }));
    let nickname = id_to_nick(account);
    let encoded = suffix.base64();
    let mut body = Zeroizing::new([0; 80]);
    let mut position = 0;
    for field in [
        &b"NICK "[..],
        &nickname,
        b":",
        &hexadecimal[..],
        b":",
        &encoded[..],
    ] {
        body[position..position + field.len()].copy_from_slice(field);
        position += field.len();
    }
    frame_line(&body[..])
}

fn line_checksum(body: &[u8]) -> [u8; 4] {
    let sum = crypto::checksum(body, 0);
    std::array::from_fn(|slot| (((sum >> (26 - slot * 6)) & 63) + 63) as u8)
}

fn printable(body: &[u8]) -> Result<(), Error> {
    if body.is_empty() || !body.iter().all(|byte| (32..=126).contains(byte)) {
        Err(Error::InvalidFrame)
    } else {
        Ok(())
    }
}

pub fn frame_line(body: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    printable(body)?;
    let length = body.len().checked_add(6).ok_or(Error::InvalidFrame)?;
    let mut result = Zeroizing::new(Vec::new());
    result
        .try_reserve_exact(length)
        .map_err(|_| Error::InvalidFrame)?;
    result.resize(length, 0);
    result[..body.len()].copy_from_slice(body);
    result[body.len()..length - 2].copy_from_slice(&line_checksum(body));
    result[length - 2..].copy_from_slice(b"\r\n");
    Ok(result)
}

pub fn verified_line_body(frame: &[u8]) -> Result<&[u8], Error> {
    let without_end = frame.strip_suffix(b"\r\n").ok_or(Error::InvalidFrame)?;
    let length = without_end
        .len()
        .checked_sub(4)
        .ok_or(Error::InvalidFrame)?;
    let (body, checksum) = without_end.split_at(length);
    printable(body)?;
    if !checksum.iter().all(|byte| (63..=126).contains(byte)) {
        return Err(Error::InvalidFrame);
    }
    if checksum != line_checksum(body) {
        return Err(Error::Checksum);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_advance_matches_the_native_iterated_recurrence() {
        let mut native = 0u32;
        for steps in 0..=2048 {
            assert_eq!(recurrence(steps), native);
            native = native.wrapping_mul(0x425f0cbd).wrapping_add(0x7f4f);
        }
    }

    #[test]
    fn pol_id_mask_matches_independent_integer_vectors() {
        // Independent Python modulo arithmetic, not outputs generated by this
        // Rust function. Root's native fixture can exercise arbitrary8B input.
        for (input, expected) in [
            ([0; 8], 0x0000_0000_0000_0001),
            ([0xff; 8], 0x0000_0000_25e4_b9d2),
            (*b"TEST0001", 0x42d8_be85_aaae_9c40),
            (*b"ABCD1234", 0x31a0_d748_90e7_fb20),
            (*b"EEEEEEEE", 0x0000_0000_6e6c_10b4),
        ] {
            assert_eq!(pol_id_mask(&input), expected);
        }
    }

    #[test]
    fn windows_baseline_suffix_matches_independent_md5_wire_vector() {
        // Python hashlib over zero64 || LE64(mask) || zero10, then custom POL
        // base64 of header8 || zero8 || digest8. Synthetic textual account.
        let suffix = NickSuffix::windows_baseline(2, b"TEST0001").unwrap();
        assert_eq!(&suffix.base64()[..], b"TTTTTAIGTTTTTTTTTTTTTHqQQfyDZVNb");
        assert!(NickSuffix::windows_baseline(100, b"TEST0001").is_err());
        assert!(NickSuffix::windows_baseline(2, b"short").is_err());
        assert!(NickSuffix::windows_baseline(2, b"test0001").is_err());
    }

    #[test]
    fn pinned_id_and_nick_vectors_are_distinct_codecs() {
        //Constants independently calculated in Python integer arithmetic from
        //the pinned reference equations, not generated by these Rust methods.
        for (id, text, nick) in [
            (0, *b"EEEEEEEE", *b"UGITW5T4X"),
            (1, *b"EEEEEEEF", *b"UGITW5T4U"),
            (74565, *b"EEEEFRCN", *b"UGITBE7OJ"),
            ((1u64 << 41) - 1, *b"2KJ8XAD0", *b"ULQ7QAVWM"),
        ] {
            assert_eq!(id_encode(id).unwrap(), text);
            assert_eq!(id_decode(&text).unwrap(), id);
            assert_eq!(id_to_nick(id), nick);
        }
        assert_eq!(id_to_nick(1u64 << 41), id_to_nick(0));
        assert_eq!(id_encode(ID_SPACE - 1).unwrap(), *b"55555555");
    }
    #[test]
    fn suffix_digest_and_full_line_match_independent_vectors() {
        //Python hashlib plus explicit POL base64 alphabet and dword-sum
        //framing produce these constants; no native/live authentication claim.
        let mut fingerprint = [0; 82];
        for (i, b) in fingerprint.iter_mut().enumerate() {
            *b = i as u8;
        }
        let suffix = NickSuffix::from_material(2, &[0, 1, 2, 3, 4, 5, 6, 7], &fingerprint).unwrap();
        assert_eq!(&suffix.base64()[..], b"TTTTTAIGTTTTT738STecSaGZlyCbBxjw");
        let line = build_nick(74565, b"challenge", b"secret", &suffix).unwrap();
        assert_eq!(&line[..], b"NICK UGITBE7OJ:3956ed0865d1cd94c99f8e01c3988788:TTTTTAIGTTTTT738STecSaGZlyCbBxjwJeVP\r\n");
        assert_eq!(verified_line_body(&line).unwrap().len(), 80);
        let derived = NickSuffix::from_derived(*suffix.bytes).unwrap();
        assert_eq!(*derived.base64(), *suffix.base64());
    }
    #[test]
    fn checksum_known_vector_and_corruption_are_checked() {
        assert_eq!(
            &frame_line(b"PlayOnline").unwrap()[..],
            b"PlayOnlineQBnY\r\n"
        );
        let mut line = frame_line(b"USER x 0 * :blob").unwrap();
        assert_eq!(verified_line_body(&line).unwrap(), b"USER x 0 * :blob");
        line[1] ^= 0x20;
        assert_eq!(verified_line_body(&line), Err(Error::Checksum));
    }
    #[test]
    fn invalid_ids_credentials_suffixes_and_lines_are_rejected() {
        for invalid in [&b"short"[..], &b"eeeeeeee"[..], &b"EFKAOYM!"[..]] {
            assert!(id_decode(invalid).is_err());
        }
        assert!(id_encode(ID_SPACE).is_err());
        assert!(nick_digest(b"challenge", &[b'x'; 16]).is_err());
        assert!(nick_digest(b"challenge", b"secret\0ignored").is_err());
        assert!(nick_digest(b"a\0b", b"secret").is_err());
        assert!(NickSuffix::from_derived([0; 24]).is_err());
        assert!(NickSuffix::from_material(100, &[0; 8], &[0; 82]).is_err());
        for invalid in [
            &b""[..],
            &b"NICK x\r\nOPER x"[..],
            &b"a\0b"[..],
            &[0xff][..],
            &[127][..],
        ] {
            assert!(frame_line(invalid).is_err());
        }
        assert!(verified_line_body(b"USER x????\n").is_err());
        assert!(verified_line_body(b"USER x!!!!\r\n").is_err());
        assert!(verified_line_body(b"USER x????\r\n\r\n").is_err());
    }
}
