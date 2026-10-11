//! Values a POL-less loader needs to start the retail game, and the in-game
//! presence (ChangeMyStatus) forwarding it asks the launcher to perform.
//!
//! Everything here is offline; the only network path is
//! `retail::Authenticated::send_presence`.
//! The public types are re-exported from `retail`.

use crate::{
    assembly::CommunityRequest, catalog::SelectionMap, keepalive::KeepaliveError,
    pol_records::PresenceState, transport::ExchangeError,
};
use std::io;
use zeroize::{Zeroize, Zeroizing};

/// The `v` field this crate writes.
pub const CONTRACT_VERSION: u32 = 1;
/// `{"type":"presence","payload":...}` decodes to exactly this many bytes.
pub const PRESENCE_ARGS_LEN: usize = 24;

/// POL service region. Only `Us` is verified end to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Us,
    Eu,
    Jp,
}
impl Region {
    /// Contract spelling: `"US"`, `"EU"`, `"JP"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Us => "US",
            Self::Eu => "EU",
            Self::Jp => "JP",
        }
    }
    /// Map the sign-in NICK area byte (`Options::nick_region`, env.dat
    /// `POL_UCS_AREA_KBN`). Only the installed US value 1 is verified; the
    /// EU/JP values are unknown, so they are refused rather than guessed.
    pub fn from_area_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Us),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoaderSessionError {
    /// The sign-in used a NICK area code with no verified region mapping.
    UnverifiedRegion(u8),
}
impl std::fmt::Display for LoaderSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnverifiedRegion(code) => {
                write!(f, "sign-in area code {code} has no verified POL region")
            }
        }
    }
}
impl std::error::Error for LoaderSessionError {}

/// Everything the contract's `session` line carries, zeroed on drop.
///
/// `pol_records`, when present, is exactly `pol_records::BLOB_LEN` (0x2420)
/// bytes, all integers little-endian:
///
/// | Offset | Size | Content |
/// |---|---|---|
/// | 0x0000 | 8 | magic `"XIPOLR"`, version byte `0x01`, `0x00` |
/// | 0x0008 | 24 | presence store, six `i32`: handle (slot 190), primary (189), context (204), type (206), flag, content class (207) |
/// | 0x0020 | 64 x 0x28 | handle table; record `i` is what slot 173(`i`) copies out |
/// | 0x0A20 | 64 x 0x68 | character table; record `i` is what slot 177(`i`) copies out |
///
/// Handle record (0x28): `q[0]` bit0 valid, bits 1..44 server flags, bits
/// 45..50 slot; `+0x08` name, 15 bytes + NUL; `d[0x18]` bits 0..1 status/kind,
/// bits 2..7 order; `d[0x1c]` server word; `+0x20` eight character-slot bytes
/// (bit0 used, bits 1..6 = character index).
///
/// Character record (0x68): `b[0]` bit0 valid; `w[2]` type (1 = FFXI);
/// `d[4]` packed character id `(id & 0xffff) | world << 16 | (id >> 16 & 0xff) << 24`;
/// `d[8]` content id; `d[0xc]` 0 for FFXI; `q[0x10]` bits 0..7 server flags,
/// bit 15 linked to a handle, bits 16..21 handle index; `+0x18` name
/// (15 + NUL); `+0x28` text (63 + NUL). Unused records are all zero.
///
/// The presence store is polcore's state after the sign-in's lobby
/// ChangeMyStatus; a loader should report it from slots 189/190/204/206/207
/// and treat slot 195's `-2` arguments as "keep" against it.
pub struct LoaderSession {
    pub region: Region,
    pub lang: Region,
    /// Not known to the sign-in. The launcher fills it before
    /// `to_contract_json`; `None` omits the field.
    pub install_dir: Option<String>,
    /// polcore session value V (slot 1003).
    pub v_key: [u8; 16],
    /// The 0x34 meaningful authCode bytes (slot 936).
    pub auth_code: [u8; 52],
    pub pol_records: Option<Vec<u8>>,
    /// Content IDs of the account's FFXI characters (character records with
    /// `w[2] == 1`), in table order.
    pub content_ids: Vec<u32>,
    /// Native POL credential ID (slot 283). Account data, never logged.
    pub account_id: u64,
}

impl LoaderSession {
    /// The contract's launcher-to-loader `session` object as one line of
    /// compact JSON, without the trailing `\n`. Binary fields are standard
    /// base64 with padding; `pol_records` is `null` when absent. The returned
    /// buffer is zeroed on drop.
    pub fn to_contract_json(&self) -> Zeroizing<String> {
        let records = self.pol_records.as_ref().map_or(0, Vec::len);
        let mut out = Zeroizing::new(String::with_capacity(
            256 + records * 4 / 3
                + self.content_ids.len() * 11
                + self.install_dir.as_ref().map_or(0, |d| d.len() * 6),
        ));
        out.push_str("{\"type\":\"session\",\"v\":");
        out.push_str(&CONTRACT_VERSION.to_string());
        out.push_str(",\"region\":\"");
        out.push_str(self.region.as_str());
        out.push_str("\",\"lang\":\"");
        out.push_str(self.lang.as_str());
        out.push('"');
        if let Some(dir) = &self.install_dir {
            out.push_str(",\"install_dir\":");
            push_json_string(&mut out, dir);
        }
        out.push_str(",\"v_key\":\"");
        push_base64(&mut out, &self.v_key);
        out.push_str("\",\"auth_code\":\"");
        push_base64(&mut out, &self.auth_code);
        out.push_str("\",\"pol_records\":");
        match &self.pol_records {
            Some(bytes) => {
                out.push('"');
                push_base64(&mut out, bytes);
                out.push('"');
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"content_ids\":[");
        for (i, id) in self.content_ids.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&id.to_string());
        }
        out.push_str("],\"account_id\":\"");
        out.push_str(&format!("{:016x}", self.account_id));
        out.push_str("\"}");
        out
    }
}

impl Zeroize for LoaderSession {
    fn zeroize(&mut self) {
        self.v_key.zeroize();
        self.auth_code.zeroize();
        self.pol_records.zeroize();
        self.content_ids.zeroize();
        self.account_id.zeroize();
    }
}
impl Drop for LoaderSession {
    fn drop(&mut self) {
        self.zeroize();
    }
}
impl std::fmt::Debug for LoaderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoaderSession")
            .field("region", &self.region)
            .field("lang", &self.lang)
            .field("install_dir", &self.install_dir)
            .field("v_key", &"[redacted]")
            .field("auth_code", &"[redacted]")
            .field(
                "pol_records",
                &self.pol_records.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "content_ids",
                &format_args!("[{} redacted]", self.content_ids.len()),
            )
            .finish()
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, appended in place (no intermediate copy).
fn push_base64(out: &mut String, bytes: &[u8]) {
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The six cdecl arguments FFXiMain passes to slot 195 (alias 376), which is
/// polcore `1001db60` -> `1001d7e0`, the ChangeMyStatus community (4,5) start.
///
/// Wire form (the contract's `presence` payload): 24 bytes, six `i32`
/// little-endian in argument order. `-2` keeps the retained value; see
/// [`Self::parse`]. Known FFXiMain calls: lobby `(-2,-1,0,-2,-1,1)`; world
/// entry `(handle, index, 0, -2, -1, 1)` from `100ee388..100ee3a0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresenceArgs {
    /// a0: handle 0..63, -1 absent, -2 keep.
    pub handle: i32,
    /// a1: character-table index 0..63, -1 absent, -2 keep.
    pub primary: i32,
    /// a2: context 0/1, -2 keep.
    pub context: i32,
    /// a3: selection type 0..4, -2 keep.
    pub selection_type: i32,
    /// a4: flag; negative keeps.
    pub flag: i32,
    /// a5: content class 0..1023; anything else keeps.
    pub content_class: i32,
}

impl PresenceArgs {
    /// FFXiMain's lobby bring-up call (step 11 of `0x100ed7f0`).
    pub const LOBBY: Self = Self {
        handle: -2,
        primary: -1,
        context: 0,
        selection_type: -2,
        flag: -1,
        content_class: 1,
    };

    pub fn parse(payload: &[u8]) -> Result<Self, PresenceError> {
        if payload.len() != PRESENCE_ARGS_LEN {
            return Err(PresenceError::Payload);
        }
        let a = |i: usize| i32::from_le_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap());
        Ok(Self {
            handle: a(0),
            primary: a(1),
            context: a(2),
            selection_type: a(3),
            flag: a(4),
            content_class: a(5),
        })
    }

    pub fn to_bytes(&self) -> [u8; PRESENCE_ARGS_LEN] {
        PresenceState {
            handle: self.handle,
            primary: self.primary,
            context: self.context,
            selection_type: self.selection_type,
            flag: self.flag,
            content_class: self.content_class,
        }
        .to_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceError {
    /// The payload is not 24 bytes.
    Payload,
    /// polcore would refuse these arguments; the native negative code
    /// (-7190, -7171, -7185, or -263 for type 3 without a handle).
    Argument(i32),
    /// The primary chat session has ended; polcore reports -515 here.
    ChatSession(KeepaliveError),
    Io(io::ErrorKind),
    /// Malformed or unverifiable profile reply.
    Protocol(&'static str),
    /// The profile server refused the request with this status.
    Service(u8),
}
impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Payload => f.write_str("presence payload must be 24 bytes"),
            Self::Argument(code) => write!(f, "presence arguments refused ({code})"),
            Self::ChatSession(e) => write!(f, "PlayOnline chat session: {e}"),
            Self::Io(kind) => write!(f, "profile connection: {kind:?}"),
            Self::Protocol(what) => write!(f, "profile reply: {what}"),
            Self::Service(status) => write!(f, "profile service status {status}"),
        }
    }
}
impl std::error::Error for PresenceError {}
impl From<ExchangeError> for PresenceError {
    fn from(error: ExchangeError) -> Self {
        match error {
            ExchangeError::Io(e) => Self::Io(e.kind()),
            ExchangeError::Protocol(crate::Error::Checksum) => Self::Protocol("checksum"),
            ExchangeError::Protocol(_) => Self::Protocol("invalid frame"),
            ExchangeError::Catalog(_) => Self::Protocol("unexpected catalog"),
            ExchangeError::Service { status, .. } => Self::Service(status),
        }
    }
}

/// Result of a forwarded presence update.
#[derive(Debug)]
pub enum PresenceOutcome {
    /// Nothing changed and the community state is valid: polcore completes
    /// without a network request (`1001dbee`), and so does this.
    Unchanged,
    /// The server accepted the (4,5). Retail polcore overwrites its V and
    /// authCode with these; FFXiMain does not re-read them after the lobby.
    Sent {
        session: crate::session::LobbySession,
    },
}

/// A validated (4,5) request and the retained state to install on success.
pub(crate) struct PresencePlan {
    pub body: [u8; 36],
    pub next: PresenceState,
    pub changed: bool,
}

/// polcore `1001d7e0`: validate the arguments against the retained state,
/// derive the keep/changed flags and build the 36-byte community body.
/// `valid` is the community-valid global `100ac548`.
pub(crate) fn plan_presence(
    args: &PresenceArgs,
    current: &PresenceState,
    valid: bool,
    map: &SelectionMap,
) -> Result<PresencePlan, PresenceError> {
    const BAD_HANDLE: i32 = -7190; // 0xffffe3ea
    const BAD_PRIMARY: i32 = -7171; // 0xffffe3fd
    const BAD_VALUE: i32 = -7185; // 0xffffe3ef
    const TYPE3_WITHOUT_HANDLE: i32 = -263; // 0xfffffef9
    if args.handle >= 64 {
        return Err(PresenceError::Argument(BAD_HANDLE));
    }
    if args.primary >= 64 {
        return Err(PresenceError::Argument(BAD_PRIMARY));
    }
    let mut next = *current;
    let mut keep = true;
    let mut changed = !valid;
    let mut explicit = |value: i32, retained: &mut i32| {
        keep = false;
        if value != *retained {
            changed = true;
        }
        *retained = value;
    };
    for (value, retained) in [
        (args.primary, &mut next.primary),
        (args.handle, &mut next.handle),
    ] {
        match value {
            -2 => (),
            v if v < -1 => return Err(PresenceError::Argument(BAD_HANDLE)),
            v => explicit(v, retained),
        }
    }
    match args.context {
        -2 => (),
        0 | 1 => explicit(args.context, &mut next.context),
        _ => return Err(PresenceError::Argument(BAD_VALUE)),
    }
    match args.selection_type {
        -2 => (),
        0..=4 => explicit(args.selection_type, &mut next.selection_type),
        _ => return Err(PresenceError::Argument(BAD_VALUE)),
    }
    if (0..0x400).contains(&args.content_class) {
        explicit(args.content_class, &mut next.content_class);
    }
    if args.flag >= 0 {
        explicit(args.flag, &mut next.flag);
    }
    if !valid {
        keep = false;
    }
    if next.selection_type == 3 && next.handle < 0 {
        // The second type-3 gate tests 100aaae0[slot]==1, an array a fresh
        // sign-in leaves zero; it never rejects here.
        return Err(PresenceError::Argument(TYPE3_WITHOUT_HANDLE));
    }
    let world_id = if next.handle >= 0 {
        next.handle as u8
    } else {
        0
    };
    // 1001da54..1001da8d: first valid map byte of handle `world_id` whose
    // index equals the primary; an absent (-1) primary never matches.
    let found = map.bytes()[usize::from(world_id)]
        .iter()
        .position(|e| e & 1 != 0 && i32::from((e >> 1) & 63) == next.primary);
    let body = CommunityRequest {
        world_id,
        service_available: found.is_some(),
        service_index: found.unwrap_or(0) as u8,
        context_default: next.context == 0,
        region: next.content_class as u16,
        world_index_plus1: if next.handle >= 0 {
            (next.selection_type + 1) as u8
        } else {
            0
        },
        flag_d8: next.flag != 0,
        cached: keep,
    }
    .payload();
    Ok(PresencePlan {
        body,
        next,
        changed,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn decode_base64(text: &str) -> Vec<u8> {
        let value = |c: u8| BASE64.iter().position(|b| *b == c).unwrap() as u32;
        let mut out = Vec::new();
        for chunk in text.as_bytes().chunks(4) {
            let pad = chunk.iter().filter(|c| **c == b'=').count();
            let n = chunk
                .iter()
                .map(|c| if *c == b'=' { 0 } else { value(*c) })
                .fold(0, |acc, v| acc << 6 | v);
            out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
        }
        out
    }

    /// Extract `"key":"value"` or `"key":literal` from the flat JSON object.
    pub(crate) fn field<'a>(json: &'a str, key: &str) -> &'a str {
        let start = json.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
        let rest = &json[start..];
        if let Some(stripped) = rest.strip_prefix('"') {
            &stripped[..stripped.find('"').unwrap()]
        } else if rest.starts_with('[') {
            &rest[..=rest.find(']').unwrap()]
        } else {
            &rest[..rest.find([',', '}']).unwrap()]
        }
    }

    fn session(records: Option<Vec<u8>>) -> LoaderSession {
        let mut auth_code = [0; 52];
        for (i, b) in auth_code.iter_mut().enumerate() {
            *b = i as u8;
        }
        LoaderSession {
            region: Region::Us,
            lang: Region::Us,
            install_dir: Some("C:\\Games\\Square\"Enix".into()),
            v_key: [0xfb; 16],
            auth_code,
            pol_records: records,
            content_ids: vec![1234567, 7654321],
            account_id: 0x9182_7364_5546_3728,
        }
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            (&[0xfb, 0xff, 0xbf], "+/+/"),
        ] {
            let mut out = String::new();
            push_base64(&mut out, input);
            assert_eq!(out, expected);
            assert_eq!(decode_base64(&out), input);
        }
    }

    #[test]
    fn contract_json_has_the_v1_session_shape() {
        let records = vec![7u8; 0x2420];
        let s = session(Some(records.clone()));
        let json = s.to_contract_json();
        assert!(json.starts_with("{\"type\":\"session\",\"v\":1,"));
        assert!(json.ends_with('}') && !json.contains('\n'));
        assert_eq!(field(&json, "region"), "US");
        assert_eq!(field(&json, "lang"), "US");
        assert!(json.contains(r#""install_dir":"C:\\Games\\Square\"Enix""#));
        assert_eq!(decode_base64(field(&json, "v_key")), [0xfb; 16]);
        assert_eq!(decode_base64(field(&json, "auth_code")), s.auth_code);
        assert_eq!(decode_base64(field(&json, "pol_records")), records);
        assert_eq!(field(&json, "content_ids"), "[1234567,7654321]");

        let mut bare = session(None);
        bare.install_dir = None;
        bare.content_ids.clear();
        let json = bare.to_contract_json();
        assert!(json.contains("\"pol_records\":null,"));
        assert!(json.ends_with("\"content_ids\":[],\"account_id\":\"9182736455463728\"}"));
        assert!(!json.contains("install_dir"));
    }

    #[test]
    fn debug_never_prints_secrets() {
        let s = session(Some(vec![0xab; 8]));
        let text = format!("{s:?}");
        assert!(text.contains("[redacted]") && text.contains("Us"));
        for secret in ["251", "0xfb", "fb", "1234567", "171", "ab"] {
            assert!(!text.contains(secret), "{secret} leaked in {text}");
        }
    }

    #[test]
    fn zeroize_clears_every_secret_field() {
        let mut s = session(Some(vec![9; 32]));
        s.zeroize();
        assert_eq!(s.v_key, [0; 16]);
        assert_eq!(s.auth_code, [0; 52]);
        assert!(s.pol_records.as_ref().map_or(true, Vec::is_empty));
        assert!(s.content_ids.is_empty());
        // Drop runs the same zeroize; inline key bytes are observable through
        // ManuallyDrop without freeing them.
        let mut held = std::mem::ManuallyDrop::new(session(None));
        unsafe { std::ptr::drop_in_place(&mut *held as *mut LoaderSession) };
        assert_eq!(held.v_key, [0; 16]);
        assert_eq!(held.auth_code, [0; 52]);
    }

    #[test]
    fn presence_args_round_trip_in_cdecl_order() {
        let args = PresenceArgs {
            handle: 3,
            primary: 2,
            context: 0,
            selection_type: -2,
            flag: -1,
            content_class: 1,
        };
        let bytes = args.to_bytes();
        assert_eq!(&bytes[0..4], &3i32.to_le_bytes());
        assert_eq!(&bytes[12..16], &(-2i32).to_le_bytes());
        assert_eq!(&bytes[20..24], &1i32.to_le_bytes());
        assert_eq!(PresenceArgs::parse(&bytes), Ok(args));
        assert_eq!(
            PresenceArgs::parse(&bytes[..23]),
            Err(PresenceError::Payload)
        );
        assert_eq!(PresenceArgs::parse(&[0; 25]), Err(PresenceError::Payload));
    }

    fn map_with(handle: usize, subslot: usize, primary: u8) -> SelectionMap {
        let mut bytes = [[0; 8]; 64];
        bytes[handle][subslot] = (primary << 1) | 1;
        SelectionMap::from_bytes(&bytes)
    }

    const AFTER_SIGNIN: PresenceState = PresenceState {
        handle: 0,
        primary: -1,
        context: 0,
        selection_type: 0,
        flag: 1,
        content_class: 1,
    };

    #[test]
    fn world_entry_change_my_status_body_matches_1001d7e0() {
        let map = map_with(0, 3, 2);
        let world = PresenceArgs {
            handle: 0,
            primary: 2,
            context: 0,
            selection_type: -2,
            flag: -1,
            content_class: 1,
        };
        let plan = plan_presence(&world, &AFTER_SIGNIN, true, &map).unwrap();
        assert!(plan.changed);
        assert_eq!(plan.next.primary, 2);
        let mut expected = [0u8; 36];
        // handle, found, subslot, !context, class LE, type+1, flag, keep=0, 1
        expected[16..26].copy_from_slice(&[0, 1, 3, 1, 1, 0, 1, 1, 0, 1]);
        assert_eq!(plan.body, expected);
        // Repeating it with the state installed is a no-op request.
        let again = plan_presence(&world, &plan.next, true, &map).unwrap();
        assert!(!again.changed);
    }

    #[test]
    fn keep_flag_survives_only_all_retained_arguments_on_valid_state() {
        let all_keep = PresenceArgs {
            handle: -2,
            primary: -2,
            context: -2,
            selection_type: -2,
            flag: -1,
            content_class: -1,
        };
        let map = SelectionMap::default();
        let plan = plan_presence(&all_keep, &AFTER_SIGNIN, true, &map).unwrap();
        assert!(!plan.changed);
        assert_eq!(plan.body[0x18], 1);
        assert_eq!(plan.next, AFTER_SIGNIN);
        let invalid = plan_presence(&all_keep, &AFTER_SIGNIN, false, &map).unwrap();
        assert!(invalid.changed);
        assert_eq!(invalid.body[0x18], 0);
        // An explicit but equal value clears keep without marking a change.
        let mut same = all_keep;
        same.content_class = 1;
        let plan = plan_presence(&same, &AFTER_SIGNIN, true, &map).unwrap();
        assert!(!plan.changed);
        assert_eq!(plan.body[0x18], 0);
    }

    #[test]
    fn invalid_arguments_use_native_error_codes() {
        let map = SelectionMap::default();
        let base = PresenceArgs::LOBBY;
        for (args, code) in [
            (PresenceArgs { handle: 64, ..base }, -7190),
            (PresenceArgs { handle: -3, ..base }, -7190),
            (
                PresenceArgs {
                    primary: 64,
                    ..base
                },
                -7171,
            ),
            (
                PresenceArgs {
                    primary: -3,
                    ..base
                },
                -7190,
            ),
            (PresenceArgs { context: 2, ..base }, -7185),
            (
                PresenceArgs {
                    selection_type: 5,
                    ..base
                },
                -7185,
            ),
            (
                PresenceArgs {
                    selection_type: -1,
                    ..base
                },
                -7185,
            ),
            (
                PresenceArgs {
                    handle: -1,
                    selection_type: 3,
                    ..base
                },
                -263,
            ),
        ] {
            assert_eq!(
                plan_presence(&args, &AFTER_SIGNIN, true, &map).err(),
                Some(PresenceError::Argument(code))
            );
        }
        // Out-of-range class and negative flag keep instead of failing.
        let lenient = PresenceArgs {
            content_class: 0x400,
            flag: -7,
            ..base
        };
        let plan = plan_presence(&lenient, &AFTER_SIGNIN, true, &map).unwrap();
        assert_eq!(plan.next.content_class, 1);
        assert_eq!(plan.next.flag, 1);
    }
}
