//! Decrypted catalog bodies from installed polcore build73b1864b.
//!
//! ContentIDs(2,3):100237f0, prefix count u32, records168 bytes, validator
//! 1001f090 and field consumer1001eeb0. Service catalog(0,9):1001e050,
//! prefix count u8, records136 bytes, fields1001e24c..1001e321.
//! Selection map(1,3):10022260, prefix count u8, records104 bytes;
//! map mutation10022519..1002255f and selection consumer1001da5a.
//! App task labels identify (2,3) as Get Friend List, (0,9) as Get Handle
//! List and (1,3) as Get Chr List. The earlier ContentIDs/Service API names
//! below are provisional; they do not identify FFXI entitlements or worlds.
//! All three native loops jump from exhausted count directly to completion state8,
//! skipping state7 trailer reader1001fad0. These readers likewise consume only
//! prefix+records; they neither invent nor verify a trailing checksum.
//!
//! Input must already be decrypted while preserving profile cipher continuity.
//! No TCP, key negotiation, credential access, or transport ownership here.

use crate::profile::Transaction;
use std::io::{self, Read};
use zeroize::Zeroizing;

pub const CONTENT_IDS: Transaction = Transaction(2, 3);
pub const SERVICES: Transaction = Transaction(0, 9);
pub const SELECTION_MAP: Transaction = Transaction(1, 3);
pub const PREFIX_LEN: usize = 8;
pub const CONTENT_RECORD_LEN: usize = 168;
pub const SERVICE_RECORD_LEN: usize = 136;
pub const SELECTION_MAP_RECORD_LEN: usize = 104;

#[derive(Debug)]
pub enum CatalogError {
    Io(io::Error),
    InvalidRecord,
    InvalidMapLink,
    CountLimit { count: u32, maximum: usize },
    LengthOverflow,
    Allocation,
}
impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "catalog input: {e}"),
            Self::InvalidRecord => f.write_str("invalid catalog record"),
            Self::InvalidMapLink => f.write_str("invalid selection map link"),
            Self::CountLimit { .. } => f.write_str("catalog count exceeds caller limit"),
            Self::LengthOverflow => f.write_str("catalog byte length overflows"),
            Self::Allocation => f.write_str("catalog allocation failed"),
        }
    }
}
impl std::error::Error for CatalogError {}
impl From<io::Error> for CatalogError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Sensitive record bytes are zeroed on drop and have no Debug implementation.
/// Unknown fields remain available through explicit raw access.
pub struct ContentRecord {
    raw: Zeroizing<[u8; CONTENT_RECORD_LEN]>,
}
impl ContentRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self, CatalogError> {
        let raw: [u8; CONTENT_RECORD_LEN] =
            bytes.try_into().map_err(|_| CatalogError::InvalidRecord)?;
        let record = Self {
            raw: Zeroizing::new(raw),
        };
        //1001f090: flag bit4 selects the100-entry table rather than200 entries.
        let limit = if record.alternate_table() { 100 } else { 200 };
        if record.index() >= limit {
            return Err(CatalogError::InvalidRecord);
        }
        Ok(record)
    }
    pub fn raw(&self) -> &[u8; CONTENT_RECORD_LEN] {
        &self.raw
    }
    pub fn flags(&self) -> u64 {
        le64(&self.raw[0..8])
    }
    pub fn alternate_table(&self) -> bool {
        self.flags() & 0x10 != 0
    }
    pub fn index(&self) -> u8 {
        self.raw[8]
    }
    /// Native copies byte9 to its internal record flags; meaning unproven.
    pub fn field_09(&self) -> u8 {
        self.raw[9]
    }
    /// Raw wire identifier.10019d40 XORs it with a process-local mask for
    /// internal storage; the mask is not part of the network identifier.
    pub fn identifier(&self) -> u64 {
        le64(&self.raw[0x10..0x18])
    }
    pub fn name_bytes(&self) -> &[u8] {
        nul_bytes(&self.raw[0x18..0x27])
    }
    pub fn kind(&self) -> u8 {
        ((self.flags() >> 5).wrapping_sub(1) & 3) as u8
    }
    pub fn active(&self) -> bool {
        self.flags() & 0x60 != 0
    }
    pub fn flag_62(&self) -> bool {
        self.flags() & (1u64 << 62) != 0
    }
    pub fn field_57_60(&self) -> u8 {
        ((self.flags() >> 57) & 15) as u8
    }
    pub fn field_13_56(&self) -> u64 {
        (self.flags() >> 13) & ((1u64 << 44) - 1)
    }
    pub fn field_07_12(&self) -> u8 {
        ((self.flags() >> 7) & 63) as u8
    }
    pub fn subrecords(&self) -> impl ExactSizeIterator<Item = ContentSubrecord<'_>> {
        self.raw[0x28..]
            .chunks_exact(16)
            .map(|raw| ContentSubrecord { raw })
    }
}

/// Eight native16-byte subrecords; field meanings remain neutral.
pub struct ContentSubrecord<'a> {
    raw: &'a [u8],
}
impl ContentSubrecord<'_> {
    pub fn active(&self) -> bool {
        self.raw[0] & 1 != 0
    }
    pub fn field_02(&self) -> u16 {
        le16(&self.raw[2..4])
    }
    pub fn field_04(&self) -> u32 {
        le32(&self.raw[4..8])
    }
    pub fn field_08(&self) -> u32 {
        le32(&self.raw[8..12])
    }
    pub fn field_0c(&self) -> u32 {
        le32(&self.raw[12..16])
    }
}

/// Service/handle catalog record; this is not established as a FFXI world list.
pub struct ServiceRecord {
    raw: Zeroizing<[u8; SERVICE_RECORD_LEN]>,
}
impl ServiceRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self, CatalogError> {
        let raw = bytes.try_into().map_err(|_| CatalogError::InvalidRecord)?;
        Ok(Self {
            raw: Zeroizing::new(raw),
        })
    }
    pub fn raw(&self) -> &[u8; SERVICE_RECORD_LEN] {
        &self.raw
    }
    pub fn slot(&self) -> u8 {
        self.raw[0]
    }
    /// Native consumes but does not store records with slot>=64.
    pub fn usable_slot(&self) -> bool {
        self.slot() < 64
    }
    pub fn order(&self) -> u8 {
        self.raw[1] & 63
    }
    pub fn kind(&self) -> u8 {
        self.raw[2] & 3
    }
    pub fn field_04(&self) -> u32 {
        le32(&self.raw[4..8])
    }
    pub fn flags(&self) -> u64 {
        le64(&self.raw[8..16]) & ((1u64 << 44) - 1)
    }
    pub fn name_bytes(&self) -> &[u8] {
        nul_bytes(&self.raw[16..31])
    }
    ///1004dcb0 copies at most50 LEu16 units and stops at NUL. No lossy Unicode
    /// conversion: callers can choose how to display invalid UTF-16 themselves.
    pub fn description_units(&self) -> impl Iterator<Item = u16> + '_ {
        self.raw[32..132]
            .chunks_exact(2)
            .map(le16)
            .take_while(|v| *v != 0)
    }
}

pub struct ContentCatalog {
    pub prefix: Zeroizing<[u8; PREFIX_LEN]>,
    pub records: Vec<ContentRecord>,
}
pub struct ServiceCatalog {
    pub prefix: Zeroizing<[u8; PREFIX_LEN]>,
    pub records: Vec<ServiceRecord>,
    pub ignored_records: usize,
}

/// Category1/opcode3 record from10022260. Unknown fields retain their exact
/// bytes with zeroization on drop; no character/world meaning is inferred.
pub struct SelectionMapRecord {
    raw: Zeroizing<[u8; SELECTION_MAP_RECORD_LEN]>,
}
impl SelectionMapRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self, CatalogError> {
        Ok(Self {
            raw: Zeroizing::new(bytes.try_into().map_err(|_| CatalogError::InvalidRecord)?),
        })
    }
    pub fn raw(&self) -> &[u8; SELECTION_MAP_RECORD_LEN] {
        &self.raw
    }
    pub fn primary(&self) -> u8 {
        self.raw[0]
    }
    pub fn usable_primary(&self) -> bool {
        self.primary() < 64
    }
    pub fn field_01(&self) -> u8 {
        self.raw[1]
    }
    pub fn link_enabled(&self) -> bool {
        self.raw[4] != 0
    }
    pub fn service(&self) -> u8 {
        self.raw[5]
    }
    pub fn subslot(&self) -> u8 {
        self.raw[6]
    }
    pub fn field_08(&self) -> u16 {
        le16(&self.raw[8..10])
    }
    pub fn field_0a(&self) -> u8 {
        self.raw[10]
    }
    pub fn field_0b(&self) -> u8 {
        self.raw[11]
    }
    pub fn field_0c(&self) -> u32 {
        le32(&self.raw[12..16])
    }
    pub fn field_10(&self) -> u32 {
        le32(&self.raw[16..20])
    }
    pub fn field_14(&self) -> u32 {
        le32(&self.raw[20..24])
    }
    pub fn text_18_bytes(&self) -> &[u8] {
        nul_bytes(&self.raw[24..39])
    }
    pub fn text_28_bytes(&self) -> &[u8] {
        nul_bytes(&self.raw[40..103])
    }
    fn applied_link(&self) -> bool {
        self.usable_primary() && self.link_enabled() && self.service() < 64
    }
    fn validate_link(&self) -> Result<(), CatalogError> {
        if self.applied_link() && self.subslot() >= 8 {
            return Err(CatalogError::InvalidMapLink);
        }
        Ok(())
    }
}

pub struct SelectionMapCatalog {
    pub prefix: Zeroizing<[u8; PREFIX_LEN]>,
    pub records: Vec<SelectionMapRecord>,
    pub ignored_records: usize,
}
impl SelectionMapCatalog {
    fn validate_links(&self) -> Result<(), CatalogError> {
        for record in &self.records {
            record.validate_link()?;
        }
        Ok(())
    }
}

/// The64x8 native selection bytes. Bit meanings below are established by the
/// map producer/consumer; higher-level selection policy remains caller-owned.
pub struct SelectionMap {
    bytes: Zeroizing<[[u8; 8]; 64]>,
}
impl Default for SelectionMap {
    fn default() -> Self {
        Self::from_bytes(&[[0; 8]; 64])
    }
}
impl SelectionMap {
    pub fn from_bytes(bytes: &[[u8; 8]; 64]) -> Self {
        Self {
            bytes: Zeroizing::new(*bytes),
        }
    }
    pub fn bytes(&self) -> &[[u8; 8]; 64] {
        &self.bytes
    }
    pub fn entry(&self, service: usize, subslot: usize) -> Option<SelectionMapEntry> {
        self.bytes
            .get(service)?
            .get(subslot)
            .copied()
            .map(SelectionMapEntry)
    }
    /// Validate every applied link before clearing or updating any byte.
    /// Refresh preserves bits1..7 of unfilled entries and bit7 of filled ones.
    pub fn refresh(&mut self, catalog: &SelectionMapCatalog) -> Result<(), CatalogError> {
        catalog.validate_links()?;
        for service in self.bytes.iter_mut() {
            for entry in service {
                *entry &= 0xfe;
            }
        }
        for record in &catalog.records {
            if record.applied_link() {
                let entry =
                    &mut self.bytes[usize::from(record.service())][usize::from(record.subslot())];
                *entry = (*entry & 0x80) | ((record.primary() & 63) << 1) | 1;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct SelectionMapEntry(u8);
impl SelectionMapEntry {
    pub fn active(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn selector(self) -> u8 {
        (self.0 >> 1) & 63
    }
    pub fn high_bit(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// Count is prefix byte0; all other prefix bytes remain uninterpreted. Native
/// count exhaustion skips its checksum state, so trailers remain unread.
pub fn read_selection_map<R: Read + ?Sized>(
    input: &mut R,
) -> Result<SelectionMapCatalog, CatalogError> {
    let mut prefix = Zeroizing::new([0; PREFIX_LEN]);
    input.read_exact(&mut prefix[..])?;
    let n = checked_count(
        u32::from(prefix[0]),
        SELECTION_MAP_RECORD_LEN,
        u8::MAX as usize,
    )?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(n)
        .map_err(|_| CatalogError::Allocation)?;
    let mut bytes = Zeroizing::new([0; SELECTION_MAP_RECORD_LEN]);
    let mut ignored_records = 0;
    for _ in 0..n {
        input.read_exact(&mut bytes[..])?;
        let record = SelectionMapRecord::parse(&bytes[..])?;
        if record.usable_primary() {
            records.push(record);
        } else {
            ignored_records += 1;
        }
    }
    let catalog = SelectionMapCatalog {
        prefix,
        records,
        ignored_records,
    };
    catalog.validate_links()?;
    Ok(catalog)
}

/// Read the8-byte prefix and all content records. `maximum_records` is a
/// caller-provided memory budget, not an inferred server/protocol restriction.
pub fn read_content_ids<R: Read + ?Sized>(
    input: &mut R,
    maximum_records: usize,
) -> Result<ContentCatalog, CatalogError> {
    let mut prefix = Zeroizing::new([0; PREFIX_LEN]);
    input.read_exact(&mut prefix[..])?;
    let count = le32(&prefix[..4]);
    let records = read_content_records(input, count, maximum_records)?;
    Ok(ContentCatalog { prefix, records })
}

/// For a caller that has already consumed/decrypted the prefix. Preserves wire
/// order and duplicate indices; native table overwrite policy belongs to the
/// eventual orchestration layer rather than silently dropping duplicate input.
pub fn read_content_records<R: Read + ?Sized>(
    input: &mut R,
    count: u32,
    maximum_records: usize,
) -> Result<Vec<ContentRecord>, CatalogError> {
    let count_usize = checked_count(count, CONTENT_RECORD_LEN, maximum_records)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(count_usize)
        .map_err(|_| CatalogError::Allocation)?;
    let mut bytes = Zeroizing::new([0; CONTENT_RECORD_LEN]);
    for _ in 0..count_usize {
        input.read_exact(&mut bytes[..])?;
        records.push(ContentRecord::parse(&bytes[..])?);
    }
    Ok(records)
}

pub fn read_services<R: Read + ?Sized>(input: &mut R) -> Result<ServiceCatalog, CatalogError> {
    let mut prefix = Zeroizing::new([0; PREFIX_LEN]);
    input.read_exact(&mut prefix[..])?;
    let (records, ignored_records) = read_service_records(input, prefix[0])?;
    Ok(ServiceCatalog {
        prefix,
        records,
        ignored_records,
    })
}

/// Count is exactly one wire byte, bounding allocation to255 records. Invalid
/// slots are consumed and omitted, matching1001e253..1001e256.
pub fn read_service_records<R: Read + ?Sized>(
    input: &mut R,
    count: u8,
) -> Result<(Vec<ServiceRecord>, usize), CatalogError> {
    let n = checked_count(u32::from(count), SERVICE_RECORD_LEN, u8::MAX as usize)?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(n)
        .map_err(|_| CatalogError::Allocation)?;
    let mut bytes = Zeroizing::new([0; SERVICE_RECORD_LEN]);
    let mut ignored = 0;
    for _ in 0..n {
        input.read_exact(&mut bytes[..])?;
        let record = ServiceRecord::parse(&bytes[..])?;
        if record.usable_slot() {
            records.push(record);
        } else {
            ignored += 1;
        }
    }
    Ok((records, ignored))
}

fn checked_count(count: u32, record_size: usize, maximum: usize) -> Result<usize, CatalogError> {
    let n = usize::try_from(count).map_err(|_| CatalogError::LengthOverflow)?;
    if n > maximum {
        return Err(CatalogError::CountLimit { count, maximum });
    }
    n.checked_mul(record_size)
        .and_then(|b| b.checked_add(PREFIX_LEN))
        .filter(|b| *b <= isize::MAX as usize)
        .ok_or(CatalogError::LengthOverflow)?;
    Ok(n)
}
fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().unwrap())
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}
fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}
fn nul_bytes(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    struct Fragments(Cursor<Vec<u8>>);
    impl Read for Fragments {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = out.len().min(3);
            self.0.read(&mut out[..n])
        }
    }
    fn content(index: u8, alternate: bool) -> [u8; CONTENT_RECORD_LEN] {
        let mut b = [0; CONTENT_RECORD_LEN];
        let flags = (2u64 << 5)
            | (7u64 << 57)
            | (1u64 << 62)
            | (9u64 << 13)
            | (5u64 << 7)
            | if alternate { 16 } else { 0 };
        b[..8].copy_from_slice(&flags.to_le_bytes());
        b[8] = index;
        b[9] = 0xab;
        b[16..24].copy_from_slice(&0x0102030405060708u64.to_le_bytes());
        b[24..28].copy_from_slice(b"Name");
        for (i, s) in b[40..].chunks_exact_mut(16).enumerate() {
            s[0] = 3;
            s[2..4].copy_from_slice(&(i as u16).to_le_bytes());
            s[4..8].copy_from_slice(&(100 + i as u32).to_le_bytes());
            s[8..12].copy_from_slice(&(200 + i as u32).to_le_bytes());
            s[12..16].copy_from_slice(&(300 + i as u32).to_le_bytes());
        }
        b
    }
    #[test]
    fn content_fields_and_fragmented_count_stop_before_trailer() {
        let mut bytes = vec![2, 0, 0, 0, 0xaa, 0xbb, 0xcc, 0xdd];
        bytes.extend(content(199, false));
        bytes.extend(content(99, true));
        bytes.extend([0xde, 0xad, 0xbe, 0xef]);
        let mut stream = Fragments(Cursor::new(bytes));
        let catalog = read_content_ids(&mut stream, 2).unwrap();
        assert_eq!(catalog.records.len(), 2);
        assert_eq!(&catalog.prefix[4..], &[0xaa, 0xbb, 0xcc, 0xdd]);
        let r = &catalog.records[0];
        assert_eq!(r.index(), 199);
        assert_eq!(r.identifier(), 0x0102030405060708);
        assert_eq!(r.name_bytes(), b"Name");
        assert_eq!((r.kind(), r.field_09(), r.field_57_60()), (1, 0xab, 7));
        assert_eq!((r.field_13_56(), r.field_07_12()), (9, 5));
        assert!(r.active() && r.flag_62());
        assert!(catalog.records[1].alternate_table());
        for (i, s) in r.subrecords().enumerate() {
            assert!(s.active());
            assert_eq!(s.field_02(), i as u16);
            assert_eq!(
                (s.field_04(), s.field_08(), s.field_0c()),
                (100 + i as u32, 200 + i as u32, 300 + i as u32)
            );
        }
        let mut trailer = [0; 4];
        stream.read_exact(&mut trailer).unwrap();
        assert_eq!(trailer, [0xde, 0xad, 0xbe, 0xef]);
    }
    #[test]
    fn service_count_is_one_byte_invalid_slots_are_skipped_and_text_is_bounded() {
        let mut a = [0xff; SERVICE_RECORD_LEN];
        a[0] = 63;
        a[1] = 0xc5;
        a[2] = 0xfe;
        a[4..8].copy_from_slice(&0x12345678u32.to_le_bytes());
        a[16..31].copy_from_slice(b"FifteenByteName");
        a[32..34].copy_from_slice(&0x0041u16.to_le_bytes());
        a[34..36].copy_from_slice(&0xd800u16.to_le_bytes());
        a[36..38].copy_from_slice(&0u16.to_le_bytes());
        let mut b = a;
        b[0] = 64;
        let mut bytes = vec![2, 255, 255, 255, 255, 255, 255, 255];
        bytes.extend(a);
        bytes.extend(b);
        bytes.push(0x72);
        let mut stream = Fragments(Cursor::new(bytes));
        let catalog = read_services(&mut stream).unwrap();
        assert_eq!((catalog.records.len(), catalog.ignored_records), (1, 1));
        let r = &catalog.records[0];
        assert_eq!((r.slot(), r.order(), r.kind()), (63, 5, 2));
        assert_eq!(r.field_04(), 0x12345678);
        assert_eq!(r.flags(), (1u64 << 44) - 1);
        assert_eq!(r.name_bytes(), b"FifteenByteName");
        assert_eq!(r.description_units().collect::<Vec<_>>(), [0x41, 0xd800]);
        let mut tail = [0];
        stream.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [0x72]);
    }
    #[test]
    fn limits_and_zero_counts_never_consume_records() {
        let mut stream = Cursor::new(vec![0xff; 8]);
        assert!(matches!(
            read_content_ids(&mut stream, 300),
            Err(CatalogError::CountLimit { .. })
        ));
        assert_eq!(stream.position(), 8);
        let mut empty = Cursor::new(vec![0; 9]);
        assert!(read_content_ids(&mut empty, 0).unwrap().records.is_empty());
        assert_eq!(empty.position(), 8);
        assert!(matches!(
            checked_count(u32::MAX, usize::MAX, usize::MAX),
            Err(CatalogError::LengthOverflow)
        ));
    }
    #[test]
    fn index_validation_and_truncation_match_native_boundaries() {
        assert!(ContentRecord::parse(&content(200, false)).is_err());
        assert!(ContentRecord::parse(&content(100, true)).is_err());
        assert!(ContentRecord::parse(&[0; 167]).is_err());
        let mut short = Cursor::new(vec![0; 167]);
        assert!(matches!(read_content_records(&mut short, 1, 1),
            Err(CatalogError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof));
        let mut short_service = Cursor::new(vec![0; 135]);
        assert!(matches!(read_service_records(&mut short_service, 1),
            Err(CatalogError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof));
    }

    fn selection(
        primary: u8,
        enabled: u8,
        service: u8,
        subslot: u8,
    ) -> [u8; SELECTION_MAP_RECORD_LEN] {
        let mut raw = [0; SELECTION_MAP_RECORD_LEN];
        raw[0] = primary;
        raw[4] = enabled;
        raw[5] = service;
        raw[6] = subslot;
        raw
    }

    #[test]
    fn fragmented_selection_map_fields_ignored_links_and_trailer() {
        let mut first = selection(63, 2, 63, 7);
        first[1] = 0xab;
        first[8..10].copy_from_slice(&0x1234u16.to_le_bytes());
        first[10..12].copy_from_slice(&[17, 19]);
        first[12..16].copy_from_slice(&0x12345678u32.to_le_bytes());
        first[16..20].copy_from_slice(&0x87654321u32.to_le_bytes());
        first[20..24].copy_from_slice(&0x11223344u32.to_le_bytes());
        first[24..40].fill(b'A');
        first[40..104].fill(b'B');
        let mut bytes = vec![5, 255, 255, 255, 1, 2, 3, 4];
        for record in [
            first,
            selection(64, 1, 2, 255),
            selection(7, 0, 1, 255),
            selection(5, 1, 64, 255),
            selection(9, 1, 63, 7),
        ] {
            bytes.extend(record);
        }
        bytes.extend([0xde, 0xad, 0xbe, 0xef]);
        let mut stream = Fragments(Cursor::new(bytes));
        let catalog = read_selection_map(&mut stream).unwrap();
        assert_eq!((catalog.records.len(), catalog.ignored_records), (4, 1));
        assert_eq!(&catalog.prefix[..], &[5, 255, 255, 255, 1, 2, 3, 4]);
        let record = &catalog.records[0];
        assert_eq!(record.raw(), &first);
        assert_eq!(
            (
                record.primary(),
                record.field_01(),
                record.service(),
                record.subslot()
            ),
            (63, 0xab, 63, 7)
        );
        assert_eq!(
            (record.field_08(), record.field_0a(), record.field_0b()),
            (0x1234, 17, 19)
        );
        assert_eq!(
            (record.field_0c(), record.field_10(), record.field_14()),
            (0x12345678, 0x87654321, 0x11223344)
        );
        assert_eq!(record.text_18_bytes(), &[b'A'; 15]);
        assert_eq!(record.text_28_bytes(), &[b'B'; 63]);
        let mut map = SelectionMap::from_bytes(&[[0xff; 8]; 64]);
        map.refresh(&catalog).unwrap();
        assert_eq!(map.bytes()[63][7], 0x93); // Last duplicate wins; bit7 survives.
        assert_eq!(map.bytes()[1][0], 0xfe); // Unfilled selector bits survive, validity clears.
        let entry = map.entry(63, 7).unwrap();
        assert!(entry.active() && entry.high_bit());
        assert_eq!(entry.selector(), 9);
        let stale = map.entry(1, 0).unwrap();
        assert!(!stale.active() && stale.high_bit());
        assert_eq!(stale.selector(), 63);
        assert!(map.entry(64, 0).is_none() && map.entry(0, 8).is_none());
        assert_eq!(
            stream.0.position(),
            (PREFIX_LEN + 5 * SELECTION_MAP_RECORD_LEN) as u64
        );
        let mut tail = [0; 4];
        stream.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn invalid_applied_subslot_rejects_refresh_atomically() {
        let catalog = SelectionMapCatalog {
            prefix: Zeroizing::new([2, 0, 0, 0, 0, 0, 0, 0]),
            records: vec![
                SelectionMapRecord::parse(&selection(3, 1, 2, 1)).unwrap(),
                SelectionMapRecord::parse(&selection(4, 1, 2, 8)).unwrap(),
            ],
            ignored_records: 0,
        };
        let original = [[0xa5; 8]; 64];
        let mut map = SelectionMap::from_bytes(&original);
        assert!(matches!(
            map.refresh(&catalog),
            Err(CatalogError::InvalidMapLink)
        ));
        assert_eq!(map.bytes(), &original);
        let mut body = vec![2, 0, 0, 0, 0, 0, 0, 0];
        for record in &catalog.records {
            body.extend(record.raw());
        }
        assert!(matches!(
            read_selection_map(&mut Cursor::new(body)),
            Err(CatalogError::InvalidMapLink)
        ));
    }

    #[test]
    fn zero_selection_count_clears_only_validity_and_truncation_is_rejected() {
        let mut empty = Cursor::new(vec![0, 255, 255, 255, 255, 255, 255, 255, 0x72]);
        let catalog = read_selection_map(&mut empty).unwrap();
        assert!(catalog.records.is_empty());
        assert_eq!(empty.position(), 8);
        let mut map = SelectionMap::from_bytes(&[[0x35; 8]; 64]);
        map.refresh(&catalog).unwrap();
        assert_eq!(map.bytes(), &[[0x34; 8]; 64]);
        let mut short = Cursor::new(vec![1; PREFIX_LEN + SELECTION_MAP_RECORD_LEN - 1]);
        assert!(
            matches!(read_selection_map(&mut short), Err(CatalogError::Io(e))
            if e.kind() == io::ErrorKind::UnexpectedEof)
        );
    }
}
