//! Offline chat greeting primitives traced from installed polcore 73b1864b.
//! No sockets or credentials. Synthetic tests do not establish retail acceptance.

use crate::crypto::checksum;

const BASE32: &[u8; 32] = b"N43OVHBJ1Y2C0WSXED5QFILRZMUTAPGK";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatError {
    Framing,
    Checksum,
    Alphabet,
    Truncated,
}

/// The initial plaintext branch strips four trailing characters without checking
/// them (10015f3c, 10015f7f). Encrypted-line parsing verifies them (10015ed7).
/// `Verify` is an explicit stricter option, not claimed native greeting behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GreetingChecksum {
    NativeInitial,
    Verify,
}

/// MSB-first custom base32 (10007210). A short final group is zero-filled to
/// eight symbols and produces five bytes, as in the native >=8-symbol path.
/// Invalid symbols are rejected rather than reproducing permissive table reads.
pub fn decode_base32(input: &[u8]) -> Result<Vec<u8>, ChatError> {
    let mut out = Vec::with_capacity(input.len().div_ceil(8) * 5);
    for group in input.chunks(8) {
        let mut bits = 0u64;
        for i in 0..8 {
            let value = match group.get(i) {
                Some(c) => BASE32
                    .iter()
                    .position(|v| v == c)
                    .ok_or(ChatError::Alphabet)?,
                None => 0,
            };
            bits = (bits << 5) | value as u64;
        }
        out.extend_from_slice(&bits.to_be_bytes()[3..]);
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Greeting {
    /// Exact ASCII challenge, excluding checksum and CRLF, for the NICK digest.
    pub challenge: Vec<u8>,
    /// Preserve unclassified bytes rather than inventing meanings for them.
    pub header: [u8; 24],
    pub server_clock: u32,
    pub ipv4: [u8; 4],
    pub port: u16,
}

/// Primary-chat greeting redirect, distinct from its reported POL address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub ipv4: [u8; 4],
    pub port: u16,
}

impl Greeting {
    ///10016079..94 requests one reconnect when the first greeting's+8 is
    /// nonzero.10014167..10014232 reads network IPv4+8 and BE port+12.
    /// Native accepts the second greeting without following this field again.
    pub fn redirect(&self) -> Option<Redirect> {
        let ipv4: [u8; 4] = self.header[8..12].try_into().unwrap();
        if ipv4 == [0; 4] {
            return None;
        }
        let port = u16::from_be_bytes(self.header[12..14].try_into().unwrap());
        Some(Redirect {
            ipv4,
            port: if port == 0 { 51240 } else { port },
        })
    }

    /// Only these eight address bytes are assigned by 100160a6..100160ed.
    pub fn address_prefix(&self) -> [u8; 8] {
        let mut out = [0; 8];
        out[..2].copy_from_slice(&1u16.to_le_bytes());
        out[2..4].copy_from_slice(&self.port.to_le_bytes());
        out[4..].copy_from_slice(&u32::from_be_bytes(self.ipv4).to_le_bytes());
        out
    }

    /// Native copies all twenty bytes, but provenance of the last twelve is
    /// unresolved. Require them explicitly; never silently fabricate zeros.
    pub fn address_with_tail(&self, unresolved_tail: [u8; 12]) -> [u8; 20] {
        let mut out = [0; 20];
        out[..8].copy_from_slice(&self.address_prefix());
        out[8..].copy_from_slice(&unresolved_tail);
        out
    }
}

fn next_token(input: &[u8]) -> Result<&[u8], ChatError> {
    let end = input
        .iter()
        .position(|b| *b == b' ')
        .ok_or(ChatError::Framing)?;
    if end == 0 {
        return Err(ChatError::Framing);
    }
    let rest = &input[end..];
    let start = rest
        .iter()
        .position(|b| *b != b' ')
        .ok_or(ChatError::Framing)?;
    Ok(&rest[start..])
}

fn frame_body(raw: &[u8], policy: GreetingChecksum) -> Result<&[u8], ChatError> {
    let line = raw.strip_suffix(b"\r\n").ok_or(ChatError::Framing)?;
    if line.len() < 4 || line.iter().any(|b| !(0x20..=0x7e).contains(b)) {
        return Err(ChatError::Framing);
    }
    let (body, field) = line.split_at(line.len() - 4);
    if policy == GreetingChecksum::Verify {
        if field.iter().any(|b| !(b'?'..=b'~').contains(b)) {
            return Err(ChatError::Checksum);
        }
        let supplied = field
            .iter()
            .zip([26, 20, 14, 8])
            .fold(0u32, |v, (&c, shift)| v | (u32::from(c - b'?') << shift));
        if supplied != (checksum(body, 0) & 0xffff_ff00) {
            return Err(ChatError::Checksum);
        }
    }
    Ok(body)
}

/// Parse one complete, plaintext initial greeting. Decryption is outside this
/// module. 10017350 skips an optional `:server` token; two 100172f0 calls skip
/// command and recipient. No particular numeric code is established by this
/// native branch, so this parser does not invent one.
pub fn parse_greeting(raw: &[u8], policy: GreetingChecksum) -> Result<Greeting, ChatError> {
    let body = frame_body(raw, policy)?;
    let command = if body.starts_with(b":") {
        next_token(body)?
    } else {
        body
    };
    let challenge = next_token(next_token(command)?)?;
    // 24 bytes need at least 39 five-bit symbols. Native allocates 129 bytes
    // for the challenge; reject overflow/truncation before decoding.
    if challenge.len() < 39 {
        return Err(ChatError::Truncated);
    }
    if challenge.len() > 128 {
        return Err(ChatError::Framing);
    }
    let decoded = decode_base32(challenge)?;
    let header: [u8; 24] = decoded[..24].try_into().unwrap();
    Ok(Greeting {
        challenge: challenge.to_vec(),
        server_clock: u32::from_be_bytes(header[..4].try_into().unwrap()),
        ipv4: header[4..8].try_into().unwrap(),
        port: u16::from_be_bytes(header[20..22].try_into().unwrap()),
        header,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationMetadata {
    /// 10015a00's converted bytes, including explicit preserved bytes12..15.
    pub converted: [u8; 24],
}

impl RegistrationMetadata {
    /// Sign-in state24 passes metadata byte2 through1001e9b0 to10404a92.
    pub fn host_index(&self) -> u8 {
        self.converted[2]
    }
}

/// Parse a decrypted registration numeric300 line, including its checksum and
/// CRLF. The caller must already know this is the registration reply: numeric300
/// is also used for RSA negotiation in a different native state.
///
/// 1001626d skips optional server prefix, numeric and recipient;10016286 asks
/// the base32 decoder for46 symbols. Only the first24 decoded bytes reach
/// 10015a00, so39 valid symbols suffice. Extra symbols cannot affect this24-byte
/// result and are ignored here; this avoids native reads beyond a short token.
/// Conversion swaps BE16 fields0,4,8 and BE64 field16 into little-endian storage.
/// Native leaves bytes12..15 unchanged, so their previous value is mandatory.
pub fn parse_registration_metadata(
    raw: &[u8],
    preserved_12_to_15: [u8; 4],
) -> Result<RegistrationMetadata, ChatError> {
    let body = frame_body(raw, GreetingChecksum::Verify)?;
    let command = if body.starts_with(b":") {
        next_token(body)?
    } else {
        body
    };
    if !command.starts_with(b"300 ") {
        return Err(ChatError::Framing);
    }
    let payload = next_token(next_token(command)?)?;
    if payload.len() < 39 {
        return Err(ChatError::Truncated);
    }
    let decoded = decode_base32(&payload[..39])?;
    let mut converted: [u8; 24] = decoded[..24].try_into().unwrap();
    for offset in [0, 4, 8] {
        converted[offset..offset + 2].reverse();
    }
    converted[12..16].copy_from_slice(&preserved_12_to_15);
    converted[16..24].reverse();
    Ok(RegistrationMetadata { converted })
}

/// 1004cca0/1004ccf0: server seconds anchored to a monotonic millisecond tick.
/// Pass the low32 bits of the same monotonic source to both methods. Native
/// wrap semantics cover one tick wrap, not multiple unseen ~49.7-day periods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerClock {
    pub greeting_seconds: u32,
    tick_at_greeting: u32,
}

impl ServerClock {
    pub fn new(greeting_seconds: u32, monotonic_ms: u32) -> Self {
        Self {
            greeting_seconds,
            tick_at_greeting: monotonic_ms,
        }
    }

    pub fn seconds_at(self, monotonic_ms: u32) -> u32 {
        self.greeting_seconds
            .wrapping_add(monotonic_ms.wrapping_sub(self.tick_at_greeting) / 1000)
    }
}

/// 10016216/10016222/10016224 -> 10063eb0: first two little-endian
/// modulus limbs, not the private exponent or recovered session key.
pub fn modulus_iv(modulus_le: &[u8; 32]) -> (u32, u32) {
    (
        u32::from_le_bytes(modulus_le[..4].try_into().unwrap()),
        u32::from_le_bytes(modulus_le[4..8].try_into().unwrap()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = body.to_vec();
        let sum = checksum(body, 0);
        out.extend([26, 20, 14, 8].map(|s| ((sum >> s) & 63) as u8 + b'?'));
        out.extend_from_slice(b"\r\n");
        out
    }

    // Synthetic 24-byte header: 00,01,...,17; independently encoded once.
    const CHALLENGE: &[u8] = b"NNNEVNZVNFONS3NY41HEZO1S4A143VEQ3E2D0HZ";

    #[test]
    fn known_base32_group_and_invalid_symbol() {
        assert_eq!(
            decode_base32(b"N43OVHBJ").unwrap(),
            [0x00, 0x44, 0x32, 0x14, 0xc7]
        );
        assert_eq!(decode_base32(b"!").unwrap_err(), ChatError::Alphabet);
    }

    #[test]
    fn greeting_endianness_and_unknown_tail() {
        let mut body = b":synthetic 001 target ".to_vec();
        body.extend_from_slice(CHALLENGE);
        let value = parse_greeting(&frame(&body), GreetingChecksum::Verify).unwrap();
        assert_eq!(value.header, std::array::from_fn(|i| i as u8));
        assert_eq!(value.server_clock, 0x00010203);
        assert_eq!(value.ipv4, [4, 5, 6, 7]);
        assert_eq!(value.port, 0x1415);
        assert_eq!(value.address_prefix(), [1, 0, 0x15, 0x14, 7, 6, 5, 4]);
        assert_eq!(&value.address_with_tail([0xa5; 12])[8..], &[0xa5; 12]);
        assert_eq!(value.challenge, CHALLENGE);
        assert_eq!(
            value.redirect(),
            Some(Redirect {
                ipv4: [8, 9, 10, 11],
                port: 0x0c0d
            })
        );
    }

    #[test]
    fn framing_truncation_and_checksum_policy() {
        assert_eq!(
            parse_greeting(b"001 target NNNN????", GreetingChecksum::Verify),
            Err(ChatError::Framing)
        );
        assert_eq!(
            parse_greeting(&frame(b"001 target NNNN"), GreetingChecksum::Verify),
            Err(ChatError::Truncated)
        );
        let mut body = b"001 target ".to_vec();
        body.extend_from_slice(CHALLENGE);
        let mut raw = frame(&body);
        let index = raw.len() - 6;
        raw[index] = if raw[index] == b'?' { b'@' } else { b'?' };
        assert_eq!(
            parse_greeting(&raw, GreetingChecksum::Verify),
            Err(ChatError::Checksum)
        );
        assert!(parse_greeting(&raw, GreetingChecksum::NativeInitial).is_ok());
    }

    #[test]
    fn clock_ticks_and_seconds_wrap() {
        let clock = ServerClock::new(100, u32::MAX - 499);
        assert_eq!(clock.seconds_at(499), 100);
        assert_eq!(clock.seconds_at(500), 101);
        assert_eq!(ServerClock::new(u32::MAX, 0).seconds_at(1000), 0);
        assert_eq!(clock.greeting_seconds, 100);
    }

    #[test]
    fn iv_is_modulus_low_limbs() {
        let modulus = std::array::from_fn(|i| i as u8);
        assert_eq!(modulus_iv(&modulus), (0x03020100, 0x07060504));
    }

    #[test]
    fn registration_metadata_converts_only_assigned_fields() {
        // Python standard base32(bytes(range(29))), translated to the POL
        // alphabet, first46 symbols. Expected output is literal, not produced
        // by the parser or a matching encoder in this test.
        let payload = b"NNNEVNZVNFONS3NY41HEZO1S4A143VEQ3E2D0HZZOVW4LJ";
        assert_eq!(payload.len(), 46);
        let mut body = b":synthetic 300 recipient ".to_vec();
        body.extend_from_slice(payload);
        let metadata =
            parse_registration_metadata(&frame(&body), [0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
        assert_eq!(
            metadata.converted,
            [
                1, 0, 2, 3, 5, 4, 6, 7, 9, 8, 10, 11, 0xaa, 0xbb, 0xcc, 0xdd, 23, 22, 21, 20, 19,
                18, 17, 16,
            ]
        );
        assert_eq!(metadata.host_index(), 2);
        // Only39 symbols contribute to the24-byte metadata structure.
        let mut minimum = b"300 recipient ".to_vec();
        minimum.extend_from_slice(&payload[..39]);
        assert_eq!(
            parse_registration_metadata(&frame(&minimum), [0xaa, 0xbb, 0xcc, 0xdd]).unwrap(),
            metadata
        );
        let changed_prior = parse_registration_metadata(&frame(&body), [1, 2, 3, 4]).unwrap();
        assert_eq!(&changed_prior.converted[12..16], &[1, 2, 3, 4]);
    }

    #[test]
    fn registration_rejects_wrong_reply_truncation_and_bad_checksum() {
        let mut body = b"422 recipient ".to_vec();
        body.extend_from_slice(CHALLENGE);
        assert_eq!(
            parse_registration_metadata(&frame(&body), [0; 4]),
            Err(ChatError::Framing)
        );
        body[..3].copy_from_slice(b"300");
        body.pop();
        assert_eq!(
            parse_registration_metadata(&frame(&body), [0; 4]),
            Err(ChatError::Truncated)
        );
        body.push(*CHALLENGE.last().unwrap());
        let mut bad_checksum = frame(&body);
        let index = bad_checksum.len() - 6;
        bad_checksum[index] = b'!';
        assert_eq!(
            parse_registration_metadata(&bad_checksum, [0; 4]),
            Err(ChatError::Checksum)
        );
    }
}
