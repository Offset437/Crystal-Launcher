//! Get Friend List (2,3), converted to polcore's two retained 0xb0-byte tables.
//! Stores and getter normalization follow installed polcore 73b1864b:
//! 1001eeb0, 10023da0, 10023e10, 10023f90; live stores use 1001ecb0.
//! No network or guessed presence. Saved lists alone never imply offline.

use crate::catalog::ContentCatalog;
use zeroize::{Zeroize, Zeroizing};

pub const RECORD_LEN: usize = 0xb0;
pub const A_SLOTS: usize = 200;
pub const B_SLOTS: usize = 100;
pub const MAGIC: &[u8; 8] = b"XIPOLF\x01\0";
pub const BLOB_LEN: usize = 8 + (A_SLOTS + B_SLOTS) * RECORD_LEN;

/// Account data; deliberately no Debug implementation.
#[derive(Clone)]
pub struct Friends {
    a: Zeroizing<Vec<[u8; RECORD_LEN]>>,
    b: Zeroizing<Vec<[u8; RECORD_LEN]>>,
    presence_known: Zeroizing<Vec<bool>>,
    locations: Zeroizing<Vec<Option<[u8;24]>>>,
}

impl Friends {
    pub fn from_catalog(catalog: &ContentCatalog, id_mask: u64) -> Self {
        let mut out = Self {
            a: Zeroizing::new(vec![[0; RECORD_LEN]; A_SLOTS]),
            b: Zeroizing::new(vec![[0; RECORD_LEN]; B_SLOTS]),
            presence_known: Zeroizing::new(vec![false; A_SLOTS]),
            locations: Zeroizing::new(vec![None; A_SLOTS]),
        };
        for wire in &catalog.records {
            let table = if wire.alternate_table() { &mut out.b } else { &mut out.a };
            let Some(rec) = table.get_mut(usize::from(wire.index())) else { continue };
            let flags = wire.flags();
            rec[..8].copy_from_slice(&(wire.identifier() ^ id_mask).to_le_bytes());
            let q8 = u64::from(wire.kind())
                | (u64::from(wire.field_09()) << 2)
                | (u64::from(wire.flag_62()) << 28)
                | (u64::from(wire.field_57_60()) << 29);
            rec[8..16].copy_from_slice(&q8.to_le_bytes());
            let q98 = u64::from(wire.active())
                | ((flags >> 12) & 0x1fff_ffff_fffe)
                | (u64::from(wire.field_07_12()) << 45);
            rec[0x98..0xa0].copy_from_slice(&q98.to_le_bytes());
            let name = wire.name_bytes();
            rec[0xa0..0xa0 + name.len()].copy_from_slice(name);
            for (i, sub) in wire.subrecords().enumerate() {
                let at = 0x18 + i * 16;
                rec[at] = u8::from(sub.active());
                rec[at+2..at+4].copy_from_slice(&sub.field_02().to_le_bytes());
                rec[at+4..at+8].copy_from_slice(&sub.field_04().to_le_bytes());
                rec[at+8..at+12].copy_from_slice(&sub.field_08().to_le_bytes());
                rec[at+12..at+16].copy_from_slice(&sub.field_0c().to_le_bytes());
            }
        }
        out
    }

    /// Only a validated live update can establish online/offline. The saved
    /// catalog alone supplies neither presence nor a location.
    pub fn presence(&self, alternate: bool, index: usize) -> (Option<bool>, Option<&[u8]>) {
        if alternate || !self.presence_known.get(index).copied().unwrap_or(false) { return (None,None); }
        let online = le64(&self.a[index][8..16]) & (1<<11) != 0;
        let location = if online { self.locations[index].as_ref().map(|bytes| {
            &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())]
        }) } else { None };
        (Some(online), location)
    }

    pub(crate) fn apply_notice(&mut self, notice: &crate::friend_presence::Notice, mask: u64) {
        let index=usize::from(notice.index);
        let rec=&mut self.a[index];
        if rec[0x98]&1 == 0 || le64(&rec[..8]) != notice.identifier ^ mask
            || (le64(&rec[0x98..0xa0])>>13)&63 != u64::from(notice.handle)
            || le64(&rec[0x10..0x18]) >= notice.timestamp { return; }
        rec[0x10..0x18].copy_from_slice(&notice.timestamp.to_le_bytes());
        if let Some(status)=notice.status {
            // Exact 1001ecb0 stores, including its early return for type>2.
            let mut q=le64(&rec[8..16]) & !((1<<11)|(1<<12));
            if status[0] == 1 || status[0] == 2 {
                q|=1<<11;
                if status[0] == 1 { q|=1<<12; }
            }
            if status[0] <= 2 {
                let state=status[1].wrapping_sub(1)&7;
                q=(q & !(7<<13)) | (u64::from(if state>=5 { 0 } else { state })<<13);
                q=(q & !(15<<43)) | (u64::from(status[3]&15)<<43);
                q=(q & !(1<<16)) | (u64::from(status[2]&1)<<16);
                q=(q & !(7<<17)) | (u64::from((status[2]>>1)&7)<<17);
                let class=u16::from_le_bytes([status[4],status[5]]) & 0x3ff;
                q=(q & !(0x3ff<<33)) | (u64::from(class)<<33);
            }
            rec[8..16].copy_from_slice(&q.to_le_bytes());
            self.presence_known[index]=true;
        }
        if let Some(subs)=notice.subrecords {
            for i in 0..8 {
                let at=0x18+i*16; let src=&subs[i*16..][..16];
                rec[at]=(rec[at]&!1)|u8::from(src[0]!=0);
                rec[at+2..at+16].copy_from_slice(&src[2..]);
            }
        }
        for (i,sub) in notice.selected_slots.iter().enumerate() {
            if let Some(sub)=sub {
                let at=0x18+i*16;
                rec[at]|=1; rec[at+2..at+16].copy_from_slice(&sub[2..]);
            }
        }
        if let Some(sub)=notice.selected {
            let at=0x18+((le64(&rec[8..16])>>17)&7) as usize*16;
            rec[at]|=1; rec[at+2..at+16].copy_from_slice(&sub[2..]);
        }
        if let Some(name)=notice.name {
            rec[0xa0..].fill(0); rec[0xa0..0xaf].copy_from_slice(&name[..15]);
        }
        if let Some(location)=notice.location { self.locations[index]=Some(location); }
    }

    /// Getter results, including empty slots and A's index field.
    pub fn record(&self, alternate: bool, index: usize) -> Option<[u8; RECORD_LEN]> {
        let mut rec = *(if alternate { &self.b } else { &self.a }).get(index)?;
        let mut q8 = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        if q8 & 0xe000 >= 0xa000 { q8 &= !0xe000; }
        if !alternate { q8 = (q8 & !(0xff << 20)) | ((index as u64) << 20); }
        rec[8..16].copy_from_slice(&q8.to_le_bytes());
        Some(rec)
    }

    pub fn entries(&self) -> impl Iterator<Item = (bool, usize, [u8; RECORD_LEN])> + '_ {
        [false, true].into_iter().flat_map(move |alternate| {
            let table = if alternate { &self.b } else { &self.a };
            table.iter().enumerate().filter(|(_, r)| r[0x98] & 1 != 0)
                .map(move |(index, _)| (alternate, index, self.record(alternate, index).unwrap()))
        })
    }

    pub fn to_blob(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(BLOB_LEN));
        out.extend_from_slice(MAGIC);
        for table in [&self.a, &self.b] {
            for rec in table.iter() { out.extend_from_slice(rec); }
        }
        out
    }
}

impl Zeroize for Friends {
    fn zeroize(&mut self) { self.a.zeroize(); self.b.zeroize(); self.presence_known.zeroize(); self.locations.zeroize(); }
}
fn le64(bytes: &[u8]) -> u64 { u64::from_le_bytes(bytes.try_into().unwrap()) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ContentRecord, CONTENT_RECORD_LEN};


    #[test]
    fn native_record_stores_match_wire_fields_and_getter_indices() {
        let mask = 0x9182_7364_5546_3728;
        let flags = 0x4567_89ab_cdef_efe0u64;
        let mut raw = [0; CONTENT_RECORD_LEN];
        raw[..8].copy_from_slice(&flags.to_le_bytes());
        raw[8] = 199; raw[9] = 0xfd;
        raw[0x10..0x18].copy_from_slice(&123456u64.to_le_bytes());
        raw[0x18..0x1d].copy_from_slice(b"Alice");
        raw[0x28] = 0xff;
        raw[0x2a..0x38].copy_from_slice(&[7; 14]);
        let catalog = ContentCatalog { prefix: Zeroizing::new([0;8]), records: vec![ContentRecord::parse(&raw).unwrap()] };
        let friends = Friends::from_catalog(&catalog, mask);
        let rec = friends.record(false, 199).unwrap();
        assert_eq!(u64::from_le_bytes(rec[..8].try_into().unwrap()), 123456 ^ mask);
        assert_eq!(&rec[0xa0..0xa6], b"Alice\0");
        let q8 = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        assert_eq!((q8 >> 20) & 255, 199);
        assert_eq!((q8 >> 2) & 511, 253);
        assert_eq!(rec[0x18], 1);
        assert_eq!(&rec[0x1a..0x28], &[7;14]);
        assert_eq!(friends.entries().count(), 1);
        assert!(friends.record(false, 200).is_none());
        assert!(friends.record(true, 100).is_none());
    }

    #[test]
    fn alternate_list_last_entry_and_empty_refresh_are_distinct() {
        let mut raw = [0; CONTENT_RECORD_LEN]; raw[0] = 0x30; raw[8] = 99;
        let catalog = ContentCatalog { prefix: Zeroizing::new([0;8]), records: vec![ContentRecord::parse(&raw).unwrap()] };
        let friends = Friends::from_catalog(&catalog, 0);
        assert_eq!(friends.entries().map(|(a,i,_)| (a,i)).collect::<Vec<_>>(), [(true,99)]);
        assert_eq!(friends.record(false, 99).unwrap()[0x98] & 1, 0);
        let empty = ContentCatalog { prefix: Zeroizing::new([0;8]), records: vec![] };
        assert_eq!(Friends::from_catalog(&empty, 0).entries().count(), 0);
    }
}
