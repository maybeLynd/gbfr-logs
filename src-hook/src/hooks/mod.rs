use std::{
    collections::HashMap,
    ffi::c_void,
    mem::MaybeUninit,
    sync::{Mutex, OnceLock},
};

use anyhow::Result;
use log::{info, warn};
use windows::Win32::{Foundation::HANDLE, System::Diagnostics::Debug::ReadProcessMemory};

use crate::{event, process::Process};

use self::{
    damage::{OnProcessDamageHook, OnProcessDotHook},
    death::OnDeathHook,
    player::OnLoadPlayerIdentityHook,
    quest::OnBattleEndHook,
};

mod area;
mod damage;
mod death;
mod ffi;
mod globals;
mod player;
mod quest;
mod sba;

type GetEntityHashID0x58 = unsafe extern "system" fn(*const usize, *const u32) -> *const usize;

#[derive(Default)]
struct ActorIds {
    by_instance: HashMap<usize, u32>,
    next_id: u32,
}

static ACTOR_IDS: OnceLock<Mutex<ActorIds>> = OnceLock::new();

pub fn setup_hooks(tx: event::Tx) -> Result<()> {
    let process = Process::with_name("granblue_fantasy_relink.exe")?;

    OnProcessDamageHook::new(tx.clone()).setup(&process)?;

    match OnProcessDotHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Game 2.0.2 poison/burn DoT hooks enabled"),
        Err(error) => warn!("Poison/burn DoT hooks unavailable: {error}"),
    }

    match OnLoadPlayerIdentityHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Game 2.0.2 key-based player-identity hook enabled"),
        Err(error) => warn!("Player-identity hook unavailable; using character names: {error}"),
    }

    match OnBattleEndHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Game 2.0.2 result-screen battle-end hook enabled"),
        Err(error) => warn!("Battle-end hook unavailable; using inactivity fallback: {error}"),
    }

    match OnDeathHook::new(tx.clone()).setup(&process) {
        Ok(()) => info!("Game 2.0.2 player-death hook enabled"),
        Err(error) => warn!("Player-death hook unavailable: {error}"),
    }

    warn!("Running in game 2.0 compatibility mode: equipment hooks remain disabled");

    Ok(())
}

#[inline(always)]
pub unsafe fn v_func<T: Sized>(ptr: *const usize, offset: usize) -> T {
    ((ptr.read() as *const usize).byte_add(offset) as *const T).read()
}

#[inline(always)]
pub fn actor_type_id(actor_ptr: *const usize) -> u32 {
    let mut type_id: u32 = 0;

    unsafe {
        v_func::<GetEntityHashID0x58>(actor_ptr, 0x58)(actor_ptr, &mut type_id as *mut u32);
    }

    type_id
}

#[inline(always)]
pub fn actor_idx(actor_ptr: *const usize) -> u32 {
    let mut actor_ids = ACTOR_IDS
        .get_or_init(|| Mutex::new(ActorIds::default()))
        .lock()
        .expect("actor ID map lock poisoned");

    let instance = actor_ptr as usize;

    if let Some(id) = actor_ids.by_instance.get(&instance) {
        return *id;
    }

    let id = actor_ids.next_id;
    actor_ids.next_id = actor_ids.next_id.wrapping_add(1);
    actor_ids.by_instance.insert(instance, id);
    id
}

// Returns the parent entity of the source entity if necessary.
#[inline(always)]
pub fn get_source_parent(source_type_id: u32, source: *const usize) -> Option<(u32, u32)> {
    let parent_instance = get_source_parent_instance(source_type_id, source)?;
    Some((actor_type_id(parent_instance), actor_idx(parent_instance)))
}

#[inline(always)]
pub fn get_source_parent_instance(
    source_type_id: u32,
    source: *const usize,
) -> Option<*const usize> {
    parent_specified_instance_at(source, source_parent_offset(source_type_id)?)
}

fn source_parent_offset(source_type_id: u32) -> Option<usize> {
    match source_type_id {
        0x2AF678E8 => Some(0xE58),
        0x8364C8BC => Some(0x4E8),
        0xC9F45042 => Some(0x558),
        0x5B1AB457 => Some(0x4E0),
        0xF5755C0E => Some(0x1CA80),
        _ => None,
    }
}

// Returns the specified instance of the parent entity.
// ptr+offset: Entity
// *(ptr+offset) + 0x70: m_pSpecifiedInstance (Pl0700, Pl1200, etc.)
#[inline(always)]
fn parent_specified_instance_at(actor_ptr: *const usize, offset: usize) -> Option<*const usize> {
    let actor_address = actor_ptr as usize;
    let info = read_process_value::<usize>(actor_address.checked_add(offset)?)?;
    if info == 0 {
        return None;
    }

    let parent = read_process_value::<usize>(info.checked_add(0x70)?)?;
    (parent != 0).then_some(parent as *const usize)
}

fn read_process_value<T: Copy>(address: usize) -> Option<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut bytes_read = 0usize;
    let result = unsafe {
        ReadProcessMemory(
            HANDLE(-1),
            address as *const c_void,
            value.as_mut_ptr().cast::<c_void>(),
            std::mem::size_of::<T>(),
            Some(&mut bytes_read),
        )
    };

    if result.is_err() || bytes_read != std::mem::size_of::<T>() {
        return None;
    }

    Some(unsafe { value.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::{actor_idx, source_parent_offset};

    #[test]
    fn concrete_actor_instances_receive_distinct_ids() {
        let first = 0x1000usize as *const usize;
        let second = 0x2000usize as *const usize;

        assert_eq!(actor_idx(first), actor_idx(first));
        assert_ne!(actor_idx(first), actor_idx(second));
    }

    #[test]
    fn only_live_validated_game_2_parent_offsets_are_enabled() {
        assert_eq!(source_parent_offset(0x2AF678E8), Some(0xE58));
        assert_eq!(source_parent_offset(0xC9F45042), Some(0x558));
        assert_eq!(source_parent_offset(0x5B1AB457), Some(0x4E0));
        assert_eq!(source_parent_offset(0x8364C8BC), Some(0x4E8));
        assert_eq!(source_parent_offset(0xF5755C0E), Some(0x1CA80));
        assert_eq!(source_parent_offset(0x69C0CA71), None);
    }
}
