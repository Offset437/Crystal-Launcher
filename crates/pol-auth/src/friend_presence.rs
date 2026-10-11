//! POL's normal NOTICE callback, traced from installed polcore 73b1864b.
//! 1001b640 decodes a 96-character/72-byte header; 1001b6f0 dispatches
//! friend updates. 100250b0/100250f0 require matching ID, handle and a newer
//! timestamp after the list loads. No requests, message sending or login.

use crate::{friends::{Friends, A_SLOTS}, identity, rsa};
use zeroize::{Zeroize, Zeroizing};

const HEADER_CHARS: usize = 96;
const ID_XOR: u64 = 0x1c27_3e45_6789_1133;

#[derive(Clone)]
pub(crate) struct Notice {
    pub(crate) identifier: u64,
    pub(crate) index: u8,
    pub(crate) handle: u8,
    pub(crate) timestamp: u64,
    pub(crate) status: Option<[u8; 8]>,
    pub(crate) subrecords: Option<[u8; 128]>,
    pub(crate) selected: Option<[u8; 16]>,
    pub(crate) selected_slots: [Option<[u8; 16]>; 8],
    pub(crate) name: Option<[u8; 16]>,
    pub(crate) location: Option<[u8; 24]>,
}
impl Drop for Notice {
    fn drop(&mut self) {
        self.identifier.zeroize(); self.index.zeroize(); self.handle.zeroize(); self.timestamp.zeroize();
        self.status.zeroize(); self.subrecords.zeroize(); self.selected.zeroize(); self.selected_slots.zeroize(); self.name.zeroize(); self.location.zeroize();
    }
}

impl Notice {
    /// The complete checksummed decrypted chat frame. Channel notices, mail,
    /// lookups and ordinary textual notices never become friend updates.
    pub(crate) fn parse(line: &[u8]) -> Option<Self> {
        let body = identity::verified_line_body(line).ok()?;
        let (prefix, rest) = token(body)?;
        if !prefix.starts_with(b":") { return None; }
        let (command, rest) = token(rest)?;
        if !command.eq_ignore_ascii_case(b"NOTICE") { return None; }
        let (target, rest) = token(rest)?;
        if target.first() != Some(&b'U') || target.len() != 9 { return None; }
        identity::id_decode(&target[1..]).ok()?;
        Self::decode(rest.strip_prefix(b":")?)
    }

    fn decode(body: &[u8]) -> Option<Self> {
        let header = Zeroizing::new(rsa::b64_decode(body.get(..HEADER_CHARS)?, HEADER_CHARS));
        if header[0x42] & 1 == 0 || u16::from_le_bytes(header[0x3e..0x40].try_into().ok()?) & 0xf80 != 0xf80 {
            return None;
        }
        let index = header[0x1c];
        if usize::from(index) >= A_SLOTS || header[0x18] >= 64 { return None; }
        let whole = header[0x1b] != 0;
        if !whole && (header[0x1a] != 0 || header[0x19] & 1 == 0) { return None; }
        let encoded_len = u32::from_le_bytes(header[0x38..0x3c].try_into().ok()?) as usize;
        if (whole && encoded_len > 174) || (!whole && (encoded_len == 0 || encoded_len >= 344)) { return None; }
        let encoded = body.get(HEADER_CHARS..HEADER_CHARS.checked_add(encoded_len)?)?;
        let decoded = Zeroizing::new(rsa::b64_decode(encoded, encoded.len()));
        let mut out = Self {
            identifier: u64::from_le_bytes(header[..8].try_into().ok()?) ^ ID_XOR,
            index, handle: header[0x18],
            timestamp: u64::from_le_bytes(header[0x30..0x38].try_into().ok()?),
            status: None, subrecords: None, selected: None, selected_slots: [None;8], name: None, location: None,
        };
        if whole {
            out.status = Some(header[0x10..0x18].try_into().ok()?);
            if encoded_len != 0 { out.subrecords = Some(decoded.get(..128)?.try_into().ok()?); }
        } else {
            let flags = *decoded.first()?;
            // bit 1 is a separate user-lookup path (10029650), not list A.
            if flags & 2 != 0 { return None; }
            let mut at = 8;
            if flags & 1 != 0 { out.status = Some(header[0x10..0x18].try_into().ok()?); }
            if flags & 4 != 0 { take::<8>(&decoded, &mut at)?; }
            if flags & 8 != 0 { out.selected = Some(take::<16>(&decoded, &mut at)?); }
            if flags & 16 != 0 { out.name = Some(take::<16>(&decoded, &mut at)?); }
            if flags & 32 != 0 { take::<104>(&decoded, &mut at)?; } // UTF-16 profile text
            if flags & 64 != 0 {
                let text = decoded.get(at..)?;
                if text.is_empty() { return None; }
                let mut location = [0; 24];
                let len = text.len().min(23);
                location[..len].copy_from_slice(&text[..len]);
                out.location = Some(location);
            }
        }
        Some(out)
    }

    fn merge(&mut self, prior: &Self) {
        if self.subrecords.is_none() {
            self.selected_slots = prior.selected_slots;
            if let Some(sub) = prior.selected {
                let index = prior.status.map_or(0, |status| (status[2] >> 1) & 7);
                self.selected_slots[usize::from(index)] = Some(sub);
            }
            self.subrecords = prior.subrecords;
        }
        if self.status.is_none() { self.status = prior.status; }
        if self.name.is_none() { self.name = prior.name; }
        if self.location.is_none() { self.location = prior.location; }
    }
}

fn token(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let split = body.iter().position(|b| *b == b' ')?;
    Some((&body[..split], body[split..].trim_ascii_start()))
}
fn take<const N: usize>(bytes: &[u8], at: &mut usize) -> Option<[u8; N]> {
    let end = at.checked_add(N)?;
    let out = bytes.get(*at..end)?.try_into().ok()?;
    *at = end;
    Some(out)
}

/// At most one accumulated update per POL friend slot. Registration can
/// receive these before Get Friend List completes; retain them without
/// mistaking an unseen status for offline. Different IDs/handles never merge.
pub(crate) struct Feed {
    slots: Vec<Option<Notice>>,
    revision: u64,
}
impl Default for Feed {
    fn default() -> Self { Self { slots: (0..A_SLOTS).map(|_| None).collect(), revision: 0 } }
}
impl Feed {
    pub(crate) fn push(&mut self, mut notice: Notice) {
        let slot = &mut self.slots[usize::from(notice.index)];
        if let Some(prior) = slot {
            if prior.identifier == notice.identifier && prior.handle == notice.handle {
                if notice.timestamp <= prior.timestamp { return; }
                notice.merge(prior);
            }
        }
        *slot = Some(notice);
        self.revision = self.revision.wrapping_add(1);
    }
    pub(crate) fn revision(&self) -> u64 { self.revision }
    pub(crate) fn apply(&self, friends: &mut Friends, mask: u64) -> u64 {
        for notice in self.slots.iter().flatten() { friends.apply_notice(notice, mask); }
        self.revision
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::{ContentCatalog, ContentRecord, CONTENT_RECORD_LEN};

    pub(crate) fn frame(index: u8, handle: u8, id: u64, timestamp: u64, status: [u8;8], payload: &[u8], whole: bool) -> Zeroizing<Vec<u8>> {
        let mut header = [0;72];
        header[..8].copy_from_slice(&(id ^ ID_XOR).to_le_bytes());
        header[0x10..0x18].copy_from_slice(&status);
        header[0x18] = handle; header[0x1c] = index;
        header[if whole { 0x1b } else { 0x19 }] = 1;
        header[0x30..0x38].copy_from_slice(&timestamp.to_le_bytes());
        let encoded = rsa::b64_encode(payload);
        header[0x38..0x3c].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
        header[0x3e..0x40].copy_from_slice(&0xf80u16.to_le_bytes()); header[0x42] = 1;
        identity::frame_line(format!(":server NOTICE UEEEEEEEE :{}{}", rsa::b64_encode(&header), encoded).as_bytes()).unwrap()
    }
    pub(crate) fn friends() -> Friends {
        let mut wire = [0;CONTENT_RECORD_LEN];
        wire[0] = 0x20; wire[3] = 2; // wire bit25 -> native handle index 1
        wire[8] = 199; wire[0x10..0x18].copy_from_slice(&123u64.to_le_bytes());
        wire[0x18..0x1d].copy_from_slice(b"Alice");
        Friends::from_catalog(&ContentCatalog { prefix: Zeroizing::new([0;8]), records: vec![ContentRecord::parse(&wire).unwrap()] }, 55)
    }

    #[test]
    fn buffered_status_and_location_survive_list_refresh_and_newer_partial_updates() {
        let mut feed = Feed::default();
        let mut payload = vec![0;8]; payload[0] = 0x41; payload.extend_from_slice(b"Vana'diel\0");
        feed.push(Notice::parse(&frame(199,1,123,100,[2,3,4,7,1,0,0,0],&payload,false)).unwrap());
        let mut friends = friends();
        feed.apply(&mut friends, 55);
        let rec = friends.record(false,199).unwrap();
        // 1001ecb0: online=bit11, state=(byte1-1)<<13,
        // selected=byte2 bits1..3<<17, class=word4<<33, extra=byte3<<43.
        let q = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        assert_eq!(q & ((1<<11)|(1<<12)|(7<<13)|(1<<16)|(7<<17)|(0x3ff<<33)|(15<<43)),
            (1<<11)|(2<<13)|(2<<17)|(1<<33)|(7<<43));
        assert_eq!(friends.presence(false,199).0, Some(true));
        assert_eq!(friends.presence(false,199).1, Some(&b"Vana'diel"[..]));
        let mut payload = [0;8]; payload[0]=1;
        feed.push(Notice::parse(&frame(199,1,123,101,[0,1,0,0,100,0,0,0],&payload,false)).unwrap());
        feed.apply(&mut friends,55);
        assert_eq!(friends.presence(false,199).0, Some(false));
        assert_eq!(friends.presence(false,199).1, None);
        let mut refreshed = self::friends(); feed.apply(&mut refreshed,55);
        assert_eq!(refreshed.presence(false,199).0, Some(false));
    }

    #[test]
    fn wrong_friend_handle_old_updates_and_truncated_optional_fields_are_ignored() {
        let mut payload=[0;8]; payload[0]=1;
        let mut feed=Feed::default();
        feed.push(Notice::parse(&frame(199,1,999,200,[2;8],&payload,false)).unwrap());
        let mut friends=friends(); feed.apply(&mut friends,55);
        assert_eq!(friends.presence(false,199).0,None);
        feed.push(Notice::parse(&frame(199,2,123,201,[2;8],&payload,false)).unwrap());
        feed.apply(&mut friends,55); assert_eq!(friends.presence(false,199).0,None);
        feed.push(Notice::parse(&frame(199,1,123,202,[2;8],&payload,false)).unwrap());
        let rev=feed.revision();
        feed.push(Notice::parse(&frame(199,1,123,201,[0;8],&payload,false)).unwrap());
        assert_eq!(feed.revision(),rev);
        feed.apply(&mut friends,55); assert_eq!(friends.presence(false,199).0,Some(true));
        payload[0]=0x20;
        assert!(Notice::parse(&frame(199,1,123,203,[0;8],&payload,false)).is_none());
        assert!(Notice::parse(&frame(200,1,123,203,[0;8],&payload,false)).is_none());
        assert!(Notice::parse(b":server NOTICE user :opaque\r\n").is_none());
    }

    #[test]
    fn switching_selected_character_does_not_move_an_older_character_record() {
        let mut feed=Feed::default();
        let mut payload=vec![0;24]; payload[0]=9; payload[10..12].copy_from_slice(&1u16.to_le_bytes()); payload[12..16].copy_from_slice(&42u32.to_le_bytes());
        feed.push(Notice::parse(&frame(199,1,123,10,[2,1,4,0,1,0,0,0],&payload,false)).unwrap());
        let mut payload=[0;8]; payload[0]=1;
        feed.push(Notice::parse(&frame(199,1,123,11,[2,1,6,0,1,0,0,0],&payload,false)).unwrap());
        let mut friends=friends(); feed.apply(&mut friends,55);
        let rec=friends.record(false,199).unwrap();
        assert_eq!(rec[0x18+2*16]&1,1);
        assert_eq!(u32::from_le_bytes(rec[0x18+2*16+4..0x18+2*16+8].try_into().unwrap()),42);
        assert_eq!(&rec[0x18+3*16..0x18+4*16],&[0;16]);
    }
}
