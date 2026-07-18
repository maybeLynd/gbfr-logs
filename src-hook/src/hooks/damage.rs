use std::{
    collections::HashSet,
    ptr::NonNull,
    sync::{Mutex, OnceLock},
};

use anyhow::{anyhow, Result};
use log::info;
use protocol::{ActionType, Actor, DamageEvent, Message};
use retour::static_detour;

use crate::{event, hooks::ffi::DamageInstance, process::Process};

use super::{actor_idx, actor_type_id, get_source_parent, get_source_parent_instance};

type ProcessDamageEventFunc =
    unsafe extern "system" fn(*const usize, *const usize, *const usize, u8) -> usize;

type ProcessDotEventFunc = unsafe extern "system" fn(*const usize, *mut u8) -> *mut u8;

static_detour! {
    static ProcessDamageEvent: unsafe extern "system" fn(*const usize, *const usize, *const usize, u8) -> usize;
    static ProcessPoisonDotEvent: unsafe extern "system" fn(*const usize, *mut u8) -> *mut u8;
    static ProcessBurnDotEvent: unsafe extern "system" fn(*const usize, *mut u8) -> *mut u8;
}

#[derive(Clone)]
pub struct OnProcessDamageHook {
    tx: event::Tx,
}

const PROCESS_DAMAGE_EVENT_SIG: &str = "e8 $ { ' } 66 83 bc 24 ? ? ? ? ?";
const DOT_METHOD_PROLOGUE: &[u8] = &[
    0x41, 0x57, 0x41, 0x56, 0x41, 0x55, 0x41, 0x54, 0x56, 0x57, 0x55, 0x53, 0x48, 0x83, 0xEC, 0x78,
];
const POISON_DOT_RESULT_SIG: &str =
    "8b 47 40 89 06 c7 46 04 00 00 00 00 c5 ea 2a c3 48 8d 05 ? ? ? ? 48 89 46 08 c5 fa 11 46 10 48 89 f0";
const BURN_DOT_RESULT_SIG: &str =
    "8b 47 40 89 06 c7 46 04 01 00 00 00 c5 ea 2a c3 48 8d 05 ? ? ? ? 48 89 46 08 c5 fa 11 46 10 48 89 f0";
const DOT_FUNCTION_SEARCH_DISTANCE: usize = 0x800;
const POISON_DOT_ID: u32 = 0;
const BURN_DOT_ID: u32 = 1;
const UNAVAILABLE_DAMAGE_CAP: i32 = 99_999_999;
const MAX_REASONABLE_STUN_VALUE: f32 = 1_000_000.0;
static OBSERVED_DAMAGE_SOURCES: OnceLock<Mutex<HashSet<(usize, u32)>>> = OnceLock::new();
static OBSERVED_DAMAGE_ACTIONS: OnceLock<Mutex<HashSet<(u32, ActionType)>>> = OnceLock::new();

#[inline(always)]
fn stun_value_for_event(action_type: &ActionType, raw_stun_value: f32) -> Option<f32> {
    if matches!(action_type, ActionType::SupplementaryDamage(_)) {
        None
    } else {
        (raw_stun_value.is_finite()
            && raw_stun_value > 0.0
            && raw_stun_value <= MAX_REASONABLE_STUN_VALUE)
            .then_some(raw_stun_value)
    }
}

impl OnProcessDamageHook {
    pub fn new(tx: event::Tx) -> Self {
        OnProcessDamageHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(process_dmg_evt) = process.search_address(PROCESS_DAMAGE_EVENT_SIG) {
            #[cfg(feature = "console")]
            println!("Found process dmg event");

            unsafe {
                let func: ProcessDamageEventFunc = std::mem::transmute(process_dmg_evt);

                ProcessDamageEvent
                    .initialize(func, move |a1, a2, a3, a4| cloned_self.run(a1, a2, a3, a4))?;

                ProcessDamageEvent.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find process_dmg_evt"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize, a2: *const usize, a3: *const usize, a4: u8) -> usize {
        // Target is the instance of the actor being damaged.
        // For example: Instance of the Em2700 class.
        let target_specified_instance_ptr: usize = unsafe { *(*a1.byte_add(0x08) as *const usize) };

        let original_value = unsafe { ProcessDamageEvent.call(a1, a2, a3, a4) };

        // This points to the first Entity instance in the 'a2' entity list.
        let source_entity_ptr = unsafe { (a2.byte_add(0x18) as *const *const usize).read() };

        // @TODO(false): For some reason, online + Ferry's Umlauf skill pet can return a null pointer here.
        // Possible data race with online?
        if source_entity_ptr.is_null() {
            return original_value;
        }

        // entity->m_pSpecifiedInstance, offset 0x70 from entity pointer.
        // Returns the specific class instance of the source entity. (e.g. Instance of Pl1200 / Pl0700Ghost)
        let source_specified_instance_ptr: usize = unsafe { *(source_entity_ptr.byte_add(0x70)) };

        let damage_instance = unsafe { NonNull::new(a2 as *mut DamageInstance).unwrap().as_ref() };
        let damage: i32 = damage_instance.damage;

        if original_value == 0 || damage <= 0 {
            return original_value;
        }

        let flags: u64 = damage_instance.flags;

        let action_type: ActionType = if ((1 << 7 | 1 << 50) & flags) != 0 {
            ActionType::LinkAttack
        } else if ((1 << 13 | 1 << 14) & flags) != 0 {
            ActionType::SBA
        } else if ((1 << 15) & flags) != 0 {
            ActionType::SupplementaryDamage(damage_instance.action_id)
        } else {
            ActionType::Normal(damage_instance.action_id)
        };

        // Get the source actor's type ID.
        let source_type_id = actor_type_id(source_specified_instance_ptr as *const usize);
        let source_idx = actor_idx(source_specified_instance_ptr as *const usize);

        let source_address = source_specified_instance_ptr;
        let is_new_source = OBSERVED_DAMAGE_SOURCES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("damage source set lock poisoned")
            .insert((source_address, source_type_id));
        if is_new_source {
            info!(
                "Damage source observed: actor={:#x}, type={:#010x}, index={}",
                source_address, source_type_id, source_idx
            );
        }

        let is_new_action = OBSERVED_DAMAGE_ACTIONS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("damage action set lock poisoned")
            .insert((source_type_id, action_type));
        if is_new_action {
            info!(
                "Damage action observed: type={source_type_id:#010x}, action={action_type:?}, flags={flags:#018x}"
            );
        }

        let source_instance = source_specified_instance_ptr as *const usize;
        let source_parent_instance = get_source_parent_instance(source_type_id, source_instance);
        let (source_parent_type_id, source_parent_idx) = source_parent_instance
            .map(|parent| (actor_type_id(parent), actor_idx(parent)))
            .unwrap_or((source_type_id, source_idx));
        let identity_actor = source_parent_instance.unwrap_or(source_instance);

        if let Some(identity) = super::player::identity_event_for_actor(
            identity_actor,
            source_parent_type_id,
            source_parent_idx,
        ) {
            let _ = self.tx.send(Message::PlayerIdentityEvent(identity));
        }
        if let Some(equipment) = super::player::equipment_event_for_actor(
            identity_actor,
            source_parent_type_id,
            source_parent_idx,
        ) {
            let _ = self.tx.send(Message::PlayerEquipmentEvent(equipment));
        }

        super::sba::observe_sba_for_actor(
            &self.tx,
            source_parent_instance.unwrap_or(source_instance),
            source_parent_type_id,
            source_parent_idx,
        );

        let target_type_id: u32 = actor_type_id(target_specified_instance_ptr as *const usize);
        let target_idx = actor_idx(target_specified_instance_ptr as *const usize);

        let damage_cap = (damage_instance.damage_cap > 0
            && damage_instance.damage_cap < UNAVAILABLE_DAMAGE_CAP)
            .then_some(damage_instance.damage_cap);
        let stun_value = stun_value_for_event(&action_type, damage_instance.stun_value);

        let event = Message::DamageEvent(DamageEvent {
            source: Actor {
                index: source_idx,
                actor_type: source_type_id,
                parent_index: source_parent_idx,
                parent_actor_type: source_parent_type_id,
            },
            target: Actor {
                index: target_idx,
                actor_type: target_type_id,
                parent_index: target_idx,
                parent_actor_type: target_type_id,
            },
            damage,
            flags,
            action_id: action_type,
            attack_rate: None,
            damage_cap,
            stun_value,
        });

        let _ = self.tx.send(event);

        original_value
    }
}

#[cfg(test)]
mod tests {
    use protocol::ActionType;

    use super::stun_value_for_event;

    #[test]
    fn game_2_per_hit_stun_is_used_directly() {
        assert_eq!(
            stun_value_for_event(&ActionType::Normal(1000), 41.088),
            Some(41.088)
        );
        assert_eq!(stun_value_for_event(&ActionType::Normal(1000), 0.0), None);
        assert_eq!(stun_value_for_event(&ActionType::Normal(1000), -1.0), None);
        assert_eq!(
            stun_value_for_event(&ActionType::Normal(1000), f32::NAN),
            None
        );
    }

    #[test]
    fn supplementary_damage_never_claims_stun() {
        assert_eq!(
            stun_value_for_event(&ActionType::SupplementaryDamage(101), 39.48),
            None
        );
    }
}

#[derive(Clone)]
pub struct OnProcessDotHook {
    tx: event::Tx,
}

impl OnProcessDotHook {
    pub fn new(tx: event::Tx) -> Self {
        OnProcessDotHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let poison_address = process.search_function_start(
            POISON_DOT_RESULT_SIG,
            DOT_METHOD_PROLOGUE,
            DOT_FUNCTION_SEARCH_DISTANCE,
        )?;
        let burn_address = process.search_function_start(
            BURN_DOT_RESULT_SIG,
            DOT_METHOD_PROLOGUE,
            DOT_FUNCTION_SEARCH_DISTANCE,
        )?;

        let poison_self = self.clone();
        let burn_self = self.clone();
        unsafe {
            let poison_func: ProcessDotEventFunc = std::mem::transmute(poison_address);
            let burn_func: ProcessDotEventFunc = std::mem::transmute(burn_address);
            ProcessPoisonDotEvent.initialize(poison_func, move |dot, result| {
                poison_self.run_poison(dot, result)
            })?;
            ProcessBurnDotEvent.initialize(burn_func, move |dot, result| {
                burn_self.run_burn(dot, result)
            })?;
            ProcessPoisonDotEvent.enable()?;
            ProcessBurnDotEvent.enable()?;
        }

        Ok(())
    }

    fn run_poison(&self, dot_instance: *const usize, result: *mut u8) -> *mut u8 {
        let original_result = unsafe { ProcessPoisonDotEvent.call(dot_instance, result) };
        self.emit_dot_event(dot_instance, original_result, POISON_DOT_ID);
        original_result
    }

    fn run_burn(&self, dot_instance: *const usize, result: *mut u8) -> *mut u8 {
        let original_result = unsafe { ProcessBurnDotEvent.call(dot_instance, result) };
        self.emit_dot_event(dot_instance, original_result, BURN_DOT_ID);
        original_result
    }

    fn emit_dot_event(&self, dot_instance: *const usize, result: *const u8, dot_id: u32) {
        if dot_instance.is_null() || result.is_null() {
            return;
        }

        let target_info = unsafe { dot_instance.byte_add(0x18).read() } as *const usize;
        let source_info = unsafe { dot_instance.byte_add(0x30).read() } as *const usize;

        if target_info.is_null() || source_info.is_null() {
            return;
        }

        let target = unsafe { target_info.byte_add(0x70).read() } as *const usize;
        let source = unsafe { source_info.byte_add(0x70).read() } as *const usize;

        if target.is_null() || source.is_null() {
            return;
        }

        let damage_value = unsafe { result.byte_add(0x10).cast::<f32>().read_unaligned() };
        if !damage_value.is_finite() || damage_value <= 0.0 {
            return;
        }
        let damage = damage_value.round() as i32;
        if damage <= 0 {
            return;
        }

        let source_idx = actor_idx(source);
        let source_type_id = actor_type_id(source);

        let target_idx = actor_idx(target);
        let target_type_id = actor_type_id(target);

        let (source_parent_type_id, source_parent_idx) =
            get_source_parent(source_type_id, source).unwrap_or((source_type_id, source_idx));

        let event = Message::DamageEvent(DamageEvent {
            source: Actor {
                index: source_idx,
                actor_type: source_type_id,
                parent_index: source_parent_idx,
                parent_actor_type: source_parent_type_id,
            },
            target: Actor {
                index: target_idx,
                actor_type: target_type_id,
                parent_index: target_idx,
                parent_actor_type: target_type_id,
            },
            damage,
            flags: 0,
            action_id: ActionType::DamageOverTime(dot_id),
            attack_rate: None,
            stun_value: None,
            damage_cap: None,
        });

        let _ = self.tx.send(event);
    }
}
