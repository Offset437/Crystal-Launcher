//! Read the verified game's own friend display cache. No game calls, writes,
//! network requests or hooks. POL alone continues to own presence and auth.

use crate::{lobby, log, proto};
use std::collections::BTreeMap;
use windows_sys::Win32::{
    Foundation::FreeLibrary,
    System::{
        Diagnostics::Debug::ReadProcessMemory, LibraryLoader::GetModuleHandleExW,
        Threading::GetCurrentProcess,
    },
};
use zeroize::Zeroizing;

pub const MAGIC: &[u8; 8] = b"XIPOLG\x01\0";
pub const RECORD_LEN: usize = 96; // CP932: character[16], area[64], world[16].
pub const BLOB_LEN: usize = 8 + proto::FRIEND_SLOTS * RECORD_LEN;

/// 101eaf3f/101eaf4d: [104dfd98] + 0x3c350 is my own zone.
const OWN_ZONE_RVA: usize = 0x4dfd98;
const OWN_ZONE_FIELD: usize = 0x3c350;
/// 10152f20 resolves a group through the table at 10371da8 (managed object)
/// or the legacy table at 10578548 + 4 * group.
const GROUP_TABLE_RVA: usize = 0x371da8;
const LEGACY_TABLE_RVA: usize = 0x578548;
/// Group 4 (area names) is table entry 21, object 105785f8.
const AREA_GROUP: (usize, usize, usize) = (4, 21, 0x5785f8);
/// 101cbc3a: a missing area name falls back to group 9 (entry 3, object
/// 10578658) index 0x40.
const FALLBACK_GROUP: (usize, usize, usize) = (9, 3, 0x578658);
const FALLBACK_INDEX: usize = 0x40;

type Reader<'a> = dyn Fn(usize, &mut [u8]) -> bool + 'a;
fn read<const N: usize>(reader: &Reader<'_>, at: usize) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    at.checked_add(N)?;
    reader(at, &mut bytes).then_some(bytes)
}
fn word(reader: &Reader<'_>, at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(read(reader, at)?))
}
fn dword(reader: &Reader<'_>, at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(read(reader, at)?) as usize)
}
fn process_read(at: usize, bytes: &mut [u8]) -> bool {
    let mut done = 0;
    // ReadProcessMemory rejects unmapped/partial ranges without dereferencing
    // game-owned pointers in Rust. Only this host's address space is read.
    unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            at as _,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut done,
        ) != 0
            && done == bytes.len()
    }
}

fn verified(base: usize, reader: &Reader<'_>) -> Option<()> {
    let nt = base.checked_add(dword(reader, base.checked_add(0x3c)?)?)?;
    if read::<4>(reader, nt)? != *b"PE\0\0"
        || word(reader, nt.checked_add(4)?)? != 0x14c
        || dword(reader, nt.checked_add(8)?)? != lobby::TIMESTAMP as usize
        || dword(reader, nt.checked_add(80)?)? != lobby::IMAGE_SIZE as usize
    {
        return None;
    }
    // Installed 100e6320: original cache getter, including relocated root.
    let mut expected = [
        0x66, 0x8b, 0x44, 0x24, 0x04, 0x66, 0x3d, 0x2c, 0x01, 0x7c, 0x03, 0x33, 0xc0, 0xc3, 0x8b,
        0x0d, 0, 0, 0, 0, 0x0f, 0xbf, 0xc0, 0xc1, 0xe0, 0x08, 0x8d, 0x84, 0x08, 0x90, 0x0a, 0, 0,
        0xc3,
    ];
    expected[16..20].copy_from_slice(
        &u32::try_from(base.checked_add(lobby::LOBBY_PTR_RVA)?)
            .ok()?
            .to_le_bytes(),
    );
    if read::<34>(reader, base.checked_add(0xe6320)?)? != expected
        || read::<22>(reader, base.checked_add(0xe7950)?)?
            != [
                0x8b, 0x44, 0x24, 0x04, 0xf6, 0x80, 0xb0, 0, 0, 0, 1, 0x74, 6, 5, 0xb4, 0, 0, 0,
                0xc3, 0x33, 0xc0, 0xc3,
            ]
        || read::<25>(reader, base.checked_add(0xe7970)?)?
            != [
                0x8b, 0x44, 0x24, 0x04, 0xf6, 0x80, 0xb0, 0, 0, 0, 0x40, 0x74, 8, 0x66, 0x8b, 0x80,
                0xe0, 0, 0, 0, 0xc3, 0x83, 0xc8, 0xff, 0xc3,
            ]
        || read::<12>(reader, base.checked_add(0xe7990)?)?
            != [0x8b, 0x44, 0x24, 4, 0x66, 0x8b, 0x80, 0xd8, 0, 0, 0, 0xc3]
    {
        return None;
    }
    // 101eaf3f/101eaf4d: the own-zone object and field the display compares.
    let mut own = [0x8b, 0x0d, 0, 0, 0, 0];
    own[2..].copy_from_slice(&u32::try_from(base.checked_add(OWN_ZONE_RVA)?).ok()?.to_le_bytes());
    // 101cbc3a: the group 9, index 0x40 fallback.
    let fallback = [0xc7, 0x44, 0x24, 8, 0x40, 0, 0, 0, 0xc7, 0x44, 0x24, 4, 9, 0, 0, 0];
    if read::<6>(reader, base.checked_add(0x1eaf3f)?)? != own
        || read::<6>(reader, base.checked_add(0x1eaf4d)?)? != [0x8b, 0x91, 0x50, 0xc3, 3, 0]
        || read::<16>(reader, base.checked_add(0x1cbc3a)?)? != fallback
    {
        return None;
    }
    for (group, entry, object) in [AREA_GROUP, FALLBACK_GROUP] {
        let at = base.checked_add(GROUP_TABLE_RVA + entry * 8)?;
        if dword(reader, at)? != group || dword(reader, at.checked_add(4)?)? != base.checked_add(object)? {
            return None;
        }
    }
    Some(())
}

fn bounded_text<const N: usize>(reader: &Reader<'_>, at: usize) -> Option<[u8; N]> {
    let mut text = read::<N>(reader, at)?;
    let end = text.iter().position(|b| *b == 0)?;
    if end == 0 || text[..end].iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return None;
    }
    text[end..].fill(0);
    Some(text)
}

/// 101cbc10(zone, 0): the game's localized area string (group 4), else its
/// group 9 index 0x40 fallback text.
fn area_name(base: usize, zone: usize, reader: &Reader<'_>) -> Option<[u8; 64]> {
    resource_text(base, AREA_GROUP, zone, reader)
        .or_else(|| resource_text(base, FALLBACK_GROUP, FALLBACK_INDEX, reader))
}

/// One string of a resource group. Mirror only its read-only lookup
/// (10152f20/102578b0), never its loading/decryption routines.
fn resource_text(base: usize, (group, _, object): (usize, usize, usize), zone: usize, reader: &Reader<'_>) -> Option<[u8; 64]> {
    let object = base.checked_add(object)?;
    let header = dword(reader, object.checked_add(4)?)?;
    if header == 0 {
        // 10152f20 falls back to the group's legacy table only when the
        // managed resource is absent. Its lookup is 101530c0/10153090.
        let pointer = base.checked_add(LEGACY_TABLE_RVA + 4 * group)?;
        let table = dword(reader, pointer)?;
        if table == 0 {
            return None;
        }
        let count = dword(reader, table.checked_add(0x24)?)?;
        if count > 0x1000 || zone >= count {
            return None;
        }
        let kind = word(reader, table.checked_add(0xa)?)? as i16;
        let (stride, offset) = if kind < 2 {
            (
                4,
                word(reader, table.checked_add(0x38 + zone * 4)?)? as i16 as isize,
            )
        } else {
            (
                12,
                isize::try_from(dword(reader, table.checked_add(0x38 + zone * 12)?)?).ok()?,
            )
        };
        let at = table
            .checked_add(0x38 + count * stride)?
            .checked_add_signed(offset)?;
        let text = bounded_text(reader, at)?;
        return (dword(reader, pointer)? == table
            && dword(reader, table.checked_add(0x24)?)? == count
            && word(reader, table.checked_add(0xa)?)? as i16 == kind
            && dword(reader, object.checked_add(4)?)? == 0)
            .then_some(text);
    }
    let count = dword(reader, header.checked_add(0x28)?)?;
    if count > 0x1000 || zone >= count {
        return None;
    }
    let data = dword(reader, object.checked_add(0xc)?)?;
    if data == 0 {
        return None;
    }
    let at = if word(reader, header.checked_add(8)?)? == 0x10 {
        let variants = word(reader, header.checked_add(0x3e)?)? as usize;
        if variants == 0 || variants > 16 {
            return None;
        }
        let entry = data.checked_add(zone.checked_mul(variants)?.checked_mul(8)?)?;
        if dword(reader, entry.checked_add(4)?)? == 0 {
            return None;
        }
        data.checked_add(dword(reader, entry)?)?
    } else {
        let entry = if dword(reader, header.checked_add(0x1c)?)? != 0 {
            let index = dword(reader, object.checked_add(8)?)?;
            data.checked_add(dword(reader, index.checked_add(zone.checked_mul(8)?)?)?)?
        } else {
            let stride = dword(reader, header.checked_add(0x20)?)?;
            if stride == 0 || stride > 0x10000 {
                return None;
            }
            data.checked_add(zone.checked_mul(stride)?)?
        };
        if word(reader, entry)? == 0 || dword(reader, entry.checked_add(8)?)? != 0 {
            return None;
        }
        let text = entry.checked_add(dword(reader, entry.checked_add(4)?)?)?;
        if word(reader, text)? != 1 {
            return None;
        }
        text.checked_add(0x1c)?
    };
    let text = bounded_text(reader, at)?;
    // Resource identity must still match after copying its text.
    (dword(reader, object.checked_add(4)?)? == header
        && dword(reader, object.checked_add(0xc)?)? == data)
        .then_some(text)
}

fn world_name(root: usize, world: u8, reader: &Reader<'_>) -> Option<[u8; 16]> {
    let count = dword(reader, root.checked_add(0x14140)?)?;
    if count > 256 {
        return None;
    }
    for index in 0..count {
        let at = root.checked_add(0x14144 + index * 20)?;
        if dword(reader, at)? == world as usize {
            return bounded_text(reader, at.checked_add(4)?);
        }
    }
    None
}

fn selected(record: &[u8]) -> Option<&[u8]> {
    let flags = u64::from_le_bytes(record[8..16].try_into().ok()?);
    if record[0x98] & 1 == 0
        || flags & (1 << 11) == 0
        || flags & (1 << 16) == 0
        || (flags >> 33) & 0x3ff != 1
        || record[0x10..0x18] == [0; 8]
    {
        return None;
    }
    let at = 0x18 + ((flags >> 17) & 7) as usize * 16;
    let character = &record[at..at + 16];
    (character[0] & 1 != 0 && character[2..4] == [1, 0]).then_some(character)
}

/// My own zone ([104dfd98] + 0x3c350), or None when that object is absent.
fn own_zone(base: usize, reader: &Reader<'_>) -> Option<usize> {
    let object = dword(reader, base.checked_add(OWN_ZONE_RVA)?)?;
    (object != 0).then_some(())?;
    dword(reader, object.checked_add(OWN_ZONE_FIELD)?)
}

/// 101eaec0, same-world branch. The row's +0xb0..+0xf8 is copied; its word
/// +0xd8 is the zone. The area (field 0x23) shows in both visibility branches
/// whenever my zone is known and differs (101eaf49/101eafc6); bit 14 of +0xe0
/// does not hide it. Visible (100e79a0: +0xb0 bit 6 and not +0xe0 bit 14)
/// always shows the name (field 0x25, +0xb4); hidden shows it only with +0xb0
/// bit 0 (100e7950).
fn shown_details(game: &[u8; 0x100], own_zone: Option<usize>) -> (bool, Option<usize>) {
    let visible = game[0xb0] & 0x40 != 0 && u32::from_le_bytes(game[0xe0..0xe4].try_into().unwrap()) & (1 << 14) == 0;
    let zone = u16::from_le_bytes(game[0xd8..0xda].try_into().unwrap()) as usize;
    (visible || game[0xb0] & 1 != 0, own_zone.filter(|own| *own != zone).map(|_| zone))
}

// Anonymous counts only. Inspect the stages separately: treating every blank
// result as "no FFXI friends" concealed whether POL, the game cache or its
// localized resources lacked a field. Never retain or log record identities.
#[derive(Debug, Default, PartialEq, Eq)]
struct Diagnostics {
    catalog: usize,
    online: usize,
    unknown: usize,
    applications: BTreeMap<u16, usize>,
    modes: BTreeMap<u8, usize>,
    linked: usize,
    selected_classes: BTreeMap<u16, usize>,
    game_ready: bool,
    matched: usize,
    game_applications: BTreeMap<u16, usize>,
    same_world_links: usize,
    completed: usize,
    characters: usize,
    visible_areas: usize,
    localized_areas: usize,
}

fn diagnostics(base: usize, native: &[u8], reader: &Reader<'_>) -> Diagnostics {
    let mut out = Diagnostics::default();
    if native.len() != proto::FRIENDS_LEN {
        return out;
    }
    let root = base
        .checked_add(lobby::LOBBY_PTR_RVA)
        .and_then(|at| dword(reader, at))
        .filter(|r| *r != 0);
    let own_world = root
        .and_then(|r| word(reader, r.checked_add(0x130)?))
        .filter(|w| *w != 0xffff);
    out.game_ready = own_world.is_some();
    let own_zone = own_zone(base, reader);
    for (index, r) in native[8..]
        .chunks_exact(proto::FRIEND_RECORD_LEN)
        .enumerate()
    {
        if r[0x98] & 1 == 0 {
            continue;
        }
        out.catalog += 1;
        if index >= 200 || r[0x10..0x18] == [0; 8] {
            out.unknown += 1;
            continue;
        }
        let flags = u64::from_le_bytes(r[8..16].try_into().unwrap());
        if flags & (1 << 11) == 0 {
            continue;
        }
        out.online += 1;
        *out.applications
            .entry(((flags >> 33) & 0x3ff) as u16)
            .or_default() += 1;
        *out.modes.entry(((flags >> 13) & 7) as u8).or_default() += 1;
        if flags & (1 << 16) != 0 {
            let at = 0x18 + ((flags >> 17) & 7) as usize * 16;
            let character = &r[at..at + 16];
            if character[0] & 1 != 0 {
                out.linked += 1;
                *out.selected_classes
                    .entry(u16::from_le_bytes(character[2..4].try_into().unwrap()))
                    .or_default() += 1;
                out.same_world_links += usize::from(own_world == Some(character[6] as u16));
            }
        }
        let Some(root) = root else {
            continue;
        };
        let Some(at) = root.checked_add(0xa90 + index * 0x100) else {
            continue;
        };
        let Some(game) = read::<0x100>(reader, at) else {
            continue;
        };
        if game[..8] != r[..8] || game[0x98..0xa0] != r[0x98..0xa0] {
            continue;
        }
        if read::<0x100>(reader, at).as_ref() != Some(&game) {
            continue;
        }
        out.matched += 1;
        let game_flags = u64::from_le_bytes(game[8..16].try_into().unwrap());
        *out.game_applications
            .entry(((game_flags >> 33) & 0x3ff) as u16)
            .or_default() += 1;
        if game[0xfc] & 1 == 0 {
            continue;
        }
        out.completed += 1;
        let (named, area) = shown_details(&game, own_zone);
        out.characters += usize::from(named);
        if let Some(zone) = area {
            out.visible_areas += 1;
            out.localized_areas += usize::from(area_name(base, zone, reader).is_some());
        }
    }
    out
}

fn copy_locations(base: usize, native: &[u8], reader: &Reader<'_>) -> Option<Zeroizing<Vec<u8>>> {
    if native.len() != proto::FRIENDS_LEN {
        return None;
    }
    let root_at = base.checked_add(lobby::LOBBY_PTR_RVA)?;
    let root = dword(reader, root_at)?;
    if root == 0 {
        return None;
    }
    let own_world = word(reader, root.checked_add(0x130)?)?;
    if own_world == 0xffff {
        return None;
    }
    let own_zone = own_zone(base, reader);
    let mut out = Zeroizing::new(vec![0; BLOB_LEN]);
    out[..8].copy_from_slice(MAGIC);
    for (index, record) in native[8..]
        .chunks_exact(proto::FRIEND_RECORD_LEN)
        .enumerate()
    {
        let Some(character) = selected(record) else {
            continue;
        };
        let detail = &mut out[8 + index * RECORD_LEN..][..RECORD_LEN];
        let world = character[6];
        // 101eaee6/100e7900: another world, or a row the game has not
        // completed (+0xfc bit 0), shows only the world label (100db7b0).
        let world_label = |detail: &mut [u8]| {
            if let Some(name) = world_name(root, world, reader) {
                detail[80..96].copy_from_slice(&name);
            }
        };
        if world as u16 != own_world {
            world_label(detail);
            continue;
        }
        let at = root.checked_add(0xa90 + index * 0x100)?;
        let Some(game) = read::<0x100>(reader, at) else {
            continue;
        };
        // Account, handle and selected character must match. A slot can be
        // reused while POL and the game are updating their independent caches.
        let Some(game_character) = selected(&game) else {
            continue;
        };
        if game[..8] != record[..8]
            || game[0x98..0xa0] != record[0x98..0xa0]
            || game_character != character
        {
            continue;
        }
        if game[0xfc] & 1 == 0 {
            world_label(detail);
        } else {
            let packed = u32::from_le_bytes(character[4..8].try_into().ok()?);
            let id = (packed & 0xffff) | ((packed >> 8) & 0xf0000);
            if u32::from_le_bytes(game[0xc8..0xcc].try_into().ok()?) != id {
                continue;
            }
            let (named, area) = shown_details(&game, own_zone);
            if named {
                if let Some(name) = bounded_text::<16>(reader, at.checked_add(0xb4)?) {
                    detail[..16].copy_from_slice(&name);
                }
            }
            if let Some(name) = area.and_then(|zone| area_name(base, zone, reader)) {
                detail[16..80].copy_from_slice(&name);
            }
        }
        // Reject a torn read instead of publishing mixed character/area data.
        if read::<0x100>(reader, at).as_ref() != Some(&game) {
            detail.fill(0);
        }
    }
    (dword(reader, root_at)? == root
        && word(reader, root.checked_add(0x130)?)? == own_world
        && own_zone == self::own_zone(base, reader))
        .then_some(out)
}

#[derive(Default)]
pub(crate) struct GameFriends {
    module: usize,
    unavailable: bool,
    diagnostics: Option<Diagnostics>,
}
impl GameFriends {
    pub(crate) fn snapshot(&mut self, native: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if self.unavailable {
            return None;
        }
        if self.module == 0 {
            let name: Vec<u16> = "FFXiMain.dll\0".encode_utf16().collect();
            let mut module = std::ptr::null_mut();
            if unsafe { GetModuleHandleExW(0, name.as_ptr(), &mut module) } == 0 {
                return None;
            }
            self.module = module as usize;
            if verified(self.module, &process_read).is_none() {
                self.unavailable = true;
                log!("native friend locations unavailable: unsupported game layout");
                return None;
            }
            log!("native friend locations: verified read-only game cache enabled");
        }
        let counts = diagnostics(self.module, native, &process_read);
        if self.diagnostics.as_ref() != Some(&counts) {
            log!("native friend display inputs: {counts:?}");
            self.diagnostics = Some(counts);
        }
        copy_locations(self.module, native, &process_read)
    }
}
impl Drop for GameFriends {
    fn drop(&mut self) {
        if self.module != 0 {
            unsafe {
                FreeLibrary(self.module as _);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::BTreeMap};
    const BASE: usize = 0x10000000;
    const ROOT: usize = 0x20000000;
    const HEADER: usize = 0x21000000;
    const DATA: usize = 0x22000000;
    const ZONE: usize = 0x23000000;
    const FALLBACK_HEADER: usize = 0x24000000;
    const FALLBACK_DATA: usize = 0x25000000;
    struct Fixture {
        memory: BTreeMap<usize, Vec<u8>>,
        native: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            let mut f = Self {
                memory: BTreeMap::new(),
                native: vec![0; proto::FRIENDS_LEN],
            };
            f.native[..8].copy_from_slice(b"XIPOLF\x01\0");
            let rec = &mut f.native[8..8 + proto::FRIEND_RECORD_LEN];
            rec[..8].copy_from_slice(&123u64.to_le_bytes());
            rec[8..16].copy_from_slice(&((1u64 << 11) | (1u64 << 16) | (1u64 << 33)).to_le_bytes());
            rec[0x10..0x18].copy_from_slice(&321u64.to_le_bytes());
            rec[0x18] = 1;
            rec[0x1a] = 1;
            rec[0x1c..0x20].copy_from_slice(&((28u32 << 16) | 42).to_le_bytes());
            rec[0x98] = 1;
            f.memory.insert(
                BASE + lobby::LOBBY_PTR_RVA,
                (ROOT as u32).to_le_bytes().to_vec(),
            );
            f.memory.insert(ROOT, vec![0; 0x14300]);
            f.put(ROOT + 0x130, &28u16.to_le_bytes());
            let rec = f.native[8..8 + proto::FRIEND_RECORD_LEN].to_vec();
            f.put(ROOT + 0xa90, &rec);
            f.put(ROOT + 0xa90 + 0xfc, &[1]);
            f.put(ROOT + 0xa90 + 0xb0, &[0x41]);
            f.put(ROOT + 0xa90 + 0xb4, b"Alice\0");
            f.put(ROOT + 0xa90 + 0xc8, &42u32.to_le_bytes());
            f.put(ROOT + 0xa90 + 0xd8, &1u16.to_le_bytes());
            f.put(ROOT + 0xa90 + 0xe0, &0u16.to_le_bytes());
            f.memory.insert(BASE + 0x5785f8, vec![0; 16]);
            f.put(BASE + 0x5785fc, &(HEADER as u32).to_le_bytes());
            f.put(BASE + 0x578604, &(DATA as u32).to_le_bytes());
            f.memory.insert(HEADER, vec![0; 64]);
            f.put(HEADER + 8, &0x10u16.to_le_bytes());
            f.put(HEADER + 0x28, &2u32.to_le_bytes());
            f.put(HEADER + 0x3e, &1u16.to_le_bytes());
            f.memory.insert(DATA, vec![0; 160]);
            f.put(DATA + 8, &32u32.to_le_bytes());
            f.put(DATA + 12, &1u32.to_le_bytes());
            f.put(DATA + 32, b"Bastok Markets\0");
            // I am in zone 5; the friend's row says zone 1.
            f.memory.insert(BASE + OWN_ZONE_RVA, (ZONE as u32).to_le_bytes().to_vec());
            f.memory.insert(ZONE + OWN_ZONE_FIELD, 5u32.to_le_bytes().to_vec());
            // Group 9 (managed), index 0x40.
            f.memory.insert(BASE + FALLBACK_GROUP.2, vec![0; 16]);
            f.put(BASE + FALLBACK_GROUP.2 + 4, &(FALLBACK_HEADER as u32).to_le_bytes());
            f.put(BASE + FALLBACK_GROUP.2 + 12, &(FALLBACK_DATA as u32).to_le_bytes());
            f.memory.insert(FALLBACK_HEADER, vec![0; 64]);
            f.put(FALLBACK_HEADER + 8, &0x10u16.to_le_bytes());
            f.put(FALLBACK_HEADER + 0x28, &0x41u32.to_le_bytes());
            f.put(FALLBACK_HEADER + 0x3e, &1u16.to_le_bytes());
            f.memory.insert(FALLBACK_DATA, vec![0; 0x300]);
            f.put(FALLBACK_DATA + 0x200, &0x280u32.to_le_bytes());
            f.put(FALLBACK_DATA + 0x204, &1u32.to_le_bytes());
            f.put(FALLBACK_DATA + 0x280, b"Somewhere\0");
            // World table entry for my world 28.
            f.put(ROOT + 0x14140, &1u32.to_le_bytes());
            f.put(ROOT + 0x14144, &28u32.to_le_bytes());
            f.put(ROOT + 0x14148, b"Bahamut\0");
            f
        }
        fn put(&mut self, at: usize, bytes: &[u8]) {
            let (&start, block) = self.memory.range_mut(..=at).next_back().unwrap();
            block[at - start..at - start + bytes.len()].copy_from_slice(bytes);
        }
        fn read(&self, at: usize, out: &mut [u8]) -> bool {
            let Some((&start, block)) = self.memory.range(..=at).next_back() else {
                return false;
            };
            let Some(bytes) = block.get(at - start..at - start + out.len()) else {
                return false;
            };
            out.copy_from_slice(bytes);
            true
        }
        fn locations(&self) -> Zeroizing<Vec<u8>> {
            copy_locations(BASE, &self.native, &|a, b| self.read(a, b)).unwrap()
        }
    }
    #[test]
    fn same_world_reads_character_and_localized_area_without_world_label() {
        let f = Fixture::new();
        let loc = f.locations();
        assert_eq!(&loc[..8], MAGIC);
        assert_eq!(&loc[8..14], b"Alice\0");
        assert_eq!(&loc[24..39], b"Bastok Markets\0");
        assert!(loc[88..104].iter().all(|b| *b == 0));
    }
    #[test]
    fn diagnostics_distinguish_missing_application_from_available_game_fields() {
        let mut f = Fixture::new();
        // Keep the selected character and completed game lookup; remove only
        // the native record's reported application. The aggregate must reveal
        // this difference without logging a name, account or character ID.
        let flags = (1u64 << 11) | (1u64 << 16);
        f.native[16..24].copy_from_slice(&flags.to_le_bytes());
        let counts = diagnostics(BASE, &f.native, &|a, b| f.read(a, b));
        assert_eq!((counts.catalog, counts.online, counts.linked), (1, 1, 1));
        assert_eq!(counts.applications.get(&0), Some(&1));
        assert_eq!(counts.selected_classes.get(&1), Some(&1));
        assert_eq!(counts.game_applications.get(&1), Some(&1));
        assert_eq!(
            (
                counts.completed,
                counts.characters,
                counts.visible_areas,
                counts.localized_areas
            ),
            (1, 1, 1, 1)
        );
        let text = format!("{counts:?}");
        assert!(!text.contains("Alice") && !text.contains("Bastok") && !text.contains("123"));
        assert_eq!(
            diagnostics(BASE, &f.native[..1], &|_, _| false),
            Diagnostics::default()
        );
    }
    #[test]
    fn privacy_hides_only_the_name_and_an_incomplete_row_shows_the_world() {
        let mut f = Fixture::new();
        // Hidden (+0xe0 bit 14) with +0xb0 bit 0: name and area (101eaf49).
        f.put(ROOT + 0xa90 + 0xe0, &0x4001u16.to_le_bytes());
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        // Hidden without bit 0: the area only (100e7950).
        f.put(ROOT + 0xa90 + 0xb0, &[0x40]);
        assert!(f.locations()[8..24].iter().all(|b| *b == 0));
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        // Visible: the name even without bit 0 (101eaff6).
        f.put(ROOT + 0xa90 + 0xe0, &0u16.to_le_bytes());
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        // Not completed by the game (+0xfc bit 0): the world label only.
        f.put(ROOT + 0xa90 + 0xfc, &[0]);
        let loc = f.locations();
        assert!(loc[8..88].iter().all(|b| *b == 0));
        assert_eq!(&loc[88..96], b"Bahamut\0");
    }
    #[test]
    fn the_area_is_hidden_only_in_my_own_zone_or_without_my_zone() {
        let mut f = Fixture::new();
        f.put(ZONE + OWN_ZONE_FIELD, &1u32.to_le_bytes());
        assert!(f.locations()[24..88].iter().all(|b| *b == 0));
        assert_eq!(&f.locations()[8..14], b"Alice\0");
        f.put(ZONE + OWN_ZONE_FIELD, &5u32.to_le_bytes());
        f.put(BASE + OWN_ZONE_RVA, &0u32.to_le_bytes());
        assert!(f.locations()[24..88].iter().all(|b| *b == 0));
    }
    #[test]
    fn a_missing_area_name_uses_the_group_9_fallback() {
        let mut f = Fixture::new();
        f.put(ROOT + 0xa90 + 0xd8, &7u16.to_le_bytes());
        assert_eq!(&f.locations()[24..34], b"Somewhere\0");
    }
    #[test]
    fn zone_id_is_separate_from_status_bits_and_changes_with_the_game_cache() {
        let mut f = Fixture::new();
        // Two real table entries make a mistaken privacy-as-zone lookup return
        // plausible text, rather than just failing on an out-of-range index.
        f.put(DATA, &64u32.to_le_bytes());
        f.put(DATA + 4, &1u32.to_le_bytes());
        f.put(DATA + 64, b"Windurst Waters\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(ROOT + 0xa90 + 0xe0, &0x3000u16.to_le_bytes());
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(ROOT + 0xa90 + 0xd8, &0u16.to_le_bytes());
        assert_eq!(&f.locations()[24..40], b"Windurst Waters\0");
        // Bit 14 hides the name, never the area (101eaf49).
        f.put(ROOT + 0xa90 + 0xe0, &0x4000u16.to_le_bytes());
        assert_eq!(&f.locations()[24..40], b"Windurst Waters\0");
        assert_eq!(&f.locations()[8..14], b"Alice\0");
    }
    #[test]
    fn legacy_area_fallback_matches_both_native_table_formats() {
        let mut f = Fixture::new();
        f.put(BASE + 0x5785fc, &0u32.to_le_bytes());
        f.memory
            .insert(BASE + 0x578558, (HEADER as u32).to_le_bytes().to_vec());
        f.memory.insert(HEADER, vec![0; 160]);
        f.put(HEADER + 0xa, &1u16.to_le_bytes());
        f.put(HEADER + 0x24, &2u32.to_le_bytes());
        f.put(HEADER + 0x3c, &8u16.to_le_bytes());
        f.put(HEADER + 0x48, b"Bastok Markets\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        f.put(HEADER + 0xa, &2u16.to_le_bytes());
        f.put(HEADER + 0x44, &8u32.to_le_bytes());
        f.put(HEADER + 0x58, b"Bastok Markets\0");
        assert_eq!(&f.locations()[24..39], b"Bastok Markets\0");
        assert!(resource_text(BASE, AREA_GROUP, 2, &|a, b| f.read(a, b)).is_none());
        let reads = Cell::new(0);
        assert!(resource_text(BASE, AREA_GROUP, 1, &|at, out| {
            let ok = f.read(at, out);
            if at == BASE + 0x578558 {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    out.fill(0);
                }
            }
            ok
        })
        .is_none());
    }
    #[test]
    fn another_world_uses_native_world_name_and_does_not_reuse_same_world_details() {
        let mut f = Fixture::new();
        f.native[8 + 0x1e] = 16;
        f.put(ROOT + 0x14140, &1u32.to_le_bytes());
        f.put(ROOT + 0x14144, &16u32.to_le_bytes());
        f.put(ROOT + 0x14148, b"Phoenix\0");
        let loc = f.locations();
        assert!(loc[8..88].iter().all(|b| *b == 0));
        assert_eq!(&loc[88..96], b"Phoenix\0");
    }
    #[test]
    fn stale_character_account_or_unknown_presence_cannot_supply_details() {
        let mut f = Fixture::new();
        f.put(ROOT + 0xa90 + 0xc8, &43u32.to_le_bytes());
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
        f.put(ROOT + 0xa90 + 0xc8, &42u32.to_le_bytes());
        f.put(ROOT + 0xa90, &124u64.to_le_bytes());
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
        f.put(ROOT + 0xa90, &123u64.to_le_bytes());
        f.native[8 + 0x10..8 + 0x18].fill(0);
        assert!(f.locations()[8..104].iter().all(|b| *b == 0));
    }
    #[test]
    fn concurrent_row_update_is_rejected_and_unmapped_root_is_safe() {
        let f = Fixture::new();
        let reads = Cell::new(0);
        let loc = copy_locations(BASE, &f.native, &|at, out| {
            let ok = f.read(at, out);
            if at == ROOT + 0xa90 && out.len() == 0x100 {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    out[0xe0] ^= 1;
                }
            }
            ok
        })
        .unwrap();
        assert!(loc[8..104].iter().all(|b| *b == 0));
        assert!(copy_locations(BASE, &f.native, &|_, _| false).is_none());
    }
    #[test]
    fn supported_layout_checks_relocation_machine_and_getter_code() {
        let mut f = Fixture::new();
        f.memory.insert(BASE, vec![0; 512]);
        f.put(BASE + 0x3c, &0x80u32.to_le_bytes());
        f.put(BASE + 0x80, b"PE\0\0");
        f.put(BASE + 0x84, &0x14cu16.to_le_bytes());
        f.put(BASE + 0x88, &lobby::TIMESTAMP.to_le_bytes());
        f.put(BASE + 0xd0, &lobby::IMAGE_SIZE.to_le_bytes());
        let mut getter = vec![
            0x66, 0x8b, 0x44, 0x24, 4, 0x66, 0x3d, 0x2c, 1, 0x7c, 3, 0x33, 0xc0, 0xc3, 0x8b, 0x0d,
        ];
        getter.extend_from_slice(&((BASE + lobby::LOBBY_PTR_RVA) as u32).to_le_bytes());
        getter.extend_from_slice(&[
            0x0f, 0xbf, 0xc0, 0xc1, 0xe0, 8, 0x8d, 0x84, 8, 0x90, 0x0a, 0, 0, 0xc3,
        ]);
        f.memory.insert(BASE + 0xe6320, getter);
        f.memory.insert(
            BASE + 0xe7950,
            vec![
                0x8b, 0x44, 0x24, 4, 0xf6, 0x80, 0xb0, 0, 0, 0, 1, 0x74, 6, 5, 0xb4, 0, 0, 0, 0xc3,
                0x33, 0xc0, 0xc3,
            ],
        );
        f.memory.insert(
            BASE + 0xe7970,
            vec![
                0x8b, 0x44, 0x24, 4, 0xf6, 0x80, 0xb0, 0, 0, 0, 0x40, 0x74, 8, 0x66, 0x8b, 0x80,
                0xe0, 0, 0, 0, 0xc3, 0x83, 0xc8, 0xff, 0xc3,
            ],
        );
        f.memory.insert(
            BASE + 0xe7990,
            vec![0x8b, 0x44, 0x24, 4, 0x66, 0x8b, 0x80, 0xd8, 0, 0, 0, 0xc3],
        );
        let mut own = vec![0x8b, 0x0d];
        own.extend_from_slice(&((BASE + OWN_ZONE_RVA) as u32).to_le_bytes());
        f.memory.insert(BASE + 0x1eaf3f, own);
        f.memory.insert(BASE + 0x1eaf4d, vec![0x8b, 0x91, 0x50, 0xc3, 3, 0]);
        f.memory.insert(BASE + 0x1cbc3a, vec![0xc7, 0x44, 0x24, 8, 0x40, 0, 0, 0, 0xc7, 0x44, 0x24, 4, 9, 0, 0, 0]);
        f.memory.insert(BASE + GROUP_TABLE_RVA, vec![0; 0xe0]);
        for (group, entry, object) in [AREA_GROUP, FALLBACK_GROUP] {
            f.put(BASE + GROUP_TABLE_RVA + entry * 8, &(group as u32).to_le_bytes());
            f.put(BASE + GROUP_TABLE_RVA + entry * 8 + 4, &((BASE + object) as u32).to_le_bytes());
        }
        assert!(verified(BASE, &|a, b| f.read(a, b)).is_some());
        f.put(BASE + GROUP_TABLE_RVA + 3 * 8, &10u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b)).is_none(), "group table must map 9");
        f.put(BASE + GROUP_TABLE_RVA + 3 * 8, &9u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b)).is_some());
        f.put(BASE + 0x84, &0x8664u16.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b)).is_none());
        f.put(BASE + 0x84, &0x14cu16.to_le_bytes());
        f.put(BASE + 0xe6320 + 16, &0u32.to_le_bytes());
        assert!(verified(BASE, &|a, b| f.read(a, b)).is_none());
    }
}
