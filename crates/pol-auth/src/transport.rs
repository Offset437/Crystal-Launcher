//! Profile transactions over a caller-owned byte channel, with no socket creation.
//!
//! Installed polcore build73b1864b: connect1001f0f0/hello1001f390;
//! request1001f4d0/1001f970; reply1001f690/1001f800. Header cipher reset is
//! separate for each direction; a body continues its preceding header stream.
//! Catalog bodies stream through the same cipher state without reading a
//! checksum trailer. Authentication orchestration remains caller-owned.

use crate::{
    catalog,
    crypto::{self, PolBlowfish, StreamReset},
    profile, Error,
};
use std::io::{self, Read, Write};
use zeroize::Zeroizing;

/// Borrowed connection inputs. Deliberately has no Debug implementation.
/// `pol_address` is POL's current internal20-byte address, not an inferred
/// sockaddr of this channel. The key and IV must come from chat negotiation.
pub struct Parameters<'a> {
    pub member_id: &'a [u8; 8],
    pub member_secret: &'a [u8],
    pub pol_address: &'a [u8; 20],
    pub session_key: &'a [u8; 8],
    pub iv: (u32, u32),
}

/// A successful fixed response. Payload excludes its checksum; no Debug dump.
pub struct Reply {
    pub header: profile::ReplyHeader,
    pub payload: Zeroizing<Vec<u8>>,
}

/// Catalog records and the reply header needed by subsequent orchestration.
pub struct CatalogReply<T> {
    pub header: profile::ReplyHeader,
    pub catalog: T,
}

pub enum ExchangeError {
    Io(io::Error),
    Protocol(Error),
    Catalog(catalog::CatalogError),
    /// Status114 has a verified fixed detail body in select/community. Other
    /// refusals have empty details. The details may contain session data.
    Service {
        status: u8,
        details: Zeroizing<Vec<u8>>,
    },
}

impl std::fmt::Debug for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => f.debug_tuple("Io").field(e).finish(),
            Self::Protocol(e) => f.debug_tuple("Protocol").field(e).finish(),
            Self::Catalog(e) => f
                .debug_tuple("Catalog")
                .field(&catalog_error_message(e))
                .finish(),
            Self::Service { status, .. } => f
                .debug_struct("Service")
                .field("status", status)
                .field("details", &"[redacted]")
                .finish(),
        }
    }
}
impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "profile channel: {e}"),
            Self::Protocol(e) => e.fmt(f),
            // The kind only: an io::Error's own message can carry caller text.
            Self::Catalog(catalog::CatalogError::Io(e)) => {
                write!(f, "catalog channel read failed ({})", e.kind())
            }
            Self::Catalog(e) => f.write_str(catalog_error_message(e)),
            Self::Service { status, .. } => write!(f, "profile service status {status}"),
        }
    }
}
impl std::error::Error for ExchangeError {}
impl From<io::Error> for ExchangeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<Error> for ExchangeError {
    fn from(value: Error) -> Self {
        Self::Protocol(value)
    }
}
impl From<catalog::CatalogError> for ExchangeError {
    fn from(value: catalog::CatalogError) -> Self {
        Self::Catalog(value)
    }
}

fn catalog_error_message(error: &catalog::CatalogError) -> &'static str {
    match error {
        catalog::CatalogError::Io(_) => "catalog channel read failed",
        catalog::CatalogError::InvalidRecord => "invalid catalog record",
        catalog::CatalogError::InvalidMapLink => "invalid selection map link",
        catalog::CatalogError::CountLimit { .. } => "catalog count exceeds caller limit",
        catalog::CatalogError::LengthOverflow => "catalog byte length overflows",
        catalog::CatalogError::Allocation => "catalog allocation failed",
    }
}

/// A newly initialized encrypted connection's plaintext hello.
/// Native leaves bytes12..23 untouched in its reused BSS transmit buffer.
/// Fresh portable buffers choose zero there; reused-native-buffer parity is
/// not claimed. Native copies only port/address from current POL address20.
pub fn connect_hello(pol_address: &[u8; 20]) -> [u8; 40] {
    let mut hello = [0; 40];
    hello[4..6].copy_from_slice(&1u16.to_le_bytes());
    hello[6..8].copy_from_slice(&pol_address[2..4]);
    hello[8..12].copy_from_slice(&pol_address[4..8]);
    hello
}

/// Perform exactly one fixed transaction on a fresh, already connected channel.
/// The caller owns connection/timeout/close policy and must close the channel
/// after this call, including errors: native uses a new connection per request.
/// `payload` excludes its4-byte checksum. Only installed fixed transaction
/// layouts are accepted; use the catalog functions for streaming transactions.
pub fn exchange<C: Read + Write>(
    channel: &mut C,
    params: &Parameters<'_>,
    tx: profile::Transaction,
    payload: &[u8],
) -> Result<Reply, ExchangeError> {
    let (request_len, success_len, detail_len) = match tx {
        profile::MEMBER_LOGIN => (60, 0, 0),
        profile::SELECT_SERVICE => (20, 128, 24),
        profile::ENTER_COMMUNITY => (36, 32, 32),
        _ => return Err(Error::InvalidFrame.into()),
    };
    if payload.len() != request_len {
        return Err(Error::InvalidFrame.into());
    }
    let (header, mut cipher) = begin_exchange(channel, params, tx, payload)?;
    if header.status != 0 && (header.status != 114 || detail_len == 0) {
        return Err(ExchangeError::Service {
            status: header.status,
            details: Zeroizing::new(Vec::new()),
        });
    }
    let body_len = if header.status == 114 {
        detail_len
    } else {
        success_len
    };
    if body_len == 0 {
        //1001e70d closes immediately after the member-login reply header.
        return Ok(Reply {
            header,
            payload: Zeroizing::new(Vec::new()),
        });
    }
    // Native selects fixed lengths. Reject an inconsistent success frame before
    // reading or allocating from a server-controlled length.
    if header.status == 0 && header.body_length != body_len as u32 {
        return Err(Error::InvalidFrame.into());
    }
    let mut body = Zeroizing::new(vec![0; body_len]);
    channel.read_exact(&mut body)?;
    let mut plaintext = Zeroizing::new(cipher.stream(&body, StreamReset::Continue));
    if header.status == 0 {
        header.verified_payload(&plaintext)?;
    } else {
        // Native status handling returns before recording header body length;
        // its special error-detail reader nevertheless verifies a fixed body.
        let split = plaintext.len() - 4;
        let expected = u32::from_le_bytes(plaintext[split..].try_into().unwrap());
        if crypto::checksum(&plaintext[..split], 0) != expected {
            return Err(Error::Checksum.into());
        }
    }
    let len = plaintext.len() - 4;
    plaintext.truncate(len);
    if header.status != 0 {
        return Err(ExchangeError::Service {
            status: header.status,
            details: plaintext,
        });
    }
    Ok(Reply {
        header,
        payload: plaintext,
    })
}

/// Exchange the plaintext hello, authenticated request, and encrypted reply
/// header. The returned cipher is positioned immediately before the body.
fn begin_exchange<C: Read + Write>(
    channel: &mut C,
    params: &Parameters<'_>,
    tx: profile::Transaction,
    payload: &[u8],
) -> Result<(profile::ReplyHeader, PolBlowfish), ExchangeError> {
    if params.member_secret.len() > 15 || params.member_secret.contains(&0) {
        return Err(Error::InvalidFrame.into());
    }
    channel.write_all(&connect_hello(params.pol_address))?;
    channel.flush()?;
    let mut raw_header = Zeroizing::new([0; 24]);
    channel.read_exact(&mut raw_header[..])?;
    let connected = profile::ReplyHeader::parse(&raw_header[..])?;
    if connected.status != 0 {
        return Err(ExchangeError::Service {
            status: connected.status,
            details: Zeroizing::new(Vec::new()),
        });
    }
    let frame = profile::request(
        tx,
        payload,
        params.member_id,
        params.member_secret,
        connected.nonce,
    )?;
    let mut cipher = PolBlowfish::new(params.session_key, params.iv);
    let encrypted_header = Zeroizing::new(cipher.stream(&frame[..40], StreamReset::Boundary));
    let encrypted_body = Zeroizing::new(cipher.stream(&frame[40..], StreamReset::Continue));
    channel.write_all(&encrypted_header)?;
    channel.write_all(&encrypted_body)?;
    channel.flush()?;

    channel.read_exact(&mut raw_header[..])?;
    let plaintext_header = Zeroizing::new(cipher.stream(&raw_header[..], StreamReset::Boundary));
    let header = profile::ReplyHeader::parse(&plaintext_header)?;
    Ok((header, cipher))
}

/// Decrypt exactly the bytes requested by a catalog reader. Never buffers
/// ahead, so count exhaustion leaves native's unread trailer on the channel.
struct DecryptingRead<'a, C> {
    channel: &'a mut C,
    cipher: PolBlowfish,
}

impl<C: Read> Read for DecryptingRead<'_, C> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut encrypted = Zeroizing::new([0u8; 4096]);
        let wanted = output.len().min(encrypted.len());
        let n = self.channel.read(&mut encrypted[..wanted])?;
        let plaintext = Zeroizing::new(self.cipher.stream(&encrypted[..n], StreamReset::Continue));
        output[..n].copy_from_slice(&plaintext);
        Ok(n)
    }
}

/// ContentID transaction (2,3) on a fresh connected channel. The prefix's
/// count determines record consumption; native does not compare body_length
/// or read a trailing checksum here. Caller owns timeout and close policy.
pub fn exchange_content_ids<C: Read + Write>(
    channel: &mut C,
    params: &Parameters<'_>,
    maximum_records: usize,
) -> Result<CatalogReply<catalog::ContentCatalog>, ExchangeError> {
    let (header, cipher) = begin_exchange(channel, params, catalog::CONTENT_IDS, &[])?;
    if header.status != 0 {
        return Err(ExchangeError::Service {
            status: header.status,
            details: Zeroizing::new(Vec::new()),
        });
    }
    let mut reader = DecryptingRead { channel, cipher };
    let catalog = catalog::read_content_ids(&mut reader, maximum_records)?;
    Ok(CatalogReply { header, catalog })
}

/// Service catalog transaction (0,9), with its one-byte count and native
/// invalid-slot omission. Uses the same caller-owned connection policy.
pub fn exchange_services<C: Read + Write>(
    channel: &mut C,
    params: &Parameters<'_>,
) -> Result<CatalogReply<catalog::ServiceCatalog>, ExchangeError> {
    let (header, cipher) = begin_exchange(channel, params, catalog::SERVICES, &[])?;
    if header.status != 0 {
        return Err(ExchangeError::Service {
            status: header.status,
            details: Zeroizing::new(Vec::new()),
        });
    }
    let mut reader = DecryptingRead { channel, cipher };
    let catalog = catalog::read_services(&mut reader)?;
    Ok(CatalogReply { header, catalog })
}

/// Selection map transaction (1,3), with8-byte prefix and104-byte records.
/// Count-driven consumption preserves cipher continuity and leaves trailers
/// unread. An invalid applied subslot is rejected without changing any map.
pub fn exchange_selection_map<C: Read + Write>(
    channel: &mut C,
    params: &Parameters<'_>,
) -> Result<CatalogReply<catalog::SelectionMapCatalog>, ExchangeError> {
    let (header, cipher) = begin_exchange(channel, params, catalog::SELECTION_MAP, &[])?;
    if header.status != 0 {
        return Err(ExchangeError::Service {
            status: header.status,
            details: Zeroizing::new(Vec::new()),
        });
    }
    let mut reader = DecryptingRead { channel, cipher };
    let catalog = catalog::read_selection_map(&mut reader)?;
    Ok(CatalogReply { header, catalog })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // Independent read/write buffers and deliberately fragmented IO exercise
    // exact framing through Read::read_exact and Write::write_all.
    struct Channel {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        max_chunk: usize,
    }
    impl Read for Channel {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(self.max_chunk);
            self.input.read(&mut out[..n])
        }
    }
    impl Write for Channel {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = bytes.len().min(self.max_chunk);
            self.output.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    const KEY: [u8; 8] = [0x43; 8];
    const ADDRESS: [u8; 20] = [0x25; 20];
    const NONCE: [u8; 4] = [1, 3, 5, 7];
    fn parameters() -> Parameters<'static> {
        Parameters {
            member_id: b"EFKAOYMJ",
            member_secret: b"synthetic",
            pol_address: &ADDRESS,
            session_key: &KEY,
            iv: (0x12345678, 0x87654321),
        }
    }
    fn channel(status: u8, body: Option<Vec<u8>>, declared: u32) -> Channel {
        let p = parameters();
        let mut bytes = vec![0; 24];
        bytes[20..24].copy_from_slice(&NONCE);
        let mut header = [0; 24];
        header[1] = status;
        header[4..8].copy_from_slice(&declared.to_le_bytes());
        header[8..12].copy_from_slice(&0x01020304u32.to_le_bytes());
        let mut server_cipher = PolBlowfish::new(&KEY, p.iv);
        bytes.extend(server_cipher.stream(&header, StreamReset::Boundary));
        if let Some(body) = body {
            bytes.extend(server_cipher.stream(&body, StreamReset::Continue));
        }
        Channel {
            input: Cursor::new(bytes),
            output: Vec::new(),
            max_chunk: 3,
        }
    }
    fn seal(payload: &[u8]) -> Vec<u8> {
        let mut body = payload.to_vec();
        body.extend(crypto::checksum(payload, 0).to_le_bytes());
        body
    }

    #[test]
    fn fragmented_community_preserves_direction_and_header_body_boundaries() {
        let p = parameters();
        let request_payload = [0x37; 36];
        let response_payload: Vec<u8> = (0..28).collect();
        let mut channel = channel(0, Some(seal(&response_payload)), 32);
        let reply = exchange(&mut channel, &p, profile::ENTER_COMMUNITY, &request_payload).unwrap();
        assert_eq!(&reply.payload[..], response_payload);
        assert_eq!(reply.header.redirect, 0x01020304);
        let mut hello = [0; 40];
        hello[4] = 1;
        hello[6..12].fill(0x25);
        assert_eq!(&channel.output[..40], &hello);
        // Decrypt the complete outgoing request in one operation, so an
        // accidental reset between its header and body cannot pass this test.
        let mut receiver = PolBlowfish::new(&KEY, p.iv);
        let decoded = receiver.stream(&channel.output[40..], StreamReset::Boundary);
        let expected = profile::request(
            profile::ENTER_COMMUNITY,
            &request_payload,
            p.member_id,
            p.member_secret,
            NONCE,
        )
        .unwrap();
        assert_eq!(decoded, *expected);
        assert_eq!(
            channel.input.position() as usize,
            channel.input.get_ref().len()
        );
    }

    #[test]
    fn member_login_does_not_wait_for_a_success_body() {
        let mut channel = channel(0, None, 0);
        let reply = exchange(&mut channel, &parameters(), profile::MEMBER_LOGIN, &[0; 60]).unwrap();
        assert!(reply.payload.is_empty());
        assert_eq!(channel.output.len(), 40 + 40 + 64);
    }

    #[test]
    fn select_service_has_native_fixed_success_and_special_error_sizes() {
        let mut good = channel(0, Some(seal(&[7; 124])), 128);
        assert_eq!(
            exchange(&mut good, &parameters(), profile::SELECT_SERVICE, &[0; 20])
                .unwrap()
                .payload
                .len(),
            124
        );
        let mut refused = channel(114, Some(seal(&[9; 20])), 0);
        match exchange(
            &mut refused,
            &parameters(),
            profile::SELECT_SERVICE,
            &[0; 20],
        ) {
            Err(ExchangeError::Service {
                status: 114,
                details,
            }) => assert_eq!(&details[..], &[9; 20]),
            _ => panic!("expected fixed service refusal"),
        }
    }

    #[test]
    fn refuses_catalog_before_io_and_stops_on_connect_refusal() {
        let mut unsupported = channel(0, None, 0);
        assert!(matches!(
            exchange(&mut unsupported, &parameters(), profile::CONTENT_IDS, &[]),
            Err(ExchangeError::Protocol(Error::InvalidFrame))
        ));
        assert!(unsupported.output.is_empty());
        assert_eq!(unsupported.input.position(), 0);
        let mut refusal = channel(0, None, 0);
        refusal.input.get_mut()[1] = 180;
        assert!(matches!(
            exchange(&mut refusal, &parameters(), profile::MEMBER_LOGIN, &[0; 60]),
            Err(ExchangeError::Service { status: 180, .. })
        ));
        assert_eq!(refusal.output.len(), 40);
    }

    #[test]
    fn invalid_secret_is_rejected_before_any_io() {
        let mut channel = channel(0, None, 0);
        let mut params = parameters();
        params.member_secret = b"a\0b";
        assert!(matches!(
            exchange(&mut channel, &params, profile::MEMBER_LOGIN, &[0; 60]),
            Err(ExchangeError::Protocol(Error::InvalidFrame))
        ));
        assert!(channel.output.is_empty());
        assert_eq!(channel.input.position(), 0);
    }

    #[test]
    fn rejects_truncation_checksum_and_unexpected_fixed_length() {
        let mut short = channel(0, Some(vec![0; 31]), 32);
        assert!(
            matches!(exchange(&mut short, &parameters(), profile::ENTER_COMMUNITY, &[0; 36]),
            Err(ExchangeError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
        let mut bad_body = seal(&[3; 28]);
        bad_body[0] ^= 1;
        let mut corrupt = channel(0, Some(bad_body), 32);
        assert!(matches!(
            exchange(
                &mut corrupt,
                &parameters(),
                profile::ENTER_COMMUNITY,
                &[0; 36]
            ),
            Err(ExchangeError::Protocol(Error::Checksum))
        ));
        let mut oversized = channel(0, None, u32::MAX);
        assert!(matches!(
            exchange(
                &mut oversized,
                &parameters(),
                profile::ENTER_COMMUNITY,
                &[0; 36]
            ),
            Err(ExchangeError::Protocol(Error::InvalidFrame))
        ));
    }

    #[test]
    fn fragmented_content_catalog_preserves_stream_and_leaves_trailer_unread() {
        let mut body = vec![2, 0, 0, 0, 0xaa, 0xbb, 0xcc, 0xdd];
        for index in [3, 9] {
            let mut record = [0u8; catalog::CONTENT_RECORD_LEN];
            record[8] = index;
            record[0x10..0x18].copy_from_slice(&(0x12345678u64 + u64::from(index)).to_le_bytes());
            record[0x18..0x1c].copy_from_slice(b"Name");
            body.extend_from_slice(&record);
        }
        let consumed_body = body.len();
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        // Deliberately inconsistent header length: native uses prefix count.
        let mut channel = channel(0, Some(body), 1);
        let p = parameters();
        let reply = exchange_content_ids(&mut channel, &p, 2).unwrap();
        assert_eq!(reply.header.body_length, 1);
        assert_eq!(
            &reply.catalog.prefix[..],
            &[2, 0, 0, 0, 0xaa, 0xbb, 0xcc, 0xdd]
        );
        assert_eq!(
            reply
                .catalog
                .records
                .iter()
                .map(|r| r.index())
                .collect::<Vec<_>>(),
            [3, 9]
        );
        assert_eq!(reply.catalog.records[1].identifier(), 0x12345681);
        assert_eq!(reply.catalog.records[0].name_bytes(), b"Name");
        assert_eq!(channel.input.position() as usize, 48 + consumed_body);
        assert_eq!(
            channel.input.get_ref().len() - channel.input.position() as usize,
            4
        );
        let mut receiver = PolBlowfish::new(&KEY, p.iv);
        let request = receiver.stream(&channel.output[40..], StreamReset::Boundary);
        assert_eq!(
            request,
            *profile::request(
                catalog::CONTENT_IDS,
                &[],
                p.member_id,
                p.member_secret,
                NONCE
            )
            .unwrap()
        );
        assert_eq!(channel.output.len(), 80);
    }

    #[test]
    fn fragmented_service_catalog_uses_byte_count_and_consumes_ignored_slots() {
        // Nonzero remaining prefix bytes must never inflate the byte count.
        let mut body = vec![2, 0xff, 0xff, 0xff, 1, 2, 3, 4];
        for slot in [8, 64] {
            let mut record = [0u8; catalog::SERVICE_RECORD_LEN];
            record[0] = slot;
            record[16..20].copy_from_slice(b"Test");
            record[32..34].copy_from_slice(&0x1234u16.to_le_bytes());
            body.extend_from_slice(&record);
        }
        let consumed_body = body.len();
        body.extend_from_slice(&[0xa5; 4]);
        let mut channel = channel(0, Some(body), u32::MAX);
        channel.max_chunk = 1;
        let reply = exchange_services(&mut channel, &parameters()).unwrap();
        assert_eq!(reply.catalog.prefix[0], 2);
        assert_eq!(reply.catalog.records.len(), 1);
        assert_eq!(reply.catalog.ignored_records, 1);
        assert_eq!(reply.catalog.records[0].slot(), 8);
        assert_eq!(
            reply.catalog.records[0]
                .description_units()
                .collect::<Vec<_>>(),
            [0x1234]
        );
        assert_eq!(channel.input.position() as usize, 48 + consumed_body);
        assert_eq!(
            channel.input.get_ref().len() - channel.input.position() as usize,
            4
        );
    }

    #[test]
    fn catalog_refusal_limits_truncation_and_invalid_credentials_stop_at_boundaries() {
        let mut refused = channel(114, Some(vec![0xa5; 16]), 16);
        assert!(matches!(
            exchange_services(&mut refused, &parameters()),
            Err(ExchangeError::Service { status: 114, .. })
        ));
        assert_eq!(refused.input.position(), 48);

        let mut limited = channel(0, Some(vec![2, 0, 0, 0, 0, 0, 0, 0]), 0);
        assert!(matches!(
            exchange_content_ids(&mut limited, &parameters(), 1),
            Err(ExchangeError::Catalog(catalog::CatalogError::CountLimit {
                count: 2,
                maximum: 1
            }))
        ));
        assert_eq!(limited.input.position(), 56);

        let mut short = channel(0, Some(vec![1, 0, 0, 0, 0, 0, 0, 0, 9]), 0);
        assert!(matches!(exchange_content_ids(&mut short, &parameters(), 1),
            Err(ExchangeError::Catalog(catalog::CatalogError::Io(e)))
                if e.kind() == io::ErrorKind::UnexpectedEof));

        let mut params = parameters();
        for secret in [&b"a\0b"[..], &b"sixteen-byte-key"[..]] {
            params.member_secret = secret;
            let mut invalid = channel(0, None, 0);
            assert!(matches!(
                exchange_content_ids(&mut invalid, &params, 0),
                Err(ExchangeError::Protocol(Error::InvalidFrame))
            ));
            assert!(matches!(
                exchange_services(&mut invalid, &params),
                Err(ExchangeError::Protocol(Error::InvalidFrame))
            ));
            assert!(invalid.output.is_empty());
            assert_eq!(invalid.input.position(), 0);
        }
        let error = ExchangeError::Catalog(catalog::CatalogError::Io(io::Error::new(
            io::ErrorKind::Other,
            "sensitive caller diagnostic",
        )));
        assert!(!format!("{error}").contains("sensitive"));
        assert!(!format!("{error:?}").contains("sensitive"));
        let stalled =
            ExchangeError::Catalog(catalog::CatalogError::Io(io::ErrorKind::TimedOut.into()));
        assert_eq!(
            stalled.to_string(),
            "catalog channel read failed (timed out)"
        );
    }

    #[test]
    fn encrypted_selection_map_has_exact_count_consumption_and_transaction() {
        let mut body = vec![2, 255, 255, 255, 1, 2, 3, 4];
        for primary in [63, 64] {
            let mut record = [0u8; catalog::SELECTION_MAP_RECORD_LEN];
            record[0] = primary;
            record[4..7].copy_from_slice(&[1, 7, 3]);
            body.extend(record);
        }
        let body_len = body.len();
        body.extend([0xa5; 4]);
        let mut channel = channel(0, Some(body), u32::MAX);
        channel.max_chunk = 1;
        let params = parameters();
        let reply = exchange_selection_map(&mut channel, &params).unwrap();
        assert_eq!(
            (reply.catalog.records.len(), reply.catalog.ignored_records),
            (1, 1)
        );
        let mut map = catalog::SelectionMap::default();
        map.refresh(&reply.catalog).unwrap();
        let entry = map.entry(7, 3).unwrap();
        assert!(entry.active() && !entry.high_bit());
        assert_eq!(entry.selector(), 63);
        assert_eq!(channel.input.position() as usize, 48 + body_len);
        assert_eq!(
            channel.input.get_ref().len() - channel.input.position() as usize,
            4
        );
        let mut receiver = PolBlowfish::new(&KEY, params.iv);
        let request = receiver.stream(&channel.output[40..], StreamReset::Boundary);
        assert_eq!(
            request,
            *profile::request(
                catalog::SELECTION_MAP,
                &[],
                params.member_id,
                params.member_secret,
                NONCE
            )
            .unwrap()
        );
    }

    #[test]
    fn selection_map_invalid_applied_link_refusal_and_credentials_fail_cleanly() {
        let mut body = vec![1, 0, 0, 0, 0, 0, 0, 0];
        let mut record = [0; catalog::SELECTION_MAP_RECORD_LEN];
        record[4..7].copy_from_slice(&[1, 0, 8]);
        body.extend(record);
        let mut invalid = channel(0, Some(body), 0);
        assert!(matches!(
            exchange_selection_map(&mut invalid, &parameters()),
            Err(ExchangeError::Catalog(
                catalog::CatalogError::InvalidMapLink
            ))
        ));
        assert_eq!(
            invalid.input.position() as usize,
            48 + catalog::PREFIX_LEN + catalog::SELECTION_MAP_RECORD_LEN
        );
        let mut refused = channel(114, Some(vec![0xa5; 16]), 16);
        assert!(matches!(
            exchange_selection_map(&mut refused, &parameters()),
            Err(ExchangeError::Service { status: 114, .. })
        ));
        assert_eq!(refused.input.position(), 48);
        let mut params = parameters();
        params.member_secret = b"a\0b";
        let mut invalid_secret = channel(0, None, 0);
        assert!(matches!(
            exchange_selection_map(&mut invalid_secret, &params),
            Err(ExchangeError::Protocol(Error::InvalidFrame))
        ));
        assert!(invalid_secret.output.is_empty());
        assert_eq!(invalid_secret.input.position(), 0);
    }
}
