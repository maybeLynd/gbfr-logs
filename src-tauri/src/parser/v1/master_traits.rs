use std::{
    array,
    collections::HashMap,
    env, fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

use serde::Deserialize;

use super::{Sigil, WeaponInfo};

const MASTER_TRAITS_ID_TYPE: u32 = 3007;
const PARTY_WEAPON_SLOT_ID_TYPE: u32 = 1402;
const PARTY_SIGIL_LIST_ID_TYPE: u32 = 1403;
const SIGIL_ACQUISITION_ID_TYPE: u32 = 2702;
const SIGIL_ID_TYPE: u32 = 2703;
const SIGIL_LEVEL_ID_TYPE: u32 = 2704;
const SIGIL_EQUIPPED_CHARACTER_ID_TYPE: u32 = 2706;
const SIGIL_NOTIFICATION_ID_TYPE: u32 = 2707;
const WEAPON_SLOT_ID_TYPE: u32 = 2802;
const WEAPON_ID_TYPE: u32 = 2803;
const WEAPON_EXPERIENCE_ID_TYPE: u32 = 2804;
const WEAPON_STAR_LEVEL_ID_TYPE: u32 = 2805;
const WEAPON_PLUS_MARKS_ID_TYPE: u32 = 2806;
const WEAPON_AWAKENING_LEVEL_ID_TYPE: u32 = 2807;
const WEAPON_WRIGHTSTONE_ID_TYPE: u32 = 2816;
const WEAPON_UNIT_MIN: u32 = 40_000;
const WEAPON_UNIT_MAX: u32 = 40_255;
const ACTIVE_PARTY_UNIT_BASE: u32 = 104_000;
const PARTY_SIZE: usize = 4;
const SIGIL_SLOT_COUNT: usize = 12;
const SAVED_NODE_COUNT: usize = 50;
const INVALID_ID: u32 = 0x887A_E0B0;
const UNIT_HEADER_SIZE: usize = 16;

#[derive(Clone)]
struct CachedBuilds {
    path: PathBuf,
    modified: SystemTime,
    builds: [Vec<u32>; PARTY_SIZE],
    weapons: [Option<WeaponInfo>; PARTY_SIZE],
    sigils: [Vec<Sigil>; PARTY_SIZE],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SigilTraitAsset {
    sigils: Vec<SigilTraitRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SigilTraitRow {
    sigil_id: u32,
    first_trait_id: u32,
    second_trait_id: u32,
}

#[derive(Deserialize)]
struct WeaponDataAsset {
    experience: Vec<u32>,
}

static SIGIL_TRAITS: OnceLock<HashMap<u32, (u32, u32)>> = OnceLock::new();
static WEAPON_EXPERIENCE: OnceLock<Vec<u32>> = OnceLock::new();

static CACHE: OnceLock<Mutex<Option<CachedBuilds>>> = OnceLock::new();

pub(super) fn load_for_party(party_index: u8) -> Vec<u32> {
    let index = party_index as usize;
    if index >= PARTY_SIZE {
        return Vec::new();
    }

    cached_save()
        .map(|cached| cached.builds[index].clone())
        .unwrap_or_default()
}

pub(super) fn load_weapon_for_party(party_index: u8) -> Option<WeaponInfo> {
    let index = party_index as usize;
    if index >= PARTY_SIZE {
        return None;
    }
    cached_save()?.weapons[index].clone()
}

pub(super) fn load_sigils_for_party(party_index: u8) -> Vec<Sigil> {
    let index = party_index as usize;
    if index >= PARTY_SIZE {
        return Vec::new();
    }

    cached_save()
        .map(|cached| cached.sigils[index].clone())
        .unwrap_or_default()
}

fn cached_save() -> Option<CachedBuilds> {
    let Some((path, modified)) = newest_save() else {
        return None;
    };
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("master traits cache lock poisoned");
    if let Some(cached) = cache.as_ref() {
        if cached.path == path && cached.modified == modified {
            return Some(cached.clone());
        }
    }

    let Ok(bytes) = fs::read(&path) else {
        return None;
    };
    let builds = parse_active_builds(&bytes);
    let weapons = parse_active_weapons(&bytes);
    let sigils = parse_active_sigils(&bytes);
    let cached = CachedBuilds {
        path,
        modified,
        builds,
        weapons,
        sigils,
    };
    *cache = Some(cached.clone());
    Some(cached)
}

fn sigil_traits() -> &'static HashMap<u32, (u32, u32)> {
    SIGIL_TRAITS.get_or_init(|| {
        serde_json::from_str::<SigilTraitAsset>(include_str!("../../../assets/sigil-traits.json"))
            .map(|asset| {
                asset
                    .sigils
                    .into_iter()
                    .map(|row| (row.sigil_id, (row.first_trait_id, row.second_trait_id)))
                    .collect()
            })
            .unwrap_or_default()
    })
}

pub(super) fn sigil_trait_ids(sigil_id: u32) -> Option<(u32, u32)> {
    sigil_traits().get(&sigil_id).copied()
}

fn weapon_experience() -> &'static [u32] {
    WEAPON_EXPERIENCE.get_or_init(|| {
        serde_json::from_str::<WeaponDataAsset>(include_str!("../../../assets/weapon-data.json"))
            .map(|asset| asset.experience)
            .unwrap_or_default()
    })
}

fn weapon_level_from_experience(experience: u32) -> u32 {
    weapon_experience().partition_point(|threshold| *threshold <= experience) as u32
}

fn newest_save() -> Option<(PathBuf, SystemTime)> {
    let root = PathBuf::from(env::var_os("LOCALAPPDATA")?)
        .join("GBFR")
        .join("Saved")
        .join("SaveGames");
    (1..=3)
        .filter_map(|index| {
            let path = root.join(format!("SaveData{index}.dat"));
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some((path, modified))
        })
        .max_by_key(|(_, modified)| *modified)
}

fn parse_active_builds(bytes: &[u8]) -> [Vec<u32>; PARTY_SIZE] {
    let mut builds: [Vec<u32>; PARTY_SIZE] = array::from_fn(|_| Vec::new());
    let value_size = SAVED_NODE_COUNT * std::mem::size_of::<u32>();
    if bytes.len() < UNIT_HEADER_SIZE + value_size {
        return builds;
    }

    for offset in (0..=bytes.len() - UNIT_HEADER_SIZE - value_size).step_by(4) {
        if read_u32(bytes, offset) != Some(MASTER_TRAITS_ID_TYPE)
            || read_u32(bytes, offset + 8) != Some(4)
            || read_u32(bytes, offset + 12) != Some(SAVED_NODE_COUNT as u32)
        {
            continue;
        }
        let Some(unit_id) = read_u32(bytes, offset + 4) else {
            continue;
        };
        let Some(party_index) = unit_id
            .checked_sub(ACTIVE_PARTY_UNIT_BASE)
            .filter(|index| *index < PARTY_SIZE as u32)
        else {
            continue;
        };

        let values = (0..SAVED_NODE_COUNT)
            .filter_map(|index| read_u32(bytes, offset + UNIT_HEADER_SIZE + index * 4))
            .filter(|value| *value != 0 && *value != INVALID_ID)
            .collect();
        builds[party_index as usize] = values;
    }
    builds
}

fn parse_active_weapons(bytes: &[u8]) -> [Option<WeaponInfo>; PARTY_SIZE] {
    let values = parse_single_value_units(bytes);
    array::from_fn(|party_index| {
        let party_unit = ACTIVE_PARTY_UNIT_BASE + party_index as u32;
        let weapon_slot = *values.get(&(PARTY_WEAPON_SLOT_ID_TYPE, party_unit))?;
        let weapon_unit = values.iter().find_map(|(&(id_type, unit_id), &value)| {
            (id_type == WEAPON_SLOT_ID_TYPE && value == weapon_slot).then_some(unit_id)
        })?;
        if !(WEAPON_UNIT_MIN..=WEAPON_UNIT_MAX).contains(&weapon_unit) {
            return None;
        }

        let weapon_id = *values.get(&(WEAPON_ID_TYPE, weapon_unit))?;
        if weapon_id == 0 || weapon_id == INVALID_ID {
            return None;
        }

        let validated = |id_type, maximum| {
            values
                .get(&(id_type, weapon_unit))
                .copied()
                .filter(|value| *value <= maximum)
                .unwrap_or_default()
        };
        let wrightstone_id = values
            .get(&(WEAPON_WRIGHTSTONE_ID_TYPE, weapon_unit))
            .copied()
            .filter(|value| *value != 0)
            .unwrap_or(INVALID_ID);
        let weapon_experience = values
            .get(&(WEAPON_EXPERIENCE_ID_TYPE, weapon_unit))
            .copied()
            .unwrap_or_default();

        Some(WeaponInfo {
            weapon_id,
            star_level: validated(WEAPON_STAR_LEVEL_ID_TYPE, 6),
            plus_marks: validated(WEAPON_PLUS_MARKS_ID_TYPE, 99),
            awakening_level: validated(WEAPON_AWAKENING_LEVEL_ID_TYPE, 10),
            trait_1_id: INVALID_ID,
            trait_1_level: 0,
            trait_2_id: INVALID_ID,
            trait_2_level: 0,
            trait_3_id: INVALID_ID,
            trait_3_level: 0,
            wrightstone_id,
            weapon_level: weapon_level_from_experience(weapon_experience),
            weapon_hp: 0,
            weapon_attack: 0,
        })
    })
}

fn parse_active_sigils(bytes: &[u8]) -> [Vec<Sigil>; PARTY_SIZE] {
    let mut lists: [Vec<u32>; PARTY_SIZE] = array::from_fn(|_| Vec::new());
    let mut fields: HashMap<(u32, u32), u32> = HashMap::new();
    if bytes.len() < UNIT_HEADER_SIZE + 4 {
        return array::from_fn(|_| Vec::new());
    }

    for offset in (0..=bytes.len() - UNIT_HEADER_SIZE - 4).step_by(4) {
        if read_u32(bytes, offset + 8) != Some(4) {
            continue;
        }
        let Some(id_type) = read_u32(bytes, offset) else {
            continue;
        };
        let Some(unit_id) = read_u32(bytes, offset + 4) else {
            continue;
        };
        let Some(count) = read_u32(bytes, offset + 12).map(|count| count as usize) else {
            continue;
        };

        if id_type == PARTY_SIGIL_LIST_ID_TYPE {
            let Some(party_index) = unit_id
                .checked_sub(ACTIVE_PARTY_UNIT_BASE)
                .filter(|index| *index < PARTY_SIZE as u32)
            else {
                continue;
            };
            if count < SIGIL_SLOT_COUNT
                || offset + UNIT_HEADER_SIZE + count * std::mem::size_of::<u32>() > bytes.len()
            {
                continue;
            }
            lists[party_index as usize] = (0..SIGIL_SLOT_COUNT)
                .filter_map(|index| read_u32(bytes, offset + UNIT_HEADER_SIZE + index * 4))
                .collect();
        } else if count == 1
            && matches!(
                id_type,
                SIGIL_ACQUISITION_ID_TYPE
                    | SIGIL_ID_TYPE
                    | SIGIL_LEVEL_ID_TYPE
                    | SIGIL_EQUIPPED_CHARACTER_ID_TYPE
                    | SIGIL_NOTIFICATION_ID_TYPE
            )
        {
            if let Some(value) = read_u32(bytes, offset + UNIT_HEADER_SIZE) {
                fields.insert((id_type, unit_id), value);
            }
        }
    }

    let inventory_by_acquisition: HashMap<u32, u32> = fields
        .iter()
        .filter_map(|(&(id_type, unit_id), &acquisition_id)| {
            (id_type == SIGIL_ACQUISITION_ID_TYPE
                && acquisition_id != 0
                && acquisition_id != INVALID_ID)
                .then_some((acquisition_id, unit_id))
        })
        .collect();

    array::from_fn(|party_index| {
        if lists[party_index].len() != SIGIL_SLOT_COUNT {
            return Vec::new();
        }

        lists[party_index]
            .iter()
            .map(|&acquisition_id| {
                let Some(&unit_id) = inventory_by_acquisition.get(&acquisition_id) else {
                    return empty_sigil();
                };
                let sigil_id = fields
                    .get(&(SIGIL_ID_TYPE, unit_id))
                    .copied()
                    .filter(|value| *value != 0 && *value != INVALID_ID)
                    .unwrap_or(INVALID_ID);
                if sigil_id == INVALID_ID {
                    return empty_sigil();
                }

                let sigil_level = fields
                    .get(&(SIGIL_LEVEL_ID_TYPE, unit_id))
                    .copied()
                    .filter(|level| (1..=15).contains(level))
                    .unwrap_or_default();
                let (first_trait_id, second_trait_id) = sigil_traits()
                    .get(&sigil_id)
                    .copied()
                    .unwrap_or((INVALID_ID, INVALID_ID));

                Sigil {
                    first_trait_id,
                    first_trait_level: (first_trait_id != INVALID_ID)
                        .then_some(sigil_level)
                        .unwrap_or_default(),
                    second_trait_id,
                    second_trait_level: (second_trait_id != INVALID_ID)
                        .then_some(sigil_level)
                        .unwrap_or_default(),
                    sigil_id,
                    equipped_character: fields
                        .get(&(SIGIL_EQUIPPED_CHARACTER_ID_TYPE, unit_id))
                        .copied()
                        .unwrap_or(INVALID_ID),
                    sigil_level,
                    acquisition_count: acquisition_id,
                    notification_enum: fields
                        .get(&(SIGIL_NOTIFICATION_ID_TYPE, unit_id))
                        .copied()
                        .unwrap_or_default(),
                }
            })
            .collect()
    })
}

fn empty_sigil() -> Sigil {
    Sigil {
        first_trait_id: INVALID_ID,
        first_trait_level: 0,
        second_trait_id: INVALID_ID,
        second_trait_level: 0,
        sigil_id: INVALID_ID,
        equipped_character: INVALID_ID,
        sigil_level: 0,
        acquisition_count: 0,
        notification_enum: 0,
    }
}

fn parse_single_value_units(bytes: &[u8]) -> HashMap<(u32, u32), u32> {
    let mut values = HashMap::new();
    if bytes.len() < UNIT_HEADER_SIZE + 4 {
        return values;
    }
    for offset in (0..=bytes.len() - UNIT_HEADER_SIZE - 4).step_by(4) {
        if read_u32(bytes, offset + 8) != Some(4) || read_u32(bytes, offset + 12) != Some(1) {
            continue;
        }
        let Some(id_type) = read_u32(bytes, offset) else {
            continue;
        };
        if !matches!(
            id_type,
            PARTY_WEAPON_SLOT_ID_TYPE
                | WEAPON_SLOT_ID_TYPE
                | WEAPON_ID_TYPE
                | WEAPON_EXPERIENCE_ID_TYPE
                | WEAPON_STAR_LEVEL_ID_TYPE
                | WEAPON_PLUS_MARKS_ID_TYPE
                | WEAPON_AWAKENING_LEVEL_ID_TYPE
                | WEAPON_WRIGHTSTONE_ID_TYPE
        ) {
            continue;
        }
        if let (Some(unit_id), Some(value)) = (
            read_u32(bytes, offset + 4),
            read_u32(bytes, offset + UNIT_HEADER_SIZE),
        ) {
            values.insert((id_type, unit_id), value);
        }
    }
    values
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        parse_active_builds, parse_active_sigils, parse_active_weapons,
        weapon_level_from_experience, ACTIVE_PARTY_UNIT_BASE, INVALID_ID,
    };

    fn write_unit(bytes: &mut [u8], offset: usize, id_type: u32, unit_id: u32, value: u32) {
        for (at, entry) in [
            (offset, id_type),
            (offset + 4, unit_id),
            (offset + 8, 4),
            (offset + 12, 1),
            (offset + 16, value),
        ] {
            bytes[at..at + 4].copy_from_slice(&entry.to_le_bytes());
        }
    }

    fn write_vector_unit(
        bytes: &mut [u8],
        offset: usize,
        id_type: u32,
        unit_id: u32,
        values: &[u32],
    ) {
        for (at, entry) in [
            (offset, id_type),
            (offset + 4, unit_id),
            (offset + 8, 4),
            (offset + 12, values.len() as u32),
        ] {
            bytes[at..at + 4].copy_from_slice(&entry.to_le_bytes());
        }
        for (index, value) in values.iter().enumerate() {
            let at = offset + 16 + index * 4;
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    #[test]
    fn parses_only_active_party_master_trait_units() {
        let mut bytes = vec![0u8; 0x400];
        let offset = 0x80;
        for (at, value) in [
            (offset, 3007),
            (offset + 4, ACTIVE_PARTY_UNIT_BASE + 2),
            (offset + 8, 4),
            (offset + 12, 50),
            (offset + 16, 0x1234_5678),
            (offset + 20, INVALID_ID),
            (offset + 24, 0xAABB_CCDD),
        ] {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        let builds = parse_active_builds(&bytes);
        assert!(builds[0].is_empty());
        assert!(builds[1].is_empty());
        assert_eq!(builds[2], vec![0x1234_5678, 0xAABB_CCDD]);
        assert!(builds[3].is_empty());
    }

    #[test]
    fn ignores_preset_units_with_the_same_value_shape() {
        let mut bytes = vec![0u8; 0x200];
        for (at, value) in [(0x40, 3007u32), (0x44, 20_195), (0x48, 4), (0x4C, 50)] {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[0x50..0x54].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert!(parse_active_builds(&bytes).iter().all(Vec::is_empty));
    }

    #[test]
    fn resolves_active_party_weapon_through_its_inventory_slot() {
        let mut bytes = vec![0u8; 0x800];
        write_unit(&mut bytes, 0x040, 1402, ACTIVE_PARTY_UNIT_BASE + 1, 69);
        write_unit(&mut bytes, 0x100, 2802, 40_066, 69);
        write_unit(&mut bytes, 0x160, 2803, 40_066, 0x1CC9_0CAE);
        write_unit(&mut bytes, 0x1A0, 2804, 40_066, 162_540);
        write_unit(&mut bytes, 0x1C0, 2805, 40_066, 6);
        write_unit(&mut bytes, 0x220, 2806, 40_066, 99);
        write_unit(&mut bytes, 0x280, 2807, 40_066, 2);
        write_unit(&mut bytes, 0x340, 2816, 40_066, 0x0BD3_73A4);

        let weapons = parse_active_weapons(&bytes);
        assert!(weapons[0].is_none());
        let weapon = weapons[1].as_ref().expect("party 2 weapon");
        assert_eq!(weapon.weapon_id, 0x1CC9_0CAE);
        assert_eq!(weapon.star_level, 6);
        assert_eq!(weapon.plus_marks, 99);
        assert_eq!(weapon.awakening_level, 2);
        assert_eq!(weapon.weapon_level, 150);
        assert_eq!(weapon.wrightstone_id, 0x0BD3_73A4);
        assert!(weapons[2].is_none());
        assert!(weapons[3].is_none());
    }

    #[test]
    fn converts_saved_weapon_experience_to_level() {
        assert_eq!(weapon_level_from_experience(0), 1);
        assert_eq!(weapon_level_from_experience(9), 1);
        assert_eq!(weapon_level_from_experience(10), 2);
        assert_eq!(weapon_level_from_experience(162_539), 149);
        assert_eq!(weapon_level_from_experience(162_540), 150);
    }

    #[test]
    fn resolves_exactly_twelve_active_sigils_through_acquisition_ids() {
        let mut bytes = vec![0u8; 0x1000];
        let mut slots = vec![0u32; 13];
        slots[0] = 42;
        slots[2] = 43;
        write_vector_unit(&mut bytes, 0x040, 1403, ACTIVE_PARTY_UNIT_BASE, &slots);

        let inventory_unit = 30_001;
        write_unit(&mut bytes, 0x100, 2702, inventory_unit, 42);
        write_unit(&mut bytes, 0x160, 2703, inventory_unit, 858_692_400);
        write_unit(&mut bytes, 0x1C0, 2704, inventory_unit, 15);
        write_unit(&mut bytes, 0x220, 2706, inventory_unit, 1_652_280_077);
        write_unit(&mut bytes, 0x280, 2707, inventory_unit, 2);

        let sigils = parse_active_sigils(&bytes);
        assert_eq!(sigils[0].len(), 12);
        assert_eq!(sigils[0][0].sigil_id, 858_692_400);
        assert_eq!(sigils[0][0].first_trait_id, 4_001_746_207);
        assert_eq!(sigils[0][0].first_trait_level, 15);
        assert_eq!(sigils[0][0].second_trait_id, INVALID_ID);
        assert_eq!(sigils[0][0].acquisition_count, 42);
        assert_eq!(sigils[0][0].equipped_character, 1_652_280_077);
        assert_eq!(sigils[0][0].notification_enum, 2);
        assert_eq!(sigils[0][1].sigil_id, INVALID_ID);
        assert_eq!(sigils[0][2].sigil_id, INVALID_ID);
        assert!(sigils[1].is_empty());
        assert!(sigils[2].is_empty());
        assert!(sigils[3].is_empty());
    }

    #[test]
    fn bundled_traits_cover_generic_and_concrete_game_2_sigils() {
        let traits = super::sigil_traits();
        assert_eq!(
            traits.get(&3_760_801_040),
            Some(&(1_470_847_760, 3_769_368_062))
        );
        assert_eq!(
            traits.get(&1_862_062_726),
            Some(&(1_280_871_463, 1_072_455_552))
        );
        assert_eq!(
            traits.get(&1_225_749_252),
            Some(&(612_907_763, 3_468_099_822))
        );
        assert_eq!(
            traits.get(&3_634_652_401),
            Some(&(2_069_563_421, 2_597_196_811))
        );
        assert_eq!(traits.get(&3_896_853_593), Some(&(801_700_863, INVALID_ID)));
    }
}
