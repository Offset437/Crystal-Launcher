//! Fresh-session selection(4,6) -> FFXI lobby community(4,5), installed polcore73b1864b.
//!
//! Selection success1001d658..1001d769 consumes124 payload bytes after transport
//! has verified its checksum. Initial values1007541c..28 / reset1001c9d0 are
//! slot=-1, primary=-1, type=4, class=1000, context=0, flag=0.
//! FFXiMain100edb1b..26 ->10310067 ->core+30c/1001db60 supplies
//! (-2,-1,0,-2,-1,1): retain slot/type/flag, absent primary, context0, class1.
//!
//! This API is deliberately for a fresh driver that fetched handles(0,9) and
//! made no local handle-state setter calls.1001e0b4..ca zeroes all64 bytes at
//!100aaae0; record kind is stored elsewhere, at10405818+slot*40. The native
//! type3 gate1001dab4 tests that separate zeroed array, NOT record kind.
//! Native setter1001c903 can subsequently change a gate to1/2/3; this module
//! does not model those UI mutations or resume an existing POL session.
//!
//! No name-based catalog selection, networking, authentication, or cached keep
//! claim occurs here. Opaque reply bytes remain intact and are zeroed on drop.

use crate::{
    assembly::CommunityRequest,
    catalog::{SelectionMap, ServiceCatalog},
};
use zeroize::Zeroizing;

pub const SELECTION_PAYLOAD_LEN: usize = 124;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    PayloadLength,
    HandleOutOfRange,
    SubslotOutOfRange,
    TypeOutOfRange,
    TypeThreeWithoutHandle,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PayloadLength => "selection payload must contain exactly124 bytes",
            Self::HandleOutOfRange => "selection handle at byte0 is outside0..63",
            Self::SubslotOutOfRange => "applied selection subslot at byte2 is outside0..7",
            Self::TypeOutOfRange => "selection type-plus-one at byte6 is outside0..5",
            Self::TypeThreeWithoutHandle => "native community type3 requires a retained handle",
        })
    }
}
impl std::error::Error for Error {}

/// Typed native fields plus the unmodified payload. No Debug implementation:
/// the opaque response may contain account-related bytes.
pub struct SelectionState {
    raw: Option<Zeroizing<[u8; SELECTION_PAYLOAD_LEN]>>,
    handle: Option<u8>,
    primary: Option<u8>,
    selection_type: u8,
    content_class: u16,
    context: bool,
    flag: u8,
}
impl Default for SelectionState {
    fn default() -> Self {
        Self {
            raw: None,
            handle: None,
            primary: None,
            selection_type: 4,
            content_class: 1000,
            context: false,
            flag: 0,
        }
    }
}
impl SelectionState {
    /// Apply the successful124-byte payload atomically. Unlike native stale-
    /// state retention on malformed indices, reject invalid applied fields.
    /// Unused handle/subslot bytes are opaque and need not be zero or in range.
    pub fn apply(&mut self, payload: &[u8], map: &SelectionMap) -> Result<(), Error> {
        if payload.len() != SELECTION_PAYLOAD_LEN {
            return Err(Error::PayloadLength);
        }
        if payload[6] > 5 {
            return Err(Error::TypeOutOfRange);
        }
        let (handle, selection_type) = if payload[6] == 0 {
            (None, 4)
        } else {
            if payload[0] >= 64 {
                return Err(Error::HandleOutOfRange);
            }
            (Some(payload[0]), payload[6] - 1)
        };
        let primary = if payload[1] != 0 {
            if let Some(handle) = handle {
                let entry = map
                    .entry(usize::from(handle), usize::from(payload[2]))
                    .ok_or(Error::SubslotOutOfRange)?;
                //1001d717..23 ignores active bit0 and high bit7, unlike the
                // later community map search. Do not invent an active gate.
                Some(entry.selector())
            } else {
                None
            }
        } else {
            None
        };
        let mut raw = Zeroizing::new([0; SELECTION_PAYLOAD_LEN]);
        raw.copy_from_slice(payload);
        self.raw = Some(raw);
        self.handle = handle;
        self.primary = primary;
        self.selection_type = selection_type;
        self.content_class = u16::from_le_bytes([payload[4], payload[5]]);
        self.context = payload[3] == 1;
        self.flag = payload[7];
        Ok(())
    }

    pub fn parse(payload: &[u8], map: &SelectionMap) -> Result<Self, Error> {
        let mut state = Self::default();
        state.apply(payload, map)?;
        Ok(state)
    }
    pub fn raw_payload(&self) -> Option<&[u8; SELECTION_PAYLOAD_LEN]> {
        self.raw.as_deref()
    }
    pub fn handle(&self) -> Option<u8> {
        self.handle
    }
    pub fn primary(&self) -> Option<u8> {
        self.primary
    }
    pub fn selection_type(&self) -> u8 {
        self.selection_type
    }
    pub fn content_class(&self) -> u16 {
        self.content_class
    }
    pub fn context(&self) -> bool {
        self.context
    }
    /// Retain the complete native integer byte; the request encodes!=0.
    pub fn flag(&self) -> u8 {
        self.flag
    }
    pub fn ancillary_words(&self) -> Option<[u32; 2]> {
        self.raw.as_ref().map(|r| {
            [
                u32::from_le_bytes(r[8..12].try_into().unwrap()),
                u32::from_le_bytes(r[12..16].try_into().unwrap()),
            ]
        })
    }
    pub fn ancillary_flag(&self) -> bool {
        self.raw.as_ref().is_some_and(|r| r[118] & 1 != 0)
    }

    /// Build the exact FFXI lobby call after a fresh handles(0,9) fetch.
    /// The catalog is explicit to document that prerequisite; its kind field
    /// must NOT be mistaken for the independent native handle-state gate.
    pub fn lobby_request(&self, _handles: &ServiceCatalog) -> Result<[u8; 36], Error> {
        if self.selection_type == 3 && self.handle.is_none() {
            return Err(Error::TypeThreeWithoutHandle);
        }
        Ok(CommunityRequest {
            world_id: self.handle.unwrap_or(0), // native retained slot, no world-name inference
            service_available: false, // explicit primary=-1 cannot match a six-bit map selector
            service_index: 0,
            context_default: true, // explicit context0 -> wire!context=1
            region: 1, // this legacy field name encodes content class, NOT geographic region
            world_index_plus1: if self.handle.is_some() {
                self.selection_type + 1
            } else {
                0
            },
            flag_d8: self.flag != 0,
            cached: false, // explicit primary/context/class clear native keep, even if unchanged
        }
        .payload())
    }
}

/// Exact length excludes the four-byte transport checksum, already verified by
/// the profile response reader. `handles` and `map` are from the same fresh run.
pub fn build_lobby_request(
    selection_payload: &[u8],
    map: &SelectionMap,
    handles: &ServiceCatalog,
) -> Result<[u8; 36], Error> {
    SelectionState::parse(selection_payload, map)?.lobby_request(handles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ServiceRecord;

    fn handles() -> ServiceCatalog {
        ServiceCatalog {
            prefix: Zeroizing::new([0; 8]),
            records: Vec::new(),
            ignored_records: 0,
        }
    }
    fn reply(handle: u8, kind_plus_one: u8) -> [u8; 124] {
        let mut p = [0x5a; 124];
        p[0] = handle;
        p[1] = 1;
        p[2] = 7;
        p[3] = 1;
        p[4..6].copy_from_slice(&1000u16.to_le_bytes());
        p[6] = kind_plus_one;
        p[7] = 0xff;
        p[8..12].copy_from_slice(&0x12345678u32.to_le_bytes());
        p[12..16].copy_from_slice(&0x90abcdefu32.to_le_bytes());
        p[118] = 0x81;
        p
    }
    #[test]
    fn loader_defaults_are_native_initial_state() {
        let s = SelectionState::default();
        assert_eq!(
            (
                s.handle(),
                s.primary(),
                s.selection_type(),
                s.content_class(),
                s.context(),
                s.flag()
            ),
            (None, None, 4, 1000, false, 0)
        );
        assert!(s.raw_payload().is_none());
        let p = s.lobby_request(&handles()).unwrap();
        assert_eq!(&p[16..26], &[0, 0, 0, 1, 1, 0, 0, 0, 0, 1]);
    }
    #[test]
    fn preserves_reply_and_restores_slot_type_but_overrides_lobby_fields() {
        let mut bytes = [[0; 8]; 64];
        bytes[63][7] = 0xff;
        let map = SelectionMap::from_bytes(&bytes);
        let raw = reply(63, 5);
        let s = SelectionState::parse(&raw, &map).unwrap();
        assert_eq!(s.raw_payload(), Some(&raw));
        assert_eq!(
            (
                s.handle(),
                s.primary(),
                s.selection_type(),
                s.context(),
                s.flag()
            ),
            (Some(63), Some(63), 4, true, 255)
        );
        assert_eq!(s.ancillary_words(), Some([0x12345678, 0x90abcdef]));
        assert!(s.ancillary_flag());
        // Independently pinned native field stores1001d9b8..1001dae6.
        let mut expected = [0; 36];
        expected[16..26].copy_from_slice(&[63, 0, 0, 1, 1, 0, 5, 1, 0, 1]);
        assert_eq!(s.lobby_request(&handles()).unwrap(), expected);
        assert_eq!(
            build_lobby_request(&raw, &map, &handles()).unwrap(),
            expected
        );
    }
    #[test]
    fn selection_ignores_map_active_and_high_bits() {
        for byte in [0x00, 0x01, 0x7e, 0x7f, 0x80, 0x81, 0xfe, 0xff] {
            let mut bytes = [[0; 8]; 64];
            bytes[0][7] = byte;
            let s = SelectionState::parse(&reply(0, 1), &SelectionMap::from_bytes(&bytes)).unwrap();
            assert_eq!(s.primary(), Some((byte >> 1) & 63));
            assert_eq!(
                &s.lobby_request(&handles()).unwrap()[16..26],
                &[0, 0, 0, 1, 1, 0, 1, 1, 0, 1]
            );
        }
    }
    #[test]
    fn absent_selection_resets_and_ignores_unused_indices() {
        let map = SelectionMap::default();
        let mut s = SelectionState::parse(&reply(63, 2), &map).unwrap();
        let mut raw = reply(255, 0);
        raw[2] = 255;
        raw[3] = 2;
        raw[7] = 0;
        s.apply(&raw, &map).unwrap();
        assert_eq!(
            (s.handle(), s.primary(), s.selection_type(), s.context()),
            (None, None, 4, false)
        );
        assert_eq!(
            &s.lobby_request(&handles()).unwrap()[16..26],
            &[0, 0, 0, 1, 1, 0, 0, 0, 0, 1]
        );
        raw = reply(0, 1);
        raw[1] = 0;
        raw[2] = 255;
        s.apply(&raw, &map).unwrap();
        assert_eq!(s.primary(), None);
    }
    #[test]
    fn validates_applied_bounds_atomically() {
        let map = SelectionMap::default();
        let good = reply(63, 5);
        let mut state = SelectionState::parse(&good, &map).unwrap();
        for (offset, value, error) in [
            (0, 64, Error::HandleOutOfRange),
            (0, 255, Error::HandleOutOfRange),
            (2, 8, Error::SubslotOutOfRange),
            (2, 255, Error::SubslotOutOfRange),
            (6, 6, Error::TypeOutOfRange),
            (6, 255, Error::TypeOutOfRange),
        ] {
            let mut bad = good;
            bad[offset] = value;
            assert_eq!(state.apply(&bad, &map), Err(error));
            assert_eq!(state.raw_payload(), Some(&good));
        }
        assert_eq!(state.apply(&good[..123], &map), Err(Error::PayloadLength));
        assert_eq!(state.apply(&[0; 125], &map), Err(Error::PayloadLength));
    }
    #[test]
    fn fresh_type_three_gate_is_not_handle_record_kind() {
        let mut catalog = handles();
        let mut record = [0; 136];
        record[0] = 7;
        record[2] = 1;
        catalog.records.push(ServiceRecord::parse(&record).unwrap());
        catalog.prefix[0] = 1;
        let result = build_lobby_request(&reply(7, 4), &SelectionMap::default(), &catalog).unwrap();
        assert_eq!(&result[16..26], &[7, 0, 0, 1, 1, 0, 4, 1, 0, 1]);
        // The native absent/type3 rejection remains explicit even though a valid
        // reply with byte6=0 normalizes type to4 before reaching this branch.
        let mut invalid = SelectionState::default();
        invalid.selection_type = 3;
        assert_eq!(
            invalid.lobby_request(&catalog),
            Err(Error::TypeThreeWithoutHandle)
        );
    }
    #[test]
    fn context_flag_and_class_are_not_conflated() {
        for context in [0, 1, 2, 255] {
            for flag in [0, 1, 2, 255] {
                let mut raw = reply(1, 2);
                raw[3] = context;
                raw[7] = flag;
                raw[4..6].copy_from_slice(&0xffffu16.to_le_bytes());
                let state = SelectionState::parse(&raw, &SelectionMap::default()).unwrap();
                assert_eq!(state.context(), context == 1);
                assert_eq!(state.content_class(), 65535);
                let body = state.lobby_request(&handles()).unwrap();
                assert_eq!(&body[19..24], &[1, 1, 0, 2, u8::from(flag != 0)]);
                assert!(body[..16].iter().chain(body[26..].iter()).all(|b| *b == 0));
                assert_eq!(body[24], 0);
            }
        }
    }
}
