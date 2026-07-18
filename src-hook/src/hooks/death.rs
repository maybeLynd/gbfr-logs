use anyhow::Result;
use log::info;
use retour::static_detour;

use crate::{event, process::Process};

use super::{
    actor_idx, actor_type_id, get_source_parent_instance, read_process_value,
    sba::is_player_actor_type,
};

type ProcessPlayerDeathFunc = unsafe extern "system" fn(*const usize);

static_detour! {
    static ProcessPlayerDeath: unsafe extern "system" fn(*const usize);
}

const PROCESS_PLAYER_DEATH_SIG: &str =
    "55 41 57 41 56 41 54 56 57 53 48 81 ec 30 02 00 00 48 8d ac 24 80 00 00 00 \
     c5 78 29 95 a0 01 00 00 c5 78 29 8d 90 01 00 00 c5 78 29 85 80 01 00 00 \
     c5 f8 29 bd 70 01 00 00 c5 f8 29 b5 60 01 00 00 \
     48 c7 85 58 01 00 00 fe ff ff ff 80 b9 00 01 00 00 00";
const PLAYER_ACTOR_OFFSET: usize = 0x10;
const PLAYER_DEATH_COUNT_OFFSET: usize = 0x104;

fn incremented_death_delta(before: u32, after: u32) -> Option<u32> {
    after.checked_sub(before).filter(|delta| *delta != 0)
}

#[derive(Clone)]
pub struct OnDeathHook {
    tx: event::Tx,
}

impl OnDeathHook {
    pub fn new(tx: event::Tx) -> Self {
        Self { tx }
    }

    pub fn setup(&self, process: &Process) -> Result<()> {
        let process_player_death = process.search_match_address(PROCESS_PLAYER_DEATH_SIG)?;
        let cloned_self = self.clone();

        unsafe {
            let func: ProcessPlayerDeathFunc = std::mem::transmute(process_player_death);
            ProcessPlayerDeath.initialize(func, move |owner| cloned_self.run(owner))?;
            ProcessPlayerDeath.enable()?;
        }

        Ok(())
    }

    fn run(&self, owner: *const usize) {
        let owner_address = owner as usize;
        let actor = owner_address
            .checked_add(PLAYER_ACTOR_OFFSET)
            .and_then(read_process_value::<usize>)
            .filter(|actor| *actor != 0)
            .map(|actor| actor as *const usize);
        let count_before = owner_address
            .checked_add(PLAYER_DEATH_COUNT_OFFSET)
            .and_then(read_process_value::<u32>);

        unsafe { ProcessPlayerDeath.call(owner) };

        let Some(actor) = actor else {
            return;
        };
        let Some(count_before) = count_before else {
            return;
        };
        let Some(count_after) = owner_address
            .checked_add(PLAYER_DEATH_COUNT_OFFSET)
            .and_then(read_process_value::<u32>)
        else {
            return;
        };
        let Some(death_delta) = incremented_death_delta(count_before, count_after) else {
            return;
        };

        let actor_type = actor_type_id(actor);
        if !is_player_actor_type(actor_type) {
            return;
        }
        let actor = get_source_parent_instance(actor_type, actor).unwrap_or(actor);
        let actor_index = actor_idx(actor);

        info!(
            "Player death observed: actor={}, game_count={}, added={}",
            actor_index, count_after, death_delta
        );
        let _ = self
            .tx
            .send(protocol::Message::OnDeathEvent(protocol::OnDeathEvent {
                actor_index,
                death_counter: death_delta,
                is_delta: true,
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::incremented_death_delta;

    #[test]
    fn reports_only_a_real_counter_increment() {
        assert_eq!(incremented_death_delta(0, 1), Some(1));
        assert_eq!(incremented_death_delta(2, 4), Some(2));
        assert_eq!(incremented_death_delta(3, 3), None);
        assert_eq!(incremented_death_delta(3, 0), None);
    }
}
