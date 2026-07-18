use std::{
    ffi::c_void,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{anyhow, Result};
use protocol::Message;
use retour::static_detour;
use windows::Win32::{Foundation::HANDLE, System::Diagnostics::Debug::ReadProcessMemory};

use crate::{
    event,
    hooks::{ffi::QuestState, globals::QUEST_STATE_PTR},
    process::Process,
};

type OnLoadQuestStateFunc = unsafe extern "system" fn(*const usize) -> usize;
type OnShowResultScreenFunc = unsafe extern "system" fn(*const usize) -> usize;
type OnBattleEndFunc = unsafe extern "system" fn(*const usize);

static_detour! {
    static OnLoadQuestState: unsafe extern "system" fn(*const usize) -> usize;
    static OnShowResultScreen: unsafe extern "system" fn(*const usize) -> usize;
    static OnBattleEnd: unsafe extern "system" fn(*const usize);
}

const ON_LOAD_QUEST_STATE: &str =
    "48 8b 0d ? ? ? ? e8 $ { ' } c5 fb 12 ? ? ? ? ? c5 f8 11 ? ? ? ? ? c5 f8 11 ? ? ? ? ? 48 83 c4 48";
const ON_SHOW_RESULT_SCREEN_SIG: &str =
    "e8 $ { ' } b8 ? ? ? ? 23 87 ? ? 00 00 3d 00 00 60 00 0f 94 c0";

const ON_BATTLE_END_SIG: &str =
    "41 56 56 57 53 48 83 ec 38 48 89 ce 48 8b 0d ? ? ? ? 48 8d 54 24 30 41 b8 ab 4e f1 51 e8 ? ? ? ? 48 8b 44 24 30 48 85 c0 74 ? 48 8b 58 18 4c 8b 70 20 4c 39 f3 74 ? 48 8d 7c 24 2c 90 48 8b 0b 48 8b 01 48 89 fa ff 50 10 81 7c 24 2c 45 dc 94 36 74 ? 48 83 c3 10 4c 39 f3 75 ? eb ? 48 8b 03 48 85 c0 74 ? 0f b6 4e 30 88 88 f2 07 00 00";

const QUEST_CONTEXT_SIG: &str =
    "48 8b 05 ? ? ? ? 48 8b 80 10 02 00 00 48 85 c0 74 ? 0f b6 49 30 80 f1 01 88 48 3c c3";
const QUEST_ID_OFFSET: usize = 0x238;
const QUEST_ELAPSED_SECONDS_OFFSET: usize = 0xAC8;
const QUEST_TIME_LIMIT_SECONDS_OFFSET: usize = 0xAD0;
const MAX_REASONABLE_QUEST_SECONDS: u32 = 7 * 24 * 60 * 60;

static QUEST_SINGLETON_SLOT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QuestSnapshot {
    quest_id: u32,
    elapsed_time_in_secs: u32,
}

#[derive(Clone)]
pub struct OnBattleEndHook {
    tx: event::Tx,
}

impl OnBattleEndHook {
    pub fn new(tx: event::Tx) -> Self {
        Self { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();
        let quest_singleton_slot = process
            .search_rip_relative_address(QUEST_CONTEXT_SIG, 3, 7)
            .map_err(|_| anyhow!("Could not resolve game 2.0.2 quest context"))?;
        let on_battle_end = process
            .search_match_address(ON_BATTLE_END_SIG)
            .map_err(|_| anyhow!("Could not find game 2.0.2 battle-end action"))?;

        QUEST_SINGLETON_SLOT.store(quest_singleton_slot, Ordering::Relaxed);

        #[cfg(feature = "console")]
        println!("Found game 2.0.2 battle-end action");

        unsafe {
            let func: OnBattleEndFunc = std::mem::transmute(on_battle_end);
            OnBattleEnd.initialize(func, move |a1| cloned_self.run(a1))?;
            OnBattleEnd.enable()?;
        }

        Ok(())
    }

    fn run(&self, a1: *const usize) {
        let quest = read_quest_snapshot();
        unsafe { OnBattleEnd.call(a1) };

        let message = quest
            .map(|quest| {
                Message::OnQuestComplete(protocol::QuestCompleteEvent {
                    quest_id: quest.quest_id,
                    elapsed_time_in_secs: quest.elapsed_time_in_secs,
                })
            })
            .unwrap_or(Message::OnBattleEnd);
        let _ = self.tx.send(message);
    }
}

fn read_quest_snapshot() -> Option<QuestSnapshot> {
    let singleton_slot = QUEST_SINGLETON_SLOT.load(Ordering::Relaxed);
    if singleton_slot == 0 {
        return None;
    }

    let singleton = read_process_value::<usize>(singleton_slot)?;
    if singleton == 0 {
        return None;
    }

    validate_quest_snapshot(
        read_process_value(singleton.checked_add(QUEST_ID_OFFSET)?)?,
        read_process_value(singleton.checked_add(QUEST_ELAPSED_SECONDS_OFFSET)?)?,
        read_process_value(singleton.checked_add(QUEST_TIME_LIMIT_SECONDS_OFFSET)?)?,
    )
}

fn validate_quest_snapshot(
    quest_id: u32,
    elapsed_time_in_secs: u32,
    time_limit_in_secs: u32,
) -> Option<QuestSnapshot> {
    if quest_id == 0
        || quest_id > 0x00FF_FFFF
        || elapsed_time_in_secs > MAX_REASONABLE_QUEST_SECONDS
        || (time_limit_in_secs != 0 && elapsed_time_in_secs > time_limit_in_secs)
    {
        return None;
    }

    Some(QuestSnapshot {
        quest_id,
        elapsed_time_in_secs,
    })
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

/// Called while loading into a quest.
#[derive(Clone)]
pub struct OnLoadQuestHook {}

impl OnLoadQuestHook {
    pub fn new() -> Self {
        OnLoadQuestHook {}
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(on_load_quest_state) = process.search_address(ON_LOAD_QUEST_STATE) {
            #[cfg(feature = "console")]
            println!("Found on load quest state");

            unsafe {
                let func: OnLoadQuestStateFunc = std::mem::transmute(on_load_quest_state);
                OnLoadQuestState.initialize(func, move |a1| cloned_self.run(a1))?;
                OnLoadQuestState.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_load_quest_state"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!("on load quest state");

        let ret = unsafe { OnLoadQuestState.call(a1) };
        let quest_state_ptr = unsafe { a1.byte_add(0x1D8) } as *mut QuestState;

        if quest_state_ptr.is_null() {
            return ret;
        }

        QUEST_STATE_PTR.store(quest_state_ptr, std::sync::atomic::Ordering::Relaxed);

        ret
    }
}

/// Called whenever the result screen is shown for the quest.
#[derive(Clone)]
pub struct OnQuestCompleteHook {
    tx: event::Tx,
}

impl OnQuestCompleteHook {
    pub fn new(tx: event::Tx) -> Self {
        OnQuestCompleteHook { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let cloned_self = self.clone();

        if let Ok(on_show_result_screen) = process.search_address(ON_SHOW_RESULT_SCREEN_SIG) {
            #[cfg(feature = "console")]
            println!("Found on show result screen");

            unsafe {
                let func: OnShowResultScreenFunc = std::mem::transmute(on_show_result_screen);
                OnShowResultScreen.initialize(func, move |a1| cloned_self.run(a1))?;
                OnShowResultScreen.enable()?;
            }
        } else {
            return Err(anyhow!("Could not find on_show_result_screen"));
        }

        Ok(())
    }

    fn run(&self, a1: *const usize) -> usize {
        #[cfg(feature = "console")]
        println!("on show result screen");

        let quest_state_ptr = QUEST_STATE_PTR.load(Ordering::Relaxed);

        if !quest_state_ptr.is_null() {
            #[cfg(feature = "console")]
            println!("quest_state_ptr: {:p}", quest_state_ptr);

            let quest_state = unsafe { quest_state_ptr.read() };
            let quest_id = quest_state.quest_id;
            let timer = quest_state.elapsed_time;

            let _ = self
                .tx
                .send(Message::OnQuestComplete(protocol::QuestCompleteEvent {
                    quest_id,
                    elapsed_time_in_secs: timer,
                }));
        }

        unsafe { OnShowResultScreen.call(a1) }
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_quest_snapshot, QuestSnapshot, MAX_REASONABLE_QUEST_SECONDS};

    #[test]
    fn accepts_live_game_2_quest_snapshot() {
        assert_eq!(
            validate_quest_snapshot(0x406324, 67, 3000),
            Some(QuestSnapshot {
                quest_id: 0x406324,
                elapsed_time_in_secs: 67,
            })
        );
    }

    #[test]
    fn rejects_invalid_or_inconsistent_quest_snapshots() {
        assert_eq!(validate_quest_snapshot(0, 67, 3000), None);
        assert_eq!(validate_quest_snapshot(0x406324, 3001, 3000), None);
        assert_eq!(
            validate_quest_snapshot(0x406324, MAX_REASONABLE_QUEST_SECONDS + 1, 0),
            None
        );
    }
}
