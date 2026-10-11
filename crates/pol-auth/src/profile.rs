use crate::{crypto, Error};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transaction(pub u8, pub u8);

pub const MEMBER_LOGIN: Transaction = Transaction(4, 7);
pub const CONTENT_IDS: Transaction = Transaction(2, 3);
pub const SELECT_SERVICE: Transaction = Transaction(4, 6);
pub const ENTER_COMMUNITY: Transaction = Transaction(4, 5);

pub fn request(
    tx: Transaction,
    payload: &[u8],
    member_id: &[u8; 8],
    member_secret: &[u8],
    nonce: [u8; 4],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    if member_secret.len() > 15 || member_secret.contains(&0) {
        return Err(Error::InvalidFrame);
    }
    let length = if payload.is_empty() {
        0
    } else {
        u32::try_from(payload.len())
            .ok()
            .and_then(|n| n.checked_add(4))
            .ok_or(Error::InvalidFrame)?
    };
    let mut authenticated = Zeroizing::new(Vec::with_capacity(8 + member_secret.len() + 4));
    authenticated.extend_from_slice(member_id);
    authenticated.extend_from_slice(member_secret);
    authenticated.extend_from_slice(&nonce);
    let mut bytes = Zeroizing::new(vec![0u8; 40]);
    bytes[0] = 2;
    bytes[1] = tx.0;
    bytes[2] = tx.1;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes[24..40].copy_from_slice(&crypto::md5(&authenticated));
    if !payload.is_empty() {
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&crypto::checksum(payload, 0).to_le_bytes());
    }
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReplyHeader {
    pub status: u8,
    pub body_length: u32,
    pub redirect: u32,
    pub nonce: [u8; 4],
}

impl ReplyHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 24 {
            return Err(Error::InvalidFrame);
        }
        Ok(Self {
            status: bytes[1],
            body_length: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            redirect: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            nonce: bytes[20..24].try_into().unwrap(),
        })
    }

    pub fn verified_payload<'a>(&self, body: &'a [u8]) -> Result<&'a [u8], Error> {
        if self.status != 0 {
            return Err(Error::ServerStatus(self.status));
        }
        if self.body_length == 0 && body.is_empty() {
            return Ok(body);
        }
        if body.len() != self.body_length as usize || body.len() < 4 {
            return Err(Error::InvalidFrame);
        }
        let (payload, checksum) = body.split_at(body.len() - 4);
        let expected = u32::from_le_bytes(checksum.try_into().unwrap());
        if crypto::checksum(payload, 0) != expected {
            return Err(Error::Checksum);
        }
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_nul_cannot_change_native_secret_interpretation() {
        assert_eq!(
            request(MEMBER_LOGIN, &[0; 60], b"EFKAOYMJ", b"a\0b", [0; 4]),
            Err(Error::InvalidFrame)
        );
    }

    #[test]
    fn catalog_request_has_no_empty_checksum_body() {
        let bytes = request(CONTENT_IDS, &[], b"EFKAOYMJ", b"fixture", [0; 4]).unwrap();
        assert_eq!(bytes.len(), 40);
        assert_eq!(&bytes[4..8], &[0; 4]);
    }

    #[test]
    fn nonce_binds_the_request_to_the_connection() {
        let a = request(MEMBER_LOGIN, &[5; 64], b"EFKAOYMJ", b"fixture", [1; 4]).unwrap();
        let b = request(MEMBER_LOGIN, &[5; 64], b"EFKAOYMJ", b"fixture", [2; 4]).unwrap();
        assert_ne!(&a[24..40], &b[24..40]);
        assert_eq!(&a[40..], &b[40..]);
    }

    #[test]
    fn reply_rejects_partial_and_corrupt_bodies() {
        let payload = [1, 2, 3, 4, 5, 6];
        let request = request(ENTER_COMMUNITY, &payload, b"EFKAOYMJ", b"fixture", [0; 4]).unwrap();
        let body = &request[40..];
        let mut header = [0u8; 24];
        header[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
        let reply = ReplyHeader::parse(&header).unwrap();
        assert_eq!(reply.verified_payload(body).unwrap(), payload);
        assert_eq!(
            reply.verified_payload(&body[..body.len() - 1]),
            Err(Error::InvalidFrame)
        );
        let mut corrupt = body.to_vec();
        corrupt[0] ^= 1;
        assert_eq!(reply.verified_payload(&corrupt), Err(Error::Checksum));
    }
}
