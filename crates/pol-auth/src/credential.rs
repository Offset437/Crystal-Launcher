use crate::Error;
use sha1::{Digest, Sha1};
use zeroize::{Zeroize, Zeroizing};

/// Square Enix password: app.dll 1019ab29, confirmed by StringTable resource 703f.
pub struct EditorSecret(Zeroizing<[u8; 20]>);

impl EditorSecret {
    pub fn from_utf16(input: &[u16]) -> Self {
        let bytes = Zeroizing::new(input.iter().map(|v| *v as u8).collect::<Vec<_>>());
        let mut first: [u8; 20] = Sha1::digest(&*bytes).into();
        let mut lower_hex = Zeroizing::new([0u8; 40]);
        encode_lower_hex(&first, &mut lower_hex);
        let mut second = Sha1::new();
        second.update(&*lower_hex);
        second.update(b"playonline");
        first.zeroize();
        Self(Zeroizing::new(second.finalize().into()))
    }

    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Transfer the editor's transformed secret to POL's native credential setter.
    /// This is not a time-bound login payload. The recipient must zeroize it.
    pub fn into_native_secret(self) -> Zeroizing<[u8; 20]> {
        self.0
    }

    pub(crate) fn bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl std::fmt::Debug for EditorSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EditorSecret([redacted])")
    }
}

pub struct ProfileCredential {
    name: String,
    secret: EditorSecret,
    otp: Option<Zeroizing<[u8; 6]>>,
}

impl ProfileCredential {
    /// Square Enix ID: settings editor +2b8 -> member +5358, resource 7049.
    pub fn new(name: &str, secret: EditorSecret, otp: Option<&str>) -> Result<Self, Error> {
        if name.is_empty()
            || name.len() > 16
            || !name.is_ascii()
            || name.as_bytes().iter().any(|b| b.is_ascii_control())
        {
            return Err(Error::InvalidName);
        }
        let otp = otp
            .map(|value| {
                if value.len() != 6 || !value.as_bytes().iter().all(u8::is_ascii_digit) {
                    return Err(Error::InvalidOtp);
                }
                Ok(Zeroizing::new(value.as_bytes().try_into().unwrap()))
            })
            .transpose()?;
        Ok(Self {
            name: name.to_owned(),
            secret,
            otp,
        })
    }

    /// polcore 1001e760. The time is synchronized server time, not assumed wall time.
    pub fn login_payload(&self, server_seconds: u32) -> Zeroizing<[u8; 60]> {
        // Native builder reserves 64 bytes; the transport supplies the final checksum word.
        let mut body = Zeroizing::new([0u8; 60]);
        body[0] = if self.otp.is_some() { 2 } else { 1 };
        body[1..1 + self.name.len()].copy_from_slice(self.name.as_bytes());
        let mut lower_hex = Zeroizing::new([0u8; 40]);
        encode_lower_hex(self.secret.bytes(), &mut lower_hex);
        let minute = server_seconds - server_seconds % 60;
        let mut hash = Sha1::new();
        hash.update(&*lower_hex);
        hash.update(minute.to_string().as_bytes());
        body[32..52].copy_from_slice(&hash.finalize());
        if let Some(otp) = &self.otp {
            body[18..24].copy_from_slice(&**otp);
        }
        body
    }
}

impl std::fmt::Debug for ProfileCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProfileCredential([redacted])")
    }
}

impl Drop for ProfileCredential {
    fn drop(&mut self) {
        self.name.zeroize();
    }
}

fn encode_lower_hex(input: &[u8; 20], out: &mut [u8; 40]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in input.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 15) as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_is_consumed_by_code_unit_low_byte() {
        let a = EditorSecret::from_utf16(&[0x0061, 0x00ff, 0xd83d, 0xde00]);
        let b = EditorSecret::from_utf16(&[0x0161, 0x01ff, 0x003d, 0x0000]);
        assert_eq!(a.bytes(), b.bytes());
    }

    #[test]
    fn password_transform_matches_independent_hashlib_vectors() {
        let ascii = EditorSecret::from_utf16(&"synthetic".encode_utf16().collect::<Vec<_>>());
        assert_eq!(
            hex::encode(ascii.bytes()),
            "e8a29df64156bd20570e809e22cc55f153f1b830"
        );
        let units = EditorSecret::from_utf16(&[0x0061, 0x00ff, 0xd83d, 0xde00]);
        assert_eq!(
            hex::encode(units.bytes()),
            "ad5036546a7614e56fc929e7ef85c95488dd81c8"
        );
    }

    #[test]
    fn native_name_boundary_and_minute_clock() {
        let secret = EditorSecret::from_utf16(&"synthetic".encode_utf16().collect::<Vec<_>>());
        let credential =
            ProfileCredential::new("abcdefghijklmnop", secret, Some("123456")).unwrap();
        let first = credential.login_payload(60);
        assert_eq!(&first[1..17], b"abcdefghijklmnop");
        assert_eq!(first[17], 0);
        assert_eq!(&first[18..24], b"123456");
        assert_eq!(first[0], 2);
        assert_eq!(*first, *credential.login_payload(119));
        assert_ne!(&first[32..52], &credential.login_payload(120)[32..52]);
        assert!(!format!("{credential:?}").contains("123456"));
    }

    #[test]
    fn control_bytes_cannot_enter_the_name_field() {
        for name in ["", "0123456789abcdefg", "a\0b", "a\nb", "é"] {
            assert!(ProfileCredential::new(name, EditorSecret::from_bytes([0; 20]), None).is_err());
        }
    }
}
