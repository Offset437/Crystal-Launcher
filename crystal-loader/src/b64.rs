//! Standard base64 (RFC 4648 alphabet). Decoding writes straight into a
//! caller-owned buffer so secrets never pass through an intermediate allocation.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

fn value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    } as u32)
}

/// Number of bytes `src` decodes to, or `None` if it is not canonical base64
/// (padded or unpadded; no whitespace; zero trailing bits).
pub fn decoded_len(src: &[u8]) -> Option<usize> {
    let body = strip_padding(src)?;
    if body.len() % 4 == 1 {
        return None;
    }
    Some(body.len() / 4 * 3 + match body.len() % 4 {
        2 => 1,
        3 => 2,
        _ => 0,
    })
}

fn strip_padding(src: &[u8]) -> Option<&[u8]> {
    let pad = src.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 || (pad > 0 && src.len() % 4 != 0) {
        return None;
    }
    Some(&src[..src.len() - pad])
}

/// Decodes `src` into `out`, which must be exactly the decoded length.
pub fn decode_into(src: &[u8], out: &mut [u8]) -> Result<(), ()> {
    if decoded_len(src) != Some(out.len()) {
        return Err(());
    }
    let body = strip_padding(src).ok_or(())?;
    let mut o = 0;
    for chunk in body.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c).ok_or(())? << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let take = chunk.len() - 1; // 4 -> 3, 3 -> 2, 2 -> 1
        // Non-canonical encodings (stray low bits) are refused.
        if take < 3 && bytes[take..].iter().any(|&b| b != 0) {
            return Err(());
        }
        out[o..o + take].copy_from_slice(&bytes[..take]);
        o += take;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for len in 0..70 {
            let data: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            let enc = encode(&data);
            let mut out = vec![0u8; decoded_len(enc.as_bytes()).unwrap()];
            decode_into(enc.as_bytes(), &mut out).unwrap();
            assert_eq!(out, data);
            // unpadded is accepted too
            let unpadded = enc.trim_end_matches('=');
            let mut out2 = vec![0u8; decoded_len(unpadded.as_bytes()).unwrap()];
            decode_into(unpadded.as_bytes(), &mut out2).unwrap();
            assert_eq!(out2, data);
        }
    }

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode(b"fo"), "Zm8=");
        let mut out = [0u8; 4];
        decode_into(b"Zm9vYg==", &mut out).unwrap();
        assert_eq!(&out, b"foob");
    }

    #[test]
    fn rejects_bad_input() {
        let mut out = [0u8; 3];
        assert!(decode_into(b"Zm9!", &mut out).is_err());
        assert!(decode_into(b"Zm9v ", &mut out).is_err());
        let mut out = [0u8; 2];
        assert!(decode_into(b"Zm8=", &mut out).is_ok());
        assert!(decode_into(b"Zm9=", &mut out).is_err()); // stray low bits
        assert!(decode_into(b"Zm8===", &mut out).is_err());
        assert!(decode_into(b"Zm+=", &mut out).is_err()); // stray low bits
        let mut wrong = [0u8; 5];
        assert!(decode_into(b"Zm9vYg==", &mut wrong).is_err());
        assert_eq!(decoded_len(b"A"), None);
    }
}
