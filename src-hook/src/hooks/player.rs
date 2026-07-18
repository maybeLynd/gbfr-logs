use std::{
    collections::{HashMap, HashSet},
    ffi::{c_void, CString},
    sync::{Mutex, OnceLock},
};

use anyhow::{anyhow, Result};
use log::info;
use protocol::{
    Overmastery, OvermasteryInfo, PlayerEquipmentEvent, PlayerIdentityEvent, PlayerStats, Sigil,
    WeaponInfo,
};
use retour::static_detour;
use windows::Win32::{Foundation::HANDLE, System::Diagnostics::Debug::ReadProcessMemory};

use crate::{event, process::Process};

type RefreshPlayerIdentityFunc = unsafe extern "system" fn(*const usize);

static_detour! {
    static RefreshPlayerIdentity: unsafe extern "system" fn(*const usize);
}

const PLAYER_IDENTITY_OFFSET: usize = 0x5E60;
const PLAYER_KEY_OFFSET: usize = 0x5EA8;
const PLAYER_STATE_OFFSET: usize = 0x5EAC;
const CHARACTER_NAME_OFFSET: usize = 0x1E8;
const DISPLAY_NAME_OFFSET: usize = 0x208;
const PARTY_INDEX_OFFSET: usize = 0x22C;
const SIGIL_COUNT: usize = 12;
const SIGIL_SIZE: usize = 0x24;
const VBUFFER_INLINE_CAPACITY: usize = 0x0F;
const MAX_PLAYER_NAME_BYTES: usize = 0x100;
const INVALID_PLAYER_KEY: u32 = 0x887A_E0B0;
const ACTOR_PARTY_INDEX_OFFSET: usize = 0x24;
const ACTOR_AI_CONTROLLED_OFFSET: usize = 0x774;
const ACTOR_PLAYER_STATS_OFFSET: usize = 0x15030;
const ACTOR_WEAPON_INFO_OFFSET: usize = 0x15080;
const ACTOR_OVERMASTERY_OFFSET: usize = 0x1A8E8;
const ACTOR_MASTER_TRAIT_SCAN_SIZE: usize = 0x1C000;
const ACTOR_PLAYER_KEY_OFFSET: usize = 0x1AB40;
const REFRESH_PLAYER_IDENTITY_SIG: &str =
    "55 41 57 41 56 41 54 56 57 53 48 83 ec 70 48 8d 6c 24 70 48 c7 45 f8 fe ff ff ff 80 b9 bc 5e 00 00 00";

const MASTER_TRAIT_EFFECTS: &[(&str, &[(u32, u32)])] = include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/master-trait-effects.rs"
));

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredPlayerIdentity {
    character_name: CString,
    display_name: CString,
    party_index: u8,
    is_online: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct StoredPlayerEquipment {
    party_index: u8,
    sigils: Vec<Sigil>,
}

#[derive(Default)]
struct IdentityStore {
    by_key: HashMap<u32, StoredPlayerIdentity>,
    active_key_by_party: HashMap<u8, u32>,
    local_player_key: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct IdentityInsertOutcome {
    mapping_changed: bool,
    session_changed: bool,
    equipment_trusted: bool,
}

impl IdentityStore {
    fn insert(
        &mut self,
        player_key: u32,
        identity: StoredPlayerIdentity,
        has_verified_loadout: bool,
    ) -> IdentityInsertOutcome {
        let party_index = identity.party_index;

        let session_changed = if party_index == 0 {
            let changed = self
                .local_player_key
                .is_some_and(|known_key| known_key != player_key);
            if changed {
                self.by_key.clear();
                self.active_key_by_party.clear();
            }
            self.local_player_key = Some(player_key);
            changed
        } else {
            false
        };

        self.active_key_by_party.retain(|known_party, known_key| {
            *known_party == party_index || *known_key != player_key
        });
        let previous_key = self.active_key_by_party.insert(party_index, player_key);
        let identity_changed = self.by_key.get(&player_key) != Some(&identity);
        self.by_key.insert(player_key, identity);

        IdentityInsertOutcome {
            mapping_changed: session_changed
                || identity_changed
                || previous_key != Some(player_key),
            session_changed,
            equipment_trusted: has_verified_loadout,
        }
    }
}

static IDENTITIES: OnceLock<Mutex<IdentityStore>> = OnceLock::new();
static EQUIPMENT: OnceLock<Mutex<HashMap<u32, StoredPlayerEquipment>>> = OnceLock::new();
static ACTOR_KEYS: OnceLock<Mutex<HashSet<(usize, u32, u32, u8)>>> = OnceLock::new();
static EQUIPMENT_ACTORS: OnceLock<Mutex<HashSet<(usize, u32, u8)>>> = OnceLock::new();

#[derive(Clone)]
pub struct OnLoadPlayerIdentityHook {
    #[allow(dead_code)]
    tx: event::Tx,
}

impl OnLoadPlayerIdentityHook {
    pub fn new(tx: event::Tx) -> Self {
        Self { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let refresh_player_identity = process
            .search_match_address(REFRESH_PLAYER_IDENTITY_SIG)
            .map_err(|_| anyhow!("Could not find refresh_player_identity"))?;
        let cloned_self = self.clone();

        unsafe {
            let func: RefreshPlayerIdentityFunc = std::mem::transmute(refresh_player_identity);
            RefreshPlayerIdentity.initialize(func, move |record| cloned_self.run(record))?;
            RefreshPlayerIdentity.enable()?;
        }

        Ok(())
    }

    fn run(&self, record: *const usize) {
        unsafe { RefreshPlayerIdentity.call(record) };

        if record.is_null() {
            return;
        }

        let snapshot = unsafe {
            (record.byte_add(PLAYER_IDENTITY_OFFSET) as *const *const u8).read_unaligned()
        };
        let player_key = unsafe {
            record
                .byte_add(PLAYER_KEY_OFFSET)
                .cast::<u32>()
                .read_unaligned()
        };

        if player_key == 0 || player_key == INVALID_PLAYER_KEY {
            return;
        }

        let player_state = unsafe {
            record
                .byte_add(PLAYER_STATE_OFFSET)
                .cast::<u32>()
                .read_unaligned()
        };
        let Some(is_online) = is_online_state(player_state) else {
            return;
        };

        let Some(identity) = (unsafe { read_player_identity(snapshot, is_online) }) else {
            return;
        };

        if !should_cache_identity(&identity) {
            return;
        }

        let identity_for_equipment = identity.clone();
        let sigils = unsafe { read_sigils(snapshot) }.filter(|sigils| !sigils.is_empty());
        let outcome = {
            let mut identities = IDENTITIES
                .get_or_init(|| Mutex::new(IdentityStore::default()))
                .lock()
                .expect("player identity map lock poisoned");
            identities.insert(player_key, identity, sigils.is_some())
        };

        info!(
            "Player identity cached: key={player_key:#010x}, party={}, online={}, name={}",
            identity_for_equipment.party_index,
            identity_for_equipment.is_online,
            identity_for_equipment.display_name.to_string_lossy()
        );

        let mut equipment = EQUIPMENT
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("player equipment map lock poisoned");
        if outcome.session_changed {
            equipment.clear();
        }
        if outcome.equipment_trusted {
            if let Some(sigils) = sigils {
                let previous = equipment.get(&player_key).cloned();
                let updated = StoredPlayerEquipment {
                    party_index: identity_for_equipment.party_index,
                    sigils,
                };
                if previous.as_ref() != Some(&updated) {
                    equipment.insert(player_key, updated);
                    clear_equipment_actor_cache_for_key(player_key);
                }
            }
        }
        drop(equipment);

        if outcome.mapping_changed {
            clear_actor_identity_caches();
        }
    }
}

fn clear_actor_identity_caches() {
    ACTOR_KEYS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("actor identity map lock poisoned")
        .clear();
    EQUIPMENT_ACTORS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("equipment actor set lock poisoned")
        .clear();
}

fn clear_equipment_actor_cache_for_key(player_key: u32) {
    EQUIPMENT_ACTORS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("equipment actor set lock poisoned")
        .retain(|(_, known_key, _)| *known_key != player_key);
}

fn is_online_state(state: u32) -> Option<bool> {
    match state {
        0..=3 => Some(false),
        4 | 5 => Some(true),
        _ => None,
    }
}

fn should_cache_identity(identity: &StoredPlayerIdentity) -> bool {
    identity.party_index == 0 || identity.is_online
}

fn damage_cap_enabled(party_index: Option<u8>, is_ai_controlled: Option<bool>) -> bool {
    party_index
        .zip(is_ai_controlled)
        .is_some_and(|(party_index, is_ai_controlled)| party_index == 0 || is_ai_controlled)
}

pub fn damage_cap_enabled_for_actor(actor: *const usize) -> bool {
    if actor.is_null() {
        return false;
    }

    damage_cap_enabled(read_actor_party_index(actor), is_ai_controlled_actor(actor))
}

pub fn identity_event_for_actor(
    actor: *const usize,
    character_type: u32,
    actor_index: u32,
) -> Option<PlayerIdentityEvent> {
    if actor.is_null() {
        return None;
    }

    let actor_address = actor as usize;
    let party_index = read_actor_party_index(actor)?;
    if is_ai_controlled_actor(actor)? {
        return None;
    }
    let player_key = read_actor_player_key(actor)?;
    let marker = (actor_address, player_key, character_type, party_index);
    if ACTOR_KEYS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("actor identity map lock poisoned")
        .contains(&marker)
    {
        return None;
    }

    let (identity_key, identity) = resolve_identity(
        &IDENTITIES
            .get_or_init(|| Mutex::new(IdentityStore::default()))
            .lock()
            .expect("player identity map lock poisoned"),
        player_key,
        party_index,
    )?;

    info!(
        "Player actor matched: actor={actor:p}, type={character_type:#010x}, party={party_index}, actor_key={player_key:#010x}, identity_key={identity_key:#010x}, offset={ACTOR_PLAYER_KEY_OFFSET:#x}, name={}",
        identity.display_name.to_string_lossy()
    );

    ACTOR_KEYS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("actor identity map lock poisoned")
        .insert(marker);

    Some(PlayerIdentityEvent {
        character_name: identity.character_name,
        display_name: identity.display_name,
        character_type,
        party_index,
        actor_index,
        is_online: identity.is_online,
    })
}

pub fn equipment_event_for_actor(
    actor: *const usize,
    character_type: u32,
    actor_index: u32,
) -> Option<PlayerEquipmentEvent> {
    if actor.is_null() {
        return None;
    }

    let actor_address = actor as usize;
    let party_index = read_actor_party_index(actor)?;
    let is_ai_controlled = is_ai_controlled_actor(actor)?;
    let player_key = read_actor_player_key(actor)?;
    let marker = (actor_address, player_key, party_index);
    if EQUIPMENT_ACTORS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("equipment actor set lock poisoned")
        .contains(&marker)
    {
        return None;
    }

    let equipment = EQUIPMENT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("player equipment map lock poisoned")
        .get(&player_key)
        .filter(|equipment| equipment.party_index == party_index)
        .cloned();
    let sigils = equipment
        .as_ref()
        .map(|equipment| equipment.sigils.clone())
        .unwrap_or_default();
    let weapon_info = read_actor_weapon_info(actor);
    let overmastery_info = read_actor_overmasteries(actor);
    let player_stats = read_actor_player_stats(actor);
    let master_traits = read_actor_master_traits(actor, character_type);

    if sigils.is_empty()
        && weapon_info.is_none()
        && overmastery_info.is_none()
        && player_stats.is_none()
        && master_traits.is_none()
    {
        return None;
    }

    EQUIPMENT_ACTORS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("equipment actor set lock poisoned")
        .insert(marker);

    info!(
        "Player equipment recovered: actor={actor:p}, type={character_type:#010x}, party={party_index}, online={}, sigils={}, weapon={}, overmasteries={}, stats={}, master_traits={}",
        !is_ai_controlled && party_index != 0,
        sigils.len(),
        weapon_info.is_some(),
        overmastery_info
            .as_ref()
            .map_or(0, |info| info.overmasteries.len()),
        player_stats.is_some(),
        master_traits
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |nodes| nodes.len().to_string())
    );

    Some(PlayerEquipmentEvent {
        sigils,
        weapon_info,
        overmastery_info,
        player_stats,
        master_traits,
        character_type,
        party_index,
        actor_index,
        is_online: !is_ai_controlled && party_index != 0,
    })
}

fn resolve_identity(
    identities: &IdentityStore,
    player_key: u32,
    party_index: u8,
) -> Option<(u32, StoredPlayerIdentity)> {
    let identity = identities
        .by_key
        .get(&player_key)
        .filter(|identity| identity.party_index == party_index)?;

    Some((player_key, identity.clone()))
}

fn character_key_for_type(character_type: u32) -> Option<&'static str> {
    Some(match character_type {
        0x26A4848A => "Pl0000",
        0x9498420D => "Pl0100",
        0x34D4FD8F => "Pl0200",
        0xF8D73D33 => "Pl0300",
        0x7B5934AD => "Pl0400",
        0x443D46BB => "Pl0500",
        0xA9D6569E => "Pl0600",
        0xFBA6615D => "Pl0700",
        0x63A7C3F0 => "Pl0800",
        0xF96A90C2 => "Pl0900",
        0x28AC1108 => "Pl1000",
        0x94E2514E => "Pl1100",
        0x2B4AA114 => "Pl1200",
        0xC97F3365 => "Pl1300",
        0x601AA977 => "Pl1400",
        0xBCC238DE => "Pl1500",
        0xC3155079 => "Pl1600",
        0xD16CFBDE => "Pl1700",
        0x6FDD6932 => "Pl1800",
        0x8056ABCD | 0xF5755C0E => "Pl1900",
        0x9C89A455 => "Pl2100",
        0x59DB0CD9 => "Pl2200",
        0xDA5A8E25 => "Pl2300",
        0x4C714F77 => "Pl2400",
        0xE330418F => "Pl2500",
        0xE3D1BE26 => "Pl2600",
        0x91418145 => "Pl2700",
        0x48ADDA36 => "Pl2800",
        0x0A58FB4D => "Pl2900",
        _ => return None,
    })
}

fn read_actor_player_key(actor: *const usize) -> Option<u32> {
    let player_key = read_actor_u32(actor, ACTOR_PLAYER_KEY_OFFSET)?;
    if player_key == 0 || player_key == INVALID_PLAYER_KEY {
        return None;
    }
    Some(player_key)
}

fn read_actor_party_index(actor: *const usize) -> Option<u8> {
    let party_index = read_actor_u32(actor, ACTOR_PARTY_INDEX_OFFSET)?;
    (party_index <= 3).then_some(party_index as u8)
}

fn is_ai_controlled_actor(actor: *const usize) -> Option<bool> {
    match read_actor_u32(actor, ACTOR_AI_CONTROLLED_OFFSET)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn read_actor_player_stats(actor: *const usize) -> Option<PlayerStats> {
    let at = |offset| read_actor_u32(actor, ACTOR_PLAYER_STATS_OFFSET + offset);
    let stats = PlayerStats {
        level: at(0x00)?,
        total_hp: at(0x04)?,
        total_attack: at(0x08)?,
        stun_power: f32::from_bits(at(0x10)?),
        critical_rate: f32::from_bits(at(0x14)?),
        total_power: at(0x18)?,
    };
    ((1..=100).contains(&stats.level)
        && stats.total_hp > 0
        && stats.total_attack > 0
        && stats.stun_power.is_finite()
        && (0.0..=1_000_000.0).contains(&stats.stun_power)
        && stats.critical_rate.is_finite()
        && (0.0..=10_000.0).contains(&stats.critical_rate)
        && stats.total_power > 0)
        .then_some(stats)
}

fn read_actor_weapon_info(actor: *const usize) -> Option<WeaponInfo> {
    let at = |offset| read_actor_u32(actor, ACTOR_WEAPON_INFO_OFFSET + offset);
    let weapon = WeaponInfo {
        weapon_id: at(0x04)?,
        star_level: at(0x14)?,
        plus_marks: at(0x18)?,
        awakening_level: at(0x1C)?,
        trait_1_id: at(0x20)?,
        trait_1_level: at(0x24)?,
        trait_2_id: at(0x28)?,
        trait_2_level: at(0x2C)?,
        trait_3_id: at(0x30)?,
        trait_3_level: at(0x34)?,
        wrightstone_id: at(0x38)?,
        weapon_level: at(0x58)?,
        weapon_hp: at(0x5C)?,
        weapon_attack: at(0x60)?,
    };
    (weapon.weapon_id != 0
        && weapon.weapon_id != INVALID_PLAYER_KEY
        && weapon.star_level <= 6
        && weapon.plus_marks <= 99
        && weapon.awakening_level <= 10
        && (1..=150).contains(&weapon.weapon_level)
        && weapon.weapon_hp > 0
        && weapon.weapon_attack > 0)
        .then_some(weapon)
}

fn read_actor_overmasteries(actor: *const usize) -> Option<OvermasteryInfo> {
    let mut overmasteries = Vec::with_capacity(4);
    for index in 0..4 {
        let base = ACTOR_OVERMASTERY_OFFSET + index * 0x10;
        let id = read_actor_u32(actor, base)?;
        let flags = read_actor_u32(actor, base + 0x04)?;
        let parameter_type = read_actor_u32(actor, base + 0x08)?;
        let value = f32::from_bits(read_actor_u32(actor, base + 0x0C)?);
        if id == 0 || id == INVALID_PLAYER_KEY {
            continue;
        }
        if flags > 0xFFFF || parameter_type > 0x100 || !value.is_finite() {
            return None;
        }
        overmasteries.push(Overmastery { id, flags, value });
    }
    (!overmasteries.is_empty()).then_some(OvermasteryInfo { overmasteries })
}

fn read_actor_master_traits(actor: *const usize, character_type: u32) -> Option<Vec<u32>> {
    let character = character_key_for_type(character_type)?;
    let effects = MASTER_TRAIT_EFFECTS
        .iter()
        .find_map(|(key, effects)| (*key == character).then_some(*effects))?;
    let data = read_actor_bytes(actor, ACTOR_MASTER_TRAIT_SCAN_SIZE)?;
    Some(selected_master_trait_nodes(&data, effects))
}

fn selected_master_trait_nodes(data: &[u8], effects: &[(u32, u32)]) -> Vec<u32> {
    let effect_to_node: HashMap<u32, u32> = effects
        .iter()
        .map(|(node_id, effect_id)| (*effect_id, *node_id))
        .collect();
    let mut selected = HashSet::new();
    let following = data.get(4..).unwrap_or_default();
    for (effect, flag) in data.chunks_exact(4).zip(following.chunks_exact(4)) {
        let effect_id = u32::from_le_bytes(effect.try_into().expect("four-byte chunk"));
        let active = u32::from_le_bytes(flag.try_into().expect("four-byte chunk"));
        if active != 0 && active != INVALID_PLAYER_KEY {
            if let Some(node_id) = effect_to_node.get(&effect_id) {
                selected.insert(*node_id);
            }
        }
    }
    let mut selected: Vec<u32> = selected.into_iter().collect();
    selected.sort_unstable();
    selected
}

fn read_actor_bytes(actor: *const usize, size: usize) -> Option<Vec<u8>> {
    let mut data = vec![0u8; size];
    let mut bytes_read = 0usize;
    let result = unsafe {
        ReadProcessMemory(
            HANDLE(-1),
            actor.cast::<c_void>(),
            data.as_mut_ptr().cast::<c_void>(),
            size,
            Some(&mut bytes_read),
        )
    };
    if result.is_err() || bytes_read != size {
        return None;
    }
    Some(data)
}

fn read_actor_u32(actor: *const usize, offset: usize) -> Option<u32> {
    let mut value = 0u32;
    let mut bytes_read = 0usize;
    let result = unsafe {
        ReadProcessMemory(
            HANDLE(-1),
            actor.byte_add(offset).cast::<c_void>(),
            (&mut value as *mut u32).cast::<c_void>(),
            std::mem::size_of::<u32>(),
            Some(&mut bytes_read),
        )
    };

    if result.is_err() || bytes_read != std::mem::size_of::<u32>() {
        return None;
    }
    Some(value)
}

unsafe fn read_player_identity(
    snapshot: *const u8,
    is_online: bool,
) -> Option<StoredPlayerIdentity> {
    if snapshot.is_null() {
        return None;
    }

    let party_index = read_party_index(snapshot)?;

    let display_name = read_vbuffer(snapshot.byte_add(DISPLAY_NAME_OFFSET))?;

    if display_name.as_bytes().is_empty() {
        return None;
    }

    let character_name = read_vbuffer(snapshot.byte_add(CHARACTER_NAME_OFFSET))
        .unwrap_or_else(|| CString::new("").expect("empty CString is valid"));

    Some(StoredPlayerIdentity {
        character_name,
        display_name,
        party_index,
        is_online,
    })
}

unsafe fn read_party_index(snapshot: *const u8) -> Option<u8> {
    if snapshot.is_null() {
        return None;
    }
    let party_index = snapshot
        .byte_add(PARTY_INDEX_OFFSET)
        .cast::<u32>()
        .read_unaligned();
    (party_index <= 3).then_some(party_index as u8)
}

unsafe fn read_sigils(snapshot: *const u8) -> Option<Vec<Sigil>> {
    if snapshot.is_null() {
        return None;
    }

    let mut sigils = Vec::with_capacity(SIGIL_COUNT);
    for index in 0..SIGIL_COUNT {
        let values = snapshot
            .byte_add(index * SIGIL_SIZE)
            .cast::<[u32; 9]>()
            .read_unaligned();
        if values[4] == 0 || values[4] == INVALID_PLAYER_KEY {
            continue;
        }
        if !valid_sigil_values(&values) {
            continue;
        }
        sigils.push(Sigil {
            first_trait_id: values[0],
            first_trait_level: values[1],
            second_trait_id: values[2],
            second_trait_level: values[3],
            sigil_id: values[4],
            equipped_character: values[5],
            sigil_level: values[6],
            acquisition_count: values[7],
            notification_enum: values[8],
        });
    }
    Some(sigils)
}

fn valid_sigil_values(values: &[u32; 9]) -> bool {
    (1..=15).contains(&values[6]) && values[7] <= 1_000_000 && values[8] <= 2
}

unsafe fn read_vbuffer(buffer: *const u8) -> Option<CString> {
    let used_size = buffer.byte_add(0x10).cast::<usize>().read_unaligned();
    let max_size = buffer.byte_add(0x18).cast::<usize>().read_unaligned();

    if used_size > MAX_PLAYER_NAME_BYTES || max_size < used_size || max_size > 0x1000 {
        return None;
    }

    let bytes_ptr = if max_size > VBUFFER_INLINE_CAPACITY {
        buffer.cast::<*const u8>().read_unaligned()
    } else {
        buffer
    };

    if bytes_ptr.is_null() {
        return None;
    }

    let bytes = std::slice::from_raw_parts(bytes_ptr, used_size);
    std::str::from_utf8(bytes).ok()?;
    CString::new(bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::{
        damage_cap_enabled, read_vbuffer, resolve_identity, selected_master_trait_nodes,
        should_cache_identity, valid_sigil_values, IdentityStore, StoredPlayerIdentity,
        ACTOR_AI_CONTROLLED_OFFSET, ACTOR_OVERMASTERY_OFFSET, ACTOR_PARTY_INDEX_OFFSET,
        ACTOR_PLAYER_KEY_OFFSET, ACTOR_PLAYER_STATS_OFFSET, ACTOR_WEAPON_INFO_OFFSET,
        INVALID_PLAYER_KEY,
    };

    fn identity(
        character_name: &str,
        display_name: &str,
        party_index: u8,
        is_online: bool,
    ) -> StoredPlayerIdentity {
        StoredPlayerIdentity {
            character_name: CString::new(character_name).unwrap(),
            display_name: CString::new(display_name).unwrap(),
            party_index,
            is_online,
        }
    }

    #[test]
    fn reads_inline_utf8_player_name() {
        let mut buffer = [0u8; 0x20];
        let name = "芙劳玩家".as_bytes();
        buffer[..name.len()].copy_from_slice(name);
        buffer[0x10..0x18].copy_from_slice(&name.len().to_ne_bytes());
        buffer[0x18..0x20].copy_from_slice(&0x0Fusize.to_ne_bytes());

        let value = unsafe { read_vbuffer(buffer.as_ptr()) }.expect("valid VBuffer");
        assert_eq!(value.to_str().unwrap(), "芙劳玩家");
    }

    #[test]
    fn rejects_unreasonably_large_player_name() {
        let mut buffer = [0u8; 0x20];
        buffer[0x10..0x18].copy_from_slice(&0x101usize.to_ne_bytes());
        buffer[0x18..0x20].copy_from_slice(&0x101usize.to_ne_bytes());

        assert!(unsafe { read_vbuffer(buffer.as_ptr()) }.is_none());
    }

    #[test]
    fn uses_verified_actor_player_key_offset() {
        assert_eq!(ACTOR_PLAYER_KEY_OFFSET, 0x1AB40);
        assert_eq!(ACTOR_PARTY_INDEX_OFFSET, 0x24);
        assert_eq!(ACTOR_AI_CONTROLLED_OFFSET, 0x774);
        assert_eq!(ACTOR_PLAYER_STATS_OFFSET, 0x15030);
        assert_eq!(ACTOR_WEAPON_INFO_OFFSET, 0x15080);
        assert_eq!(ACTOR_OVERMASTERY_OFFSET, 0x1A8E8);
    }

    #[test]
    fn rejects_remote_offline_placeholder_identity() {
        assert!(!should_cache_identity(&identity(
            "Ferry",
            "Local Player",
            1,
            false
        )));
        assert!(should_cache_identity(&identity(
            "Ferry",
            "Local Player",
            0,
            false
        )));
        assert!(should_cache_identity(&identity(
            "Ferry",
            "Remote Player",
            1,
            true
        )));
    }

    #[test]
    fn damage_cap_is_limited_to_local_and_ai_actors() {
        assert!(damage_cap_enabled(Some(0), Some(false)));
        assert!(damage_cap_enabled(Some(0), Some(true)));
        assert!(damage_cap_enabled(Some(1), Some(true)));
        assert!(!damage_cap_enabled(Some(1), Some(false)));
        assert!(!damage_cap_enabled(None, Some(true)));
        assert!(!damage_cap_enabled(Some(0), None));
    }

    #[test]
    fn replacing_party_slot_keeps_each_identity_bound_to_its_runtime_key() {
        let mut identities = IdentityStore::default();
        assert!(
            identities
                .insert(0x1111, identity("Ferry", "Player A", 2, true), true)
                .mapping_changed
        );
        assert!(
            identities
                .insert(0x2222, identity("Ferry", "Player B", 2, true), true)
                .mapping_changed
        );

        assert_eq!(
            identities.by_key[&0x1111].display_name.to_str().unwrap(),
            "Player A"
        );
        assert_eq!(
            identities.by_key[&0x2222].display_name.to_str().unwrap(),
            "Player B"
        );
    }

    #[test]
    fn identity_does_not_require_equipment_snapshot() {
        let mut identities = IdentityStore::default();
        identities.insert(
            0x1000,
            identity("Siegfried", "Local Player", 0, false),
            true,
        );

        let initial = identities.insert(
            0x2000,
            identity("Maglielle", "Remote Player", 2, true),
            false,
        );
        assert!(initial.mapping_changed);
        assert!(!initial.equipment_trusted);
        assert!(identities.by_key.contains_key(&0x2000));

        let real_remote = identities.insert(
            0x3000,
            identity("Cagliostro", "Remote Player", 1, true),
            true,
        );
        assert!(real_remote.equipment_trusted);
    }

    #[test]
    fn missing_equipment_does_not_remove_an_identity() {
        let mut identities = IdentityStore::default();
        identities.insert(0x2000, identity("Fraux", "Remote Player", 3, true), true);

        let removed = identities.insert(0x2000, identity("Fraux", "Remote Player", 3, true), false);
        assert!(!removed.mapping_changed);
        assert!(!removed.equipment_trusted);
        assert!(identities.by_key.contains_key(&0x2000));
        assert_eq!(identities.active_key_by_party.get(&3), Some(&0x2000));
    }

    #[test]
    fn moving_a_player_key_between_slots_does_not_let_old_slot_evict_it() {
        let mut identities = IdentityStore::default();
        identities.insert(0x2000, identity("Fraux", "Remote Player", 1, true), true);
        identities.insert(0x2000, identity("Fraux", "Remote Player", 2, true), true);
        identities.insert(0x3000, identity("Fediel", "Another Player", 1, true), true);

        assert_eq!(
            identities.by_key[&0x2000].display_name.to_str().unwrap(),
            "Remote Player"
        );
        assert_eq!(identities.active_key_by_party.get(&2), Some(&0x2000));
        assert_eq!(identities.active_key_by_party.get(&1), Some(&0x3000));
    }

    #[test]
    fn resolves_only_exact_keys_and_slots_even_with_stale_character_text() {
        let mut identities = IdentityStore::default();
        identities.insert(0x1111, identity("", "Lynd", 0, false), false);

        assert!(resolve_identity(&identities, 0x1111, 1).is_none());
        assert!(resolve_identity(&identities, 0x9999, 0).is_none());
        assert_eq!(
            resolve_identity(&identities, 0x1111, 0)
                .unwrap()
                .1
                .display_name
                .to_str()
                .unwrap(),
            "Lynd"
        );
    }

    #[test]
    fn maps_four_same_character_players_only_by_their_exact_keys() {
        let mut identities = IdentityStore::default();
        identities.insert(0x1111, identity("Ferry", "Player A", 0, true), true);
        identities.insert(0x2222, identity("Ferry", "Player B", 1, true), true);
        identities.insert(0x3333, identity("Ferry", "Player C", 2, true), true);
        identities.insert(0x4444, identity("Ferry", "Player D", 3, true), true);

        assert!(resolve_identity(&identities, 0x9999, 0).is_none());
        for (key, party_index, expected_name) in [
            (0x1111, 0, "Player A"),
            (0x2222, 1, "Player B"),
            (0x3333, 2, "Player C"),
            (0x4444, 3, "Player D"),
        ] {
            assert_eq!(
                resolve_identity(&identities, key, party_index)
                    .unwrap()
                    .1
                    .display_name
                    .to_str()
                    .unwrap(),
                expected_name
            );
        }
    }

    #[test]
    fn master_traits_accept_nonzero_rank_values_and_are_deduplicated() {
        let effects = [(0x1000, 0xAAAA), (0x2000, 0xBBBB), (0x3000, 0xCCCC)];
        let mut data = Vec::new();
        for value in [
            0xAAAA,
            1,
            0xBBBB,
            0,
            0xCCCC,
            2,
            0xAAAA,
            1,
            0xBBBB,
            INVALID_PLAYER_KEY,
        ] {
            data.extend_from_slice(&u32::to_le_bytes(value));
        }

        assert_eq!(
            selected_master_trait_nodes(&data, &effects),
            vec![0x1000, 0x3000]
        );
    }

    #[test]
    fn accepts_remote_sigils_with_missing_trait_metadata() {
        assert!(valid_sigil_values(&[
            0,
            0,
            INVALID_PLAYER_KEY,
            0,
            1_513_492_136,
            0,
            15,
            42,
            2,
        ]));
        assert!(!valid_sigil_values(&[
            0,
            0,
            0,
            0,
            1_513_492_136,
            0,
            0,
            42,
            2,
        ]));
    }
}
