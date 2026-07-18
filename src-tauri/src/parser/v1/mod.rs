use std::{collections::HashMap, io::BufReader};

use anyhow::Result;
use chrono::Utc;
use protocol::{
    AreaEnterEvent, DamageEvent, Message, OnAttemptSBAEvent, OnContinueSBAChainEvent, OnDeathEvent,
    OnPerformSBAEvent, OnUpdateSBAEvent, PlayerEquipmentEvent, PlayerIdentityEvent,
    PlayerLoadEvent, QuestCompleteEvent,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Window};

use super::{
    constants::{CharacterType, EnemyType},
    v0,
};

mod cap_detection;
mod master_traits;
mod player_state;
mod skill_state;

use player_state::PlayerState;

const ID_ACTOR_TYPE: u32 = 0x8056_ABCD;
const ID_TRANSFORMATION_ACTOR_TYPE: u32 = 0xF575_5C0E;
const INFERRED_SBA_USE_PREVIOUS_MIN: f32 = 900.0;
const INFERRED_SBA_USE_CURRENT_MAX: f32 = 100.0;

type IdTransformationParents = HashMap<u32, u32>;

fn apply_death_count(
    death_counts: &mut HashMap<u32, u32>,
    derived_state: &mut DerivedEncounterState,
    id_transformation_parents: &IdTransformationParents,
    actor_index: u32,
    death_counter: u32,
    is_delta: bool,
) {
    let actor_index = id_transformation_parents
        .get(&actor_index)
        .copied()
        .unwrap_or(actor_index);
    let deaths = death_counts.entry(actor_index).or_default();
    if is_delta {
        *deaths = deaths.saturating_add(death_counter);
    } else {
        *deaths = (*deaths).max(death_counter);
    }

    if let Some(player) = derived_state.party.get_mut(&actor_index) {
        player.set_deaths(*deaths);
    }
}

fn observed_id_transformation_parent(event: &DamageEvent) -> Option<(u32, u32)> {
    (event.source.actor_type == ID_TRANSFORMATION_ACTOR_TYPE
        && event.source.parent_actor_type == ID_ACTOR_TYPE)
        .then_some((event.source.index, event.source.parent_index))
}

fn collect_id_transformation_parents<'a>(
    events: impl Iterator<Item = &'a (i64, Message)>,
) -> IdTransformationParents {
    let mut parents = IdTransformationParents::new();

    for (_, message) in events {
        let Message::DamageEvent(event) = message else {
            continue;
        };
        let Some((transformation_index, parent_index)) = observed_id_transformation_parent(event)
        else {
            continue;
        };

        parents.entry(transformation_index).or_insert(parent_index);
    }

    parents
}

fn resolve_id_transformation_parent(
    event: &DamageEvent,
    players: &[Option<PlayerData>; 4],
    known_parents: &IdTransformationParents,
) -> DamageEvent {
    if event.source.actor_type != ID_TRANSFORMATION_ACTOR_TYPE {
        return event.clone();
    }

    if let Some(parent_index) = known_parents.get(&event.source.index).copied() {
        let mut resolved = event.clone();
        resolved.source.parent_actor_type = ID_ACTOR_TYPE;
        resolved.source.parent_index = parent_index;
        return resolved;
    }

    if event.source.parent_actor_type == ID_ACTOR_TYPE {
        return event.clone();
    }

    let mut ids = players
        .iter()
        .flatten()
        .filter(|player| player.character_type == CharacterType::Pl1900);
    let Some(id) = ids.next() else {
        return event.clone();
    };
    if ids.next().is_some() {
        return event.clone();
    }

    let mut resolved = event.clone();
    resolved.source.parent_actor_type = ID_ACTOR_TYPE;
    resolved.source.parent_index = id.actor_index;
    resolved
}

fn damage_cap_enabled_for_actor(players: &[Option<PlayerData>; 4], actor_index: u32) -> bool {
    players.iter().enumerate().any(|(party_index, player)| {
        player.as_ref().is_some_and(|player| {
            player.actor_index == actor_index && (party_index == 0 || !player.is_online)
        })
    })
}

fn restrict_damage_cap_to_local_and_ai(
    mut event: DamageEvent,
    players: &[Option<PlayerData>; 4],
) -> DamageEvent {
    if !damage_cap_enabled_for_actor(players, event.source.parent_index) {
        event.damage_cap = None;
    }
    event
}

pub struct AdjustedDamageInstance<'a> {
    pub event: &'a DamageEvent,
    pub player_data: Option<&'a PlayerData>,
    pub stun_damage: f64,
    pub is_capped: bool,
    pub cap_known: bool,
}

impl<'a> AdjustedDamageInstance<'a> {
    pub fn from_damage_event(event: &'a DamageEvent, player_data: Option<&'a PlayerData>) -> Self {
        Self::from_damage_event_with_multipliers(event, player_data, &[])
    }

    pub fn from_damage_event_with_multipliers(
        event: &'a DamageEvent,
        player_data: Option<&'a PlayerData>,
        crit_multipliers: &[f64],
    ) -> Self {
        let stun_damage = event.stun_value.unwrap_or(0.0) as f64;
        let cap_known = cap_detection::is_cap_known(event.damage_cap);
        let is_capped = cap_detection::is_capped(event.damage, event.damage_cap, crit_multipliers);

        Self {
            event,
            player_data,
            stun_damage,
            is_capped,
            cap_known,
        }
    }
}

/// Equippable sigil for a character
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct WeaponInfo {
    /// Weapon ID Hash
    pub weapon_id: u32,
    /// How many uncap stars the weapon has
    pub star_level: u32,
    /// Number of plus marks on the weapon
    pub plus_marks: u32,
    /// Weapon's awakening level
    pub awakening_level: u32,
    /// First trait ID
    pub trait_1_id: u32,
    /// First trait level
    pub trait_1_level: u32,
    /// Second trait ID
    pub trait_2_id: u32,
    /// Second trait level
    pub trait_2_level: u32,
    /// Third trait ID
    pub trait_3_id: u32,
    /// Third trait level
    pub trait_3_level: u32,
    /// Wrightstone used on the weapon
    pub wrightstone_id: u32,
    /// Current weapon level
    pub weapon_level: u32,
    /// Weapon's HP Stats (before plus marks)
    pub weapon_hp: u32,
    /// Weapon's Attack Stats (before plus marks)
    pub weapon_attack: u32,
}

impl From<protocol::WeaponInfo> for WeaponInfo {
    fn from(info: protocol::WeaponInfo) -> Self {
        Self {
            weapon_id: info.weapon_id,
            star_level: info.star_level,
            plus_marks: info.plus_marks,
            awakening_level: info.awakening_level,
            trait_1_id: info.trait_1_id,
            trait_1_level: info.trait_1_level,
            trait_2_id: info.trait_2_id,
            trait_2_level: info.trait_2_level,
            trait_3_id: info.trait_3_id,
            trait_3_level: info.trait_3_level,
            wrightstone_id: info.wrightstone_id,
            weapon_level: info.weapon_level,
            weapon_hp: info.weapon_hp,
            weapon_attack: info.weapon_attack,
        }
    }
}

/// Overmastery, also known as `limit_bonus`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Overmastery {
    /// Overmastery ID
    pub id: u32,
    /// Flags
    pub flags: u32,
    /// Value
    pub value: f32,
}

impl From<protocol::Overmastery> for Overmastery {
    fn from(info: protocol::Overmastery) -> Self {
        Self {
            id: info.id,
            flags: info.flags,
            value: info.value,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OvermasteryInfo {
    pub overmasteries: Vec<Overmastery>,
}

impl From<protocol::OvermasteryInfo> for OvermasteryInfo {
    fn from(info: protocol::OvermasteryInfo) -> Self {
        Self {
            overmasteries: info
                .overmasteries
                .into_iter()
                .map(Overmastery::from)
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStats {
    pub level: u32,
    pub total_hp: u32,
    pub total_attack: u32,
    pub stun_power: f32,
    pub critical_rate: f32,
    pub total_power: u32,
}

impl From<protocol::PlayerStats> for PlayerStats {
    fn from(stats: protocol::PlayerStats) -> Self {
        Self {
            level: stats.level,
            total_hp: stats.total_hp,
            total_attack: stats.total_attack,
            stun_power: stats.stun_power,
            critical_rate: stats.critical_rate,
            total_power: stats.total_power,
        }
    }
}

/// Equippable sigil for a character
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Sigil {
    /// ID of the first trait in this sigil
    pub first_trait_id: u32,
    /// Level of the first trait in this sigil
    pub first_trait_level: u32,
    /// ID of the second trait in this sigil
    pub second_trait_id: u32,
    /// Level of the second trait in this sigil
    pub second_trait_level: u32,
    /// ID of the sigil
    pub sigil_id: u32,
    /// ID of the character that this sigil is equipped to
    pub equipped_character: u32,
    /// Level of the sigil
    pub sigil_level: u32,
    /// Acquisition count, at what sigil count this sigil was acquired
    pub acquisition_count: u32,
    /// 0 is new sigil and shows a (!), 1 is nothing, 2 is notification was checked and removes the (!)
    pub notification_enum: u32,
}

fn sigil_from_protocol(sigil: protocol::Sigil) -> Sigil {
    let mapped = master_traits::sigil_trait_ids(sigil.sigil_id);
    let first_trait_id = mapped.map_or(sigil.first_trait_id, |traits| traits.0);
    let second_trait_id = mapped.map_or(sigil.second_trait_id, |traits| traits.1);
    let trait_level = |id, level| {
        if id == 0 || id == 0x887A_E0B0 {
            0
        } else if (1..=15).contains(&level) {
            level
        } else if (1..=15).contains(&sigil.sigil_level) {
            sigil.sigil_level
        } else {
            0
        }
    };

    Sigil {
        first_trait_id,
        first_trait_level: trait_level(first_trait_id, sigil.first_trait_level),
        second_trait_id,
        second_trait_level: trait_level(second_trait_id, sigil.second_trait_level),
        sigil_id: sigil.sigil_id,
        equipped_character: sigil.equipped_character,
        sigil_level: sigil.sigil_level,
        acquisition_count: sigil.acquisition_count,
        notification_enum: sigil.notification_enum,
    }
}

/// Data for a player in the encounter
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlayerData {
    /// Actor index for this player
    actor_index: u32,
    /// Display name for this player, empty if its an NPC
    display_name: String,
    /// Character name for this player if it's an NPC, otherwise it is the same as display_name
    character_name: String,
    /// Character type for this player
    character_type: CharacterType,
    /// Sigils that this player has equipped
    sigils: Vec<Sigil>,
    /// Whether this player was an online player or not
    is_online: bool,
    /// Weapon info for this player
    weapon_info: Option<WeaponInfo>,
    /// Overmastery info for this player
    overmastery_info: Option<OvermasteryInfo>,
    /// Player stats for this player
    player_stats: Option<PlayerStats>,
    #[serde(default)]
    master_traits: Vec<u32>,
}

/// Derived breakdown for an enemy target
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnemyState {
    index: u32,
    target_type: EnemyType,
    raw_target_type: u32,
    total_damage: u64,
}

impl EnemyState {
    fn update_from_damage_event(&mut self, damage_instance: &AdjustedDamageInstance) {
        self.total_damage += damage_instance.event.damage as u64;
    }
}

/// The necessary details of an encounter that can be used to recreate the state at any point in time.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Encounter {
    pub player_data: [Option<PlayerData>; 4],
    pub quest_id: Option<u32>,
    pub quest_timer: Option<u32>,
    #[serde(default)]
    pub quest_completed: bool,

    /// DEPRECATED: Use `self.event_log()` instead.
    pub event_log: Vec<(i64, DamageEvent)>,

    #[serde(default)]
    pub raw_event_log: Vec<(i64, Message)>,
}

impl Encounter {
    /// Compresses this encounter data into a binary blob.
    pub fn to_blob(&self) -> Result<Vec<u8>> {
        let blob = cbor4ii::serde::to_vec(Vec::new(), &self)?;
        let mut reader = BufReader::new(blob.as_slice());
        let compressed_blob = zstd::encode_all(&mut reader, 3)?;
        Ok(compressed_blob)
    }

    /// Deserializes a binary blob into encounter instance.
    pub fn from_blob(blob: &[u8]) -> Result<Self> {
        let decompressed = zstd::decode_all(blob)?;
        Ok(cbor4ii::serde::from_slice(&decompressed)?)
    }

    /// For older logs that don't have the event log, we need to repopulate it.
    pub fn repopulate_event_log(&mut self) {
        if !self.raw_event_log.is_empty() {
            return;
        }

        for (timestamp, event) in self.event_log.iter() {
            self.raw_event_log
                .push((*timestamp, Message::DamageEvent(event.clone())));
        }
    }

    fn reset_player_data(&mut self) {
        self.player_data[0..=3].clone_from_slice(&[None, None, None, None]);
    }

    fn reset_quest(&mut self) {
        self.quest_id = None;
        self.quest_timer = None;
    }

    fn push_event(&mut self, timestamp: i64, event: protocol::Message) {
        self.raw_event_log.push((timestamp, event));
    }

    pub fn event_log(&self) -> impl Iterator<Item = &(i64, Message)> {
        self.raw_event_log.iter()
    }
}

/// The status of the parser.
#[derive(Debug, Serialize, Deserialize, Default, PartialEq, PartialOrd, Clone, Copy)]
enum ParserStatus {
    #[default]
    Waiting,
    InProgress,
    Stopped,
}

const AUTO_SAVE_INACTIVITY_MS: i64 = 60_000;
const ENCOUNTER_EMIT_INTERVAL_MS: i64 = 100;

/// The state of the encounter after processing all damage events (or all known events for now)
/// Used for parsing the encounter into a calculated format that can be consumed by the front-end.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedEncounterState {
    /// Timestamp of the first damage event
    start_time: i64,
    /// Timestamp of the last damage event (or the last known damage event if the encounter is still in progress)
    end_time: i64,
    /// The total damage done in the encounter
    total_damage: u64,
    /// The total DPS done in the encounter
    dps: f64,
    /// The total stun value done in the encounter
    total_stun_value: f64,
    /// The total stun value per second done in the encounter
    stun_per_second: f64,
    /// Status of the parser
    status: ParserStatus,
    /// Derived party stats
    pub party: HashMap<u32, PlayerState>,
    /// Derived target stats, damage done to each target.
    targets: HashMap<u32, EnemyState>,
}

impl Default for DerivedEncounterState {
    fn default() -> Self {
        Self {
            start_time: 0,
            end_time: 0,
            total_damage: 0,
            dps: 0.0,
            total_stun_value: 0.0,
            stun_per_second: 0.0,
            status: ParserStatus::Waiting,
            party: HashMap::new(),
            targets: HashMap::new(),
        }
    }
}

impl DerivedEncounterState {
    pub fn duration(&self) -> i64 {
        (self.end_time - self.start_time).max(1)
    }

    fn utc_start_time(&self) -> Result<chrono::DateTime<Utc>> {
        chrono::DateTime::from_timestamp_millis(self.start_time)
            .ok_or(anyhow::anyhow!("Failed to convert start time to DateTime"))
    }

    fn start(&mut self, now: i64) {
        self.start_time = now;
        self.end_time = now;
    }

    /// Gets the primary target of the encounter (the target that had the most damage done to it)
    fn get_primary_target(&self) -> Option<&EnemyState> {
        self.targets
            .values()
            .max_by_key(|target| target.total_damage)
    }

    fn process_damage_event(&mut self, now: i64, damage_instance: &AdjustedDamageInstance) {
        self.end_time = now;
        self.total_damage += damage_instance.event.damage as u64;
        self.dps = self.total_damage as f64 / ((self.duration()) as f64 / 1000.0);

        // Update stun value
        self.total_stun_value += damage_instance.stun_damage;
        self.stun_per_second = self.total_stun_value / ((self.duration()) as f64 / 1000.0);

        // Add actor to party if not already present.
        let source_player = self
            .party
            .entry(damage_instance.event.source.parent_index)
            .or_insert(PlayerState {
                index: damage_instance.event.source.parent_index,
                character_type: CharacterType::from_hash(
                    damage_instance.event.source.parent_actor_type,
                ),
                total_damage: 0,
                dps: 0.0,
                sba: 0.0,
                stun_per_second: 0.0,
                total_stun_value: 0.0,
                skill_breakdown: Vec::new(),
                last_known_pet_skill: None,
                capped_hits: 0,
                cap_known_hits: 0,
                deaths: 0,
            });

        // Update player stats from damage event.
        source_player.update_from_damage_event(damage_instance);

        // Update target stats from damage event.
        let target = self
            .targets
            .entry(damage_instance.event.target.parent_index)
            .or_insert(EnemyState {
                index: damage_instance.event.target.parent_index,
                target_type: EnemyType::from_hash(damage_instance.event.target.parent_actor_type),
                raw_target_type: damage_instance.event.target.parent_actor_type,
                total_damage: 0,
            });

        target.update_from_damage_event(damage_instance);

        // Update everyone's DPS
        for player in self.party.values_mut() {
            player.update_dps(now, self.start_time);
        }
    }
}

/// The parser for the encounter.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Parser {
    /// Encounter that will be saved into the database, contains all the state needed to reparse
    pub encounter: Encounter,
    /// Derived state of the encounter, used for parsing the encounter into a calculated format that can be consumed by the front-end
    pub derived_state: DerivedEncounterState,
    /// Status of the parser
    status: ParserStatus,

    #[serde(skip)]
    id_transformation_parents: IdTransformationParents,

    #[serde(skip)]
    death_counts: HashMap<u32, u32>,

    #[serde(skip)]
    last_encounter_emit_at: i64,
    #[serde(skip)]
    encounter_update_pending: bool,

    /// The window handle for the parser, used to send messages to the front-end
    #[serde(skip)]
    app: Option<AppHandle>,

    /// The window handle for the parser, used to send messages to the front-end
    #[serde(skip)]
    window_handle: Option<Window>,

    /// The database connection for the parser, used to save the encounter
    #[serde(skip)]
    db: Option<Connection>,
}

impl Parser {
    pub fn new(app: AppHandle, window: Window, db: Connection) -> Self {
        Self {
            app: Some(app),
            db: Some(db),
            window_handle: Some(window),
            ..Default::default()
        }
    }

    /// Peeks at the first damage event in the log to get the start time of the encounter.
    pub fn start_time(&self) -> i64 {
        if let Some((timestamp, _)) = self.encounter.raw_event_log.first() {
            *timestamp
        } else {
            1
        }
    }

    /// Reparses derived state from a given encounter.
    pub fn from_encounter(encounter: Encounter) -> Self {
        let mut parser = Self {
            encounter,
            ..Default::default()
        };

        parser.reparse();
        parser
    }

    pub fn from_encounter_blob(blob: &[u8]) -> Result<Self> {
        let mut encounter = Encounter::from_blob(blob)?;

        // Repopulate the event log if it's empty.
        encounter.repopulate_event_log();

        Ok(Self::from_encounter(encounter))
    }

    fn learn_crit_multipliers(&self) -> Vec<f64> {
        let damage_and_caps = self.encounter.event_log().filter_map(|(_, event)| {
            if let Message::DamageEvent(event) = event {
                let event = resolve_id_transformation_parent(
                    event,
                    &self.encounter.player_data,
                    &self.id_transformation_parents,
                );
                let event = restrict_damage_cap_to_local_and_ai(event, &self.encounter.player_data);
                event.damage_cap.map(|cap| (event.damage, cap))
            } else {
                None
            }
        });

        cap_detection::learn_crit_multipliers(damage_and_caps)
    }

    /// Reparses derived state from the current encounter.
    pub fn reparse(&mut self) {
        self.id_transformation_parents =
            collect_id_transformation_parents(self.encounter.event_log());
        self.derived_state = Default::default();
        self.derived_state.start(self.start_time());
        self.derived_state.status = self.status;
        self.death_counts.clear();
        let crit_multipliers = self.learn_crit_multipliers();

        for (timestamp, event) in self.encounter.event_log() {
            self.derived_state.end_time = *timestamp;

            match event {
                Message::DamageEvent(event) => {
                    let event = restrict_damage_cap_to_local_and_ai(
                        resolve_id_transformation_parent(
                            event,
                            &self.encounter.player_data,
                            &self.id_transformation_parents,
                        ),
                        &self.encounter.player_data,
                    );
                    let player_data = self
                        .encounter
                        .player_data
                        .iter()
                        .flatten()
                        .find(|player| player.actor_index == event.source.parent_index);

                    let damage_instance =
                        AdjustedDamageInstance::from_damage_event_with_multipliers(
                            &event,
                            player_data,
                            &crit_multipliers,
                        );

                    self.derived_state
                        .process_damage_event(*timestamp, &damage_instance);
                    if let Some(&deaths) = self.death_counts.get(&event.source.parent_index) {
                        if let Some(player) =
                            self.derived_state.party.get_mut(&event.source.parent_index)
                        {
                            player.set_deaths(deaths);
                        }
                    }
                }
                Message::OnUpdateSBA(event) => {
                    let actor_index = self
                        .id_transformation_parents
                        .get(&event.actor_index)
                        .copied()
                        .unwrap_or(event.actor_index);
                    if let Some(player) = self.derived_state.party.get_mut(&actor_index) {
                        player.set_sba(event.sba_value as f64);
                    }
                }
                Message::OnAttemptSBA(event) => {
                    let actor_index = self
                        .id_transformation_parents
                        .get(&event.actor_index)
                        .copied()
                        .unwrap_or(event.actor_index);
                    if let Some(player) = self.derived_state.party.get_mut(&actor_index) {
                        player.set_sba(1000.0);
                    }
                }
                Message::OnPerformSBA(event) => {
                    let actor_index = self
                        .id_transformation_parents
                        .get(&event.actor_index)
                        .copied()
                        .unwrap_or(event.actor_index);
                    if let Some(player) = self.derived_state.party.get_mut(&actor_index) {
                        player.set_sba(0.0);
                    }
                }
                Message::OnDeathEvent(event) => {
                    apply_death_count(
                        &mut self.death_counts,
                        &mut self.derived_state,
                        &self.id_transformation_parents,
                        event.actor_index,
                        event.death_counter,
                        event.is_delta,
                    );
                }
                _ => {}
            }
        }
    }

    // Re-analyzes the encounter with the given targets.
    pub fn reparse_with_options(&mut self, targets: &[EnemyType]) {
        self.id_transformation_parents =
            collect_id_transformation_parents(self.encounter.event_log());
        self.derived_state = Default::default();
        self.derived_state.start(self.start_time());
        self.derived_state.status = self.status;
        self.death_counts.clear();
        let crit_multipliers = self.learn_crit_multipliers();

        for (timestamp, event) in self.encounter.event_log() {
            self.derived_state.end_time = *timestamp;

            match event {
                Message::DamageEvent(event) => {
                    let event = restrict_damage_cap_to_local_and_ai(
                        resolve_id_transformation_parent(
                            event,
                            &self.encounter.player_data,
                            &self.id_transformation_parents,
                        ),
                        &self.encounter.player_data,
                    );
                    // If the target list is empty, then we're not filtering by target.
                    // Otherwise, we only process damage events that match the target list.
                    let target_type = EnemyType::from_hash(event.target.parent_actor_type);

                    if targets.is_empty() || targets.contains(&target_type) {
                        let player_data = self
                            .encounter
                            .player_data
                            .iter()
                            .flatten()
                            .find(|player| player.actor_index == event.source.parent_index);

                        let damage_instance =
                            AdjustedDamageInstance::from_damage_event_with_multipliers(
                                &event,
                                player_data,
                                &crit_multipliers,
                            );

                        self.derived_state
                            .process_damage_event(*timestamp, &damage_instance);
                        if let Some(&deaths) = self.death_counts.get(&event.source.parent_index) {
                            if let Some(player) =
                                self.derived_state.party.get_mut(&event.source.parent_index)
                            {
                                player.set_deaths(deaths);
                            }
                        }
                    }
                }
                Message::OnDeathEvent(event) => {
                    apply_death_count(
                        &mut self.death_counts,
                        &mut self.derived_state,
                        &self.id_transformation_parents,
                        event.actor_index,
                        event.death_counter,
                        event.is_delta,
                    );
                }
                _ => {}
            }
        }
    }

    pub fn generate_sba_chart(&self, interval: i64) -> HashMap<u32, Vec<f32>> {
        let start_time = self.start_time();
        let duration = self.derived_state.duration();
        let chart_len = (duration.max(0) / interval) as usize + 1;
        let mut sampled_values: HashMap<u32, Vec<Option<f32>>> = self
            .derived_state
            .party
            .values()
            .map(|player| (player.index, vec![None; chart_len]))
            .collect();

        for (timestamp, event) in self.encounter.event_log() {
            if let Some((actor_index, sba_value)) = match event {
                Message::OnUpdateSBA(sba_update_event) => {
                    Some((sba_update_event.actor_index, sba_update_event.sba_value))
                }
                Message::OnAttemptSBA(sba_attempt_event) => {
                    Some((sba_attempt_event.actor_index, 1000.0))
                }
                Message::OnPerformSBA(sba_perform_event) => {
                    Some((sba_perform_event.actor_index, 0.0))
                }
                Message::OnContinueSBAChain(sba_continue_event) => {
                    Some((sba_continue_event.actor_index, 0.0))
                }
                _ => None,
            } {
                let actor_index = self.resolve_actor_parent(actor_index);
                let elapsed = *timestamp - start_time;
                if elapsed < 0 {
                    continue;
                }

                let index = (elapsed / interval) as usize;
                if let Some(entries) = sampled_values.get_mut(&actor_index) {
                    if let Some(entry) = entries.get_mut(index) {
                        *entry = Some(sba_value);
                    }
                }
            }
        }

        sampled_values
            .into_iter()
            .map(|(actor_index, entries)| {
                let mut last_value = 0.0;
                let entries = entries
                    .into_iter()
                    .map(|value| {
                        if let Some(value) = value {
                            last_value = value;
                        }
                        last_value
                    })
                    .collect();
                (actor_index, entries)
            })
            .collect()
    }

    pub fn generate_sba_transition_events(&self) -> Vec<(i64, Message)> {
        let explicit_events: Vec<_> = self
            .encounter
            .event_log()
            .filter_map(|(timestamp, event)| {
                let event = match event {
                    Message::OnAttemptSBA(event) => Message::OnAttemptSBA(OnAttemptSBAEvent {
                        actor_index: self.resolve_actor_parent(event.actor_index),
                    }),
                    Message::OnPerformSBA(event) => Message::OnPerformSBA(OnPerformSBAEvent {
                        actor_index: self.resolve_actor_parent(event.actor_index),
                    }),
                    Message::OnContinueSBAChain(event) => {
                        Message::OnContinueSBAChain(OnContinueSBAChainEvent {
                            actor_index: self.resolve_actor_parent(event.actor_index),
                        })
                    }
                    _ => return None,
                };
                Some((*timestamp, event))
            })
            .collect();

        if !explicit_events.is_empty() {
            return explicit_events;
        }

        let mut previous_values = HashMap::<u32, f32>::new();
        let mut inferred_events = Vec::new();

        for (timestamp, event) in self.encounter.event_log() {
            let Message::OnUpdateSBA(event) = event else {
                continue;
            };

            let actor_index = self.resolve_actor_parent(event.actor_index);
            if let Some(previous_value) = previous_values.insert(actor_index, event.sba_value) {
                if previous_value >= INFERRED_SBA_USE_PREVIOUS_MIN
                    && event.sba_value <= INFERRED_SBA_USE_CURRENT_MAX
                    && event.sba_value < previous_value
                {
                    inferred_events.push((
                        *timestamp,
                        Message::OnPerformSBA(OnPerformSBAEvent { actor_index }),
                    ));
                }
            }
        }

        inferred_events
    }

    fn resolve_actor_parent(&self, actor_index: u32) -> u32 {
        self.id_transformation_parents
            .get(&actor_index)
            .copied()
            .unwrap_or(actor_index)
    }

    /// Handles the event when an area is entered.
    /// If the current encounter was in progress, then stop it as we've left the instance.
    /// If there was damage in that stopped instance, then save it as a new log.
    /// Otherwise, we're waiting for the encounter to start.
    pub fn on_area_enter_event(&mut self, event: AreaEnterEvent) {
        self.encounter.quest_id = Some(event.last_known_quest_id);

        if self.status == ParserStatus::InProgress {
            self.update_status(ParserStatus::Stopped);

            if self.has_damage() {
                match self.save_encounter_to_db() {
                    Ok(id) => {
                        if let Some(app) = &self.app {
                            let _ = app.emit_all("encounter-saved", id);
                        }
                    }
                    Err(e) => {
                        if let Some(app) = &self.app {
                            let _ = app.emit_all("encounter-saved-error", e.to_string());
                        }
                    }
                }
            }
        } else {
            self.update_status(ParserStatus::Waiting);
        }

        self.encounter.quest_completed = false;
        self.encounter.reset_player_data();

        if let Some(window) = &self.window_handle {
            let _ = window.emit("on-area-enter", &self.derived_state);
        }
    }

    pub fn on_quest_complete_event(&mut self, event: QuestCompleteEvent) {
        self.encounter.quest_id = Some(event.quest_id);
        self.encounter.quest_timer = Some(event.elapsed_time_in_secs);
        self.encounter.quest_completed = true;

        if self.status == ParserStatus::InProgress {
            self.update_status(ParserStatus::Stopped);

            if self.has_damage() {
                match self.save_encounter_to_db() {
                    Ok(id) => {
                        if let Some(window) = &self.window_handle {
                            let _ = window.emit("encounter-saved", id);
                        }
                    }
                    Err(e) => {
                        if let Some(window) = &self.window_handle {
                            let _ = window.emit("encounter-saved-error", e.to_string());
                        }
                    }
                }
            }

            if let Some(window) = &self.window_handle {
                let _ = window.emit("encounter-update", &self.derived_state);
            }
        }
    }

    // Called when a damage event is received from the game.
    pub fn on_damage_event(&mut self, event: DamageEvent) {
        let now = Utc::now().timestamp_millis();

        if Self::should_ignore_damage_event(&event) {
            return;
        }

        // If this is the first damage event, set the start time.
        if self.status == ParserStatus::Stopped || self.status == ParserStatus::Waiting {
            self.reset();
            self.derived_state.start(now);
            self.update_status(ParserStatus::InProgress);
        }

        let newly_learned_id_parent = observed_id_transformation_parent(&event).and_then(
            |(transformation_index, parent_index)| {
                if self
                    .id_transformation_parents
                    .contains_key(&transformation_index)
                {
                    None
                } else {
                    self.id_transformation_parents
                        .insert(transformation_index, parent_index);
                    Some(transformation_index)
                }
            },
        );

        let event = restrict_damage_cap_to_local_and_ai(
            resolve_id_transformation_parent(
                &event,
                &self.encounter.player_data,
                &self.id_transformation_parents,
            ),
            &self.encounter.player_data,
        );

        self.encounter
            .push_event(now, Message::DamageEvent(event.clone()));

        if newly_learned_id_parent
            .is_some_and(|index| self.derived_state.party.contains_key(&index))
        {
            self.reparse();
            self.queue_encounter_update(now);
            return;
        }

        let player_data = self
            .encounter
            .player_data
            .iter()
            .flatten()
            .find(|player| player.actor_index == event.source.parent_index);

        let damage_instance = AdjustedDamageInstance::from_damage_event(&event, player_data);

        self.derived_state
            .process_damage_event(now, &damage_instance);

        if let Some(&deaths) = self.death_counts.get(&event.source.parent_index) {
            if let Some(player) = self.derived_state.party.get_mut(&event.source.parent_index) {
                player.set_deaths(deaths);
            }
        }

        self.queue_encounter_update(now);
    }

    pub fn auto_save_if_inactive(&mut self, now: i64) -> bool {
        self.flush_pending_encounter_update(now);

        if self.status != ParserStatus::InProgress
            || !self.has_damage()
            || now - self.derived_state.end_time < AUTO_SAVE_INACTIVITY_MS
        {
            return false;
        }

        self.finish_and_save_encounter()
    }

    pub fn on_battle_end_event(&mut self) -> bool {
        if self.status != ParserStatus::InProgress || !self.has_damage() {
            return false;
        }

        self.finish_and_save_encounter()
    }

    fn finish_and_save_encounter(&mut self) -> bool {
        self.update_status(ParserStatus::Stopped);

        match self.save_encounter_to_db() {
            Ok(id) => {
                if let Some(app) = &self.app {
                    let _ = app.emit_all("encounter-saved", id);
                } else if let Some(window) = &self.window_handle {
                    let _ = window.emit("encounter-saved", id);
                }

                if let Some(window) = &self.window_handle {
                    let _ = window.emit("encounter-update", &self.derived_state);
                }
                true
            }
            Err(error) => {
                if let Some(app) = &self.app {
                    let _ = app.emit_all("encounter-saved-error", error.to_string());
                } else if let Some(window) = &self.window_handle {
                    let _ = window.emit("encounter-saved-error", error.to_string());
                }

                if let Some(window) = &self.window_handle {
                    let _ = window.emit("encounter-update", &self.derived_state);
                }
                false
            }
        }
    }

    pub fn on_player_load_event(&mut self, event: PlayerLoadEvent) {
        let character_type = CharacterType::from_hash(event.character_type);

        // Ignore Id's transformation.
        if character_type == CharacterType::Pl2000 {
            return;
        }

        let sigils: Vec<Sigil> = event
            .sigils
            .into_iter()
            .map(sigil_from_protocol)
            .collect::<Vec<_>>();

        let player_data = PlayerData {
            actor_index: event.actor_index,
            display_name: event.display_name.to_string_lossy().to_string(),
            character_name: event.character_name.to_string_lossy().to_string(),
            is_online: event.is_online,
            character_type,
            sigils,
            weapon_info: Some(event.weapon_info.into()),
            overmastery_info: Some(event.overmastery_info.into()),
            player_stats: Some(event.player_stats.into()),
            master_traits: if event.is_online {
                Vec::new()
            } else {
                master_traits::load_for_party(event.party_index)
            },
        };

        self.insert_player_data(player_data, event.party_index);
    }

    pub fn on_player_identity_event(&mut self, event: PlayerIdentityEvent) {
        let character_type = CharacterType::from_hash(event.character_type);

        if character_type == CharacterType::Pl2000 {
            return;
        }

        let mut player_data = self
            .encounter
            .player_data
            .iter()
            .flatten()
            .find(|player| player.actor_index == event.actor_index)
            .cloned()
            .unwrap_or(PlayerData {
                actor_index: event.actor_index,
                display_name: String::new(),
                character_name: String::new(),
                character_type,
                sigils: if event.is_online {
                    Vec::new()
                } else {
                    master_traits::load_sigils_for_party(event.party_index)
                },
                is_online: event.is_online,
                weapon_info: if event.is_online {
                    None
                } else {
                    master_traits::load_weapon_for_party(event.party_index)
                },
                overmastery_info: None,
                player_stats: None,
                master_traits: if event.is_online {
                    Vec::new()
                } else {
                    master_traits::load_for_party(event.party_index)
                },
            });

        let was_online = player_data.is_online;
        player_data.display_name = event.display_name.to_string_lossy().to_string();
        player_data.character_name = event.character_name.to_string_lossy().to_string();
        player_data.character_type = character_type;
        player_data.is_online = event.is_online;
        if event.is_online {
            player_data.master_traits.clear();
            if !was_online {
                player_data.sigils.clear();
                player_data.weapon_info = None;
                player_data.overmastery_info = None;
                player_data.player_stats = None;
            }
        } else {
            player_data.master_traits = master_traits::load_for_party(event.party_index);
            if was_online || player_data.sigils.is_empty() {
                player_data.sigils = master_traits::load_sigils_for_party(event.party_index);
            }
            if was_online || player_data.weapon_info.is_none() {
                player_data.weapon_info = master_traits::load_weapon_for_party(event.party_index);
            }
            if was_online {
                player_data.overmastery_info = None;
                player_data.player_stats = None;
            }
        }

        self.insert_player_data(player_data, event.party_index);
    }

    pub fn on_player_equipment_event(&mut self, event: PlayerEquipmentEvent) {
        let character_type = CharacterType::from_hash(event.character_type);
        if character_type == CharacterType::Pl2000 {
            return;
        }

        let sigils: Vec<Sigil> = event
            .sigils
            .into_iter()
            .take(12)
            .map(sigil_from_protocol)
            .collect();

        let mut player_data = self
            .encounter
            .player_data
            .iter()
            .flatten()
            .find(|player| player.actor_index == event.actor_index)
            .cloned()
            .unwrap_or(PlayerData {
                actor_index: event.actor_index,
                display_name: String::new(),
                character_name: String::new(),
                character_type,
                sigils: Vec::new(),
                is_online: event.is_online,
                weapon_info: None,
                overmastery_info: None,
                player_stats: None,
                master_traits: Vec::new(),
            });

        let was_online = player_data.is_online;
        player_data.character_type = character_type;
        player_data.is_online = event.is_online;
        if !event.is_online && event.party_index != 0 {
            player_data.display_name.clear();
            player_data.character_name.clear();
        }
        if !sigils.is_empty() {
            player_data.sigils = sigils;
        }
        let live_master_traits = event.master_traits;
        if event.is_online {
            if live_master_traits.is_none() {
                player_data.master_traits.clear();
            }
            if !was_online {
                player_data.weapon_info = None;
                player_data.overmastery_info = None;
                player_data.player_stats = None;
            }
        } else {
            if live_master_traits.is_none() {
                player_data.master_traits = master_traits::load_for_party(event.party_index);
            }
            if player_data.weapon_info.is_none() {
                player_data.weapon_info = master_traits::load_weapon_for_party(event.party_index);
            }
        }
        if let Some(master_traits) = live_master_traits {
            player_data.master_traits = master_traits;
        }
        if let Some(weapon_info) = event.weapon_info {
            player_data.weapon_info = Some(weapon_info.into());
        }
        if let Some(overmastery_info) = event.overmastery_info {
            player_data.overmastery_info = Some(overmastery_info.into());
        }
        if let Some(player_stats) = event.player_stats {
            player_data.player_stats = Some(player_stats.into());
        }
        self.insert_player_data(player_data, event.party_index);
    }

    fn insert_player_data(&mut self, player_data: PlayerData, party_index: u8) {
        for (index, slot) in self.encounter.player_data.iter_mut().enumerate() {
            if index != party_index as usize
                && slot
                    .as_ref()
                    .is_some_and(|existing| existing.actor_index == player_data.actor_index)
            {
                *slot = None;
            }
        }
        if let Some(slot) = self.encounter.player_data.get_mut(party_index as usize) {
            *slot = Some(player_data.clone());
        }

        if let Some(window) = &self.window_handle {
            let _ = window.emit("encounter-party-update", &self.encounter.player_data);
        }
    }

    /// Handles setting the SBA gauge value for a player
    pub fn on_sba_update(&mut self, event: OnUpdateSBAEvent) {
        let now = Utc::now().timestamp_millis();
        self.encounter
            .push_event(now, Message::OnUpdateSBA(event.clone()));

        let player_index = event.actor_index;
        if let Some(player) = self.derived_state.party.get_mut(&player_index) {
            player.set_sba(event.sba_value as f64);
        }

        self.queue_encounter_update(now);
    }

    pub fn on_sba_attempt(&mut self, event: OnAttemptSBAEvent) {
        let now = Utc::now().timestamp_millis();
        self.encounter
            .push_event(now, Message::OnAttemptSBA(event.clone()));

        let player_index = event.actor_index;
        if let Some(player) = self.derived_state.party.get_mut(&player_index) {
            player.set_sba(1000.0);
        }

        self.queue_encounter_update(now);
    }

    pub fn on_sba_perform(&mut self, event: OnPerformSBAEvent) {
        let now = Utc::now().timestamp_millis();
        self.encounter
            .push_event(now, Message::OnPerformSBA(event.clone()));

        let player_index = event.actor_index;
        if let Some(player) = self.derived_state.party.get_mut(&player_index) {
            player.set_sba(0.0);
        }

        self.queue_encounter_update(now);
    }

    /// @TODO(false): Note that this event only fires for the local player.
    pub fn on_continue_sba_chain(&mut self, event: OnContinueSBAChainEvent) {
        let now = Utc::now().timestamp_millis();
        self.encounter
            .push_event(now, Message::OnContinueSBAChain(event.clone()));

        let player_index = event.actor_index;
        if let Some(player) = self.derived_state.party.get_mut(&player_index) {
            player.set_sba(0.0);
        }

        self.queue_encounter_update(now);
    }

    pub fn on_death_event(&mut self, event: OnDeathEvent) {
        let now = Utc::now().timestamp_millis();
        if self.status == ParserStatus::Stopped || self.status == ParserStatus::Waiting {
            self.reset();
            self.derived_state.start(now);
            self.update_status(ParserStatus::InProgress);
        }

        self.encounter
            .push_event(now, Message::OnDeathEvent(event.clone()));
        apply_death_count(
            &mut self.death_counts,
            &mut self.derived_state,
            &self.id_transformation_parents,
            event.actor_index,
            event.death_counter,
            event.is_delta,
        );
        self.queue_encounter_update(now);
    }

    fn reset(&mut self) {
        self.encounter.raw_event_log.clear();
        self.encounter.raw_event_log.shrink_to_fit();
        self.derived_state = Default::default();
        self.id_transformation_parents.clear();
        self.death_counts.clear();
        self.last_encounter_emit_at = 0;
        self.encounter_update_pending = false;
    }

    fn queue_encounter_update(&mut self, now: i64) {
        self.encounter_update_pending = true;
        self.flush_pending_encounter_update(now);
    }

    fn flush_pending_encounter_update(&mut self, now: i64) -> bool {
        if !self.encounter_update_pending
            || (self.last_encounter_emit_at != 0
                && now - self.last_encounter_emit_at < ENCOUNTER_EMIT_INTERVAL_MS)
        {
            return false;
        }

        if let Some(window) = &self.window_handle {
            let _ = window.emit("encounter-update", &self.derived_state);
        }
        self.last_encounter_emit_at = now;
        self.encounter_update_pending = false;
        true
    }

    fn update_status(&mut self, new_status: ParserStatus) {
        self.status = new_status;
        self.derived_state.status = new_status;
    }

    fn has_damage(&self) -> bool {
        self.derived_state.total_damage > 0
    }

    // Checks if the damage event should be ignored for the purposes of parsing.
    fn should_ignore_damage_event(event: &DamageEvent) -> bool {
        let character_type = CharacterType::from_hash(event.source.parent_actor_type);

        if event.damage <= 0 {
            return true;
        }

        // Eugen's Grenade should be ignored.
        if event.target.actor_type == 0x022a350f {
            return true;
        }

        // If the parent actor type is unknown (not tied to a player character), then ignore it.
        // This usually happens if the damage instance is tied to an enemy/monster.
        if matches!(character_type, CharacterType::Unknown(_)) {
            return true;
        }

        false
    }

    fn save_encounter_to_db(&mut self) -> Result<Option<i64>> {
        let duration_in_millis = self.derived_state.duration();
        let start_datetime = self.derived_state.utc_start_time()?;

        let primary_target = self
            .derived_state
            .get_primary_target()
            .map(|target| target.raw_target_type);

        // Sir Barrold should never save quest ID, as it could be stale.
        if primary_target == Some(0xA379AC65) {
            self.encounter.quest_id = None;
            self.encounter.quest_timer = None;
        }

        let encounter_data = self.encounter.to_blob()?;

        let p1 = self.encounter.player_data[0].as_ref();
        let p2 = self.encounter.player_data[1].as_ref();
        let p3 = self.encounter.player_data[2].as_ref();
        let p4 = self.encounter.player_data[3].as_ref();

        if let Some(conn) = &mut self.db {
            conn.execute(
                r#"INSERT INTO logs (
                        name,
                        time,
                        duration,
                        data,
                        version,
                        primary_target,
                        p1_name,
                        p1_type,
                        p2_name,
                        p2_type,
                        p3_name,
                        p3_type,
                        p4_name,
                        p4_type,
                        quest_id,
                        quest_elapsed_time,
                        quest_completed
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                params![
                    "",
                    start_datetime.timestamp_millis(),
                    duration_in_millis,
                    &encounter_data,
                    1,
                    primary_target,
                    p1.map(|p| p.display_name.as_str()),
                    p1.map(|p| p.character_type.to_string()),
                    p2.map(|p| p.display_name.as_str()),
                    p2.map(|p| p.character_type.to_string()),
                    p3.map(|p| p.display_name.as_str()),
                    p3.map(|p| p.character_type.to_string()),
                    p4.map(|p| p.display_name.as_str()),
                    p4.map(|p| p.character_type.to_string()),
                    self.encounter.quest_id,
                    self.encounter.quest_timer,
                    self.encounter.quest_completed
                ],
            )?;

            let id = conn.last_insert_rowid();

            return Ok(Some(id));
        }

        Ok(None)
    }
}

/// Converts a v0 parser into a v1 parser, but does not reparse the encounter.
impl From<v0::Parser> for Parser {
    fn from(parser: v0::Parser) -> Self {
        let encounter = Encounter {
            event_log: parser.damage_event_log,
            ..Default::default()
        };

        Self {
            encounter,
            status: ParserStatus::Stopped,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use protocol::{ActionType, Actor};

    use super::*;

    fn game_2_damage_event(actor_index: u32, damage: i32, damage_cap: Option<i32>) -> DamageEvent {
        DamageEvent {
            source: Actor {
                index: actor_index,
                actor_type: 0x4C714F77,
                parent_actor_type: 0x4C714F77,
                parent_index: actor_index,
            },
            target: Actor {
                index: 100,
                actor_type: 0x12345678,
                parent_actor_type: 0x12345678,
                parent_index: 100,
            },
            damage,
            flags: 0,
            action_id: ActionType::Normal(1),
            attack_rate: None,
            stun_value: None,
            damage_cap,
        }
    }

    fn id_player(actor_index: u32) -> PlayerData {
        PlayerData {
            actor_index,
            display_name: "Lynd".to_string(),
            character_name: "Id".to_string(),
            character_type: CharacterType::Pl1900,
            sigils: Vec::new(),
            is_online: false,
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: Vec::new(),
        }
    }

    fn party_player(actor_index: u32, is_online: bool) -> PlayerData {
        PlayerData {
            actor_index,
            display_name: format!("Player {actor_index}"),
            character_name: "Maglielle".to_string(),
            character_type: CharacterType::Pl2400,
            sigils: Vec::new(),
            is_online,
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: Vec::new(),
        }
    }

    fn id_transformation_damage(actor_index: u32, damage: i32) -> DamageEvent {
        let mut event = game_2_damage_event(actor_index, damage, None);
        event.source.actor_type = ID_TRANSFORMATION_ACTOR_TYPE;
        event.source.parent_actor_type = ID_TRANSFORMATION_ACTOR_TYPE;
        event
    }

    fn parented_id_transformation_damage(
        transformation_index: u32,
        id_index: u32,
        damage: i32,
    ) -> DamageEvent {
        let mut event = id_transformation_damage(transformation_index, damage);
        event.source.parent_actor_type = ID_ACTOR_TYPE;
        event.source.parent_index = id_index;
        event
    }

    fn id_damage(actor_index: u32, damage: i32) -> DamageEvent {
        let mut event = game_2_damage_event(actor_index, damage, None);
        event.source.actor_type = ID_ACTOR_TYPE;
        event.source.parent_actor_type = ID_ACTOR_TYPE;
        event
    }

    #[test]
    fn can_create_parser() {
        let parser = Parser::default();

        assert_eq!(parser.status, ParserStatus::Waiting);
        assert_eq!(parser.start_time(), 1);
    }

    #[test]
    fn sba_chart_carries_sparse_samples_forward() {
        let mut parser = Parser::default();
        parser.encounter.raw_event_log.extend([
            (
                1_000,
                Message::DamageEvent(game_2_damage_event(7, 100, None)),
            ),
            (
                1_500,
                Message::OnUpdateSBA(OnUpdateSBAEvent {
                    actor_index: 7,
                    sba_value: 200.0,
                    sba_added: 200.0,
                }),
            ),
            (
                3_500,
                Message::OnUpdateSBA(OnUpdateSBAEvent {
                    actor_index: 7,
                    sba_value: 700.0,
                    sba_added: 500.0,
                }),
            ),
            (
                4_000,
                Message::DamageEvent(game_2_damage_event(7, 100, None)),
            ),
        ]);
        parser.reparse();

        assert_eq!(
            parser.generate_sba_chart(1_000)[&7],
            vec![200.0, 200.0, 700.0, 700.0]
        );
    }

    #[test]
    fn gauge_reset_infers_sba_execution_and_maps_id_transformation_to_owner() {
        let mut parser = Parser::default();
        parser.encounter.raw_event_log.extend([
            (1_000, Message::DamageEvent(id_damage(13, 100))),
            (
                1_100,
                Message::OnUpdateSBA(OnUpdateSBAEvent {
                    actor_index: 4,
                    sba_value: 965.0,
                    sba_added: 965.0,
                }),
            ),
            (
                1_200,
                Message::DamageEvent(parented_id_transformation_damage(4, 13, 100)),
            ),
            (
                1_300,
                Message::OnUpdateSBA(OnUpdateSBAEvent {
                    actor_index: 4,
                    sba_value: 0.0,
                    sba_added: -965.0,
                }),
            ),
        ]);
        parser.reparse();

        let events = parser.generate_sba_transition_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, 1_300);
        assert!(matches!(
            &events[0].1,
            Message::OnPerformSBA(OnPerformSBAEvent { actor_index: 13 })
        ));
        assert!(parser.generate_sba_chart(1_000).contains_key(&13));
        assert!(!parser.generate_sba_chart(1_000).contains_key(&4));
    }

    #[test]
    fn inactive_encounter_is_saved_once() {
        let mut parser = Parser::default();
        parser.on_damage_event(game_2_damage_event(1, 100, None));
        let last_damage_at = parser.derived_state.end_time;

        assert!(parser.auto_save_if_inactive(last_damage_at + AUTO_SAVE_INACTIVITY_MS));
        assert_eq!(parser.status, ParserStatus::Stopped);
        assert!(!parser.auto_save_if_inactive(last_damage_at + AUTO_SAVE_INACTIVITY_MS * 2));
    }

    #[test]
    fn battle_end_event_stops_and_saves_once() {
        let mut parser = Parser::default();
        parser.on_damage_event(game_2_damage_event(1, 100, None));

        assert!(parser.on_battle_end_event());
        assert_eq!(parser.status, ParserStatus::Stopped);
        assert!(!parser.on_battle_end_event());
    }

    #[test]
    fn game_2_actor_ids_keep_same_character_players_separate() {
        let mut parser = Parser::default();
        parser.on_damage_event(game_2_damage_event(10, 100, None));
        parser.on_damage_event(game_2_damage_event(11, 100, None));

        assert_eq!(parser.derived_state.party.len(), 2);
    }

    #[test]
    fn reparsing_merges_id_transformation_into_the_only_id() {
        let mut parser = Parser::default();
        parser.encounter.player_data[0] = Some(id_player(5));
        parser
            .encounter
            .raw_event_log
            .push((1_000, Message::DamageEvent(id_damage(5, 100))));
        parser.encounter.raw_event_log.push((
            2_000,
            Message::DamageEvent(id_transformation_damage(4, 200)),
        ));

        parser.reparse();

        assert_eq!(parser.derived_state.party.len(), 1);
        let id = &parser.derived_state.party[&5];
        assert_eq!(id.character_type, CharacterType::Pl1900);
        assert_eq!(id.total_damage, 300);
        assert!(id
            .skill_breakdown
            .iter()
            .any(|skill| skill.child_character_type == CharacterType::Pl2000));
    }

    #[test]
    fn late_id_owner_discovery_merges_earlier_transformation_hits_live() {
        let mut parser = Parser::default();

        parser.on_damage_event(id_transformation_damage(4, 100));
        assert_eq!(parser.derived_state.party[&4].total_damage, 100);

        parser.on_sba_update(OnUpdateSBAEvent {
            actor_index: 4,
            sba_value: 427.5,
            sba_added: 427.5,
        });
        parser.on_damage_event(parented_id_transformation_damage(4, 13, 200));

        assert_eq!(parser.derived_state.party.len(), 1);
        assert!(!parser.derived_state.party.contains_key(&4));
        let id = &parser.derived_state.party[&13];
        assert_eq!(id.character_type, CharacterType::Pl1900);
        assert_eq!(id.total_damage, 300);
        assert_eq!(id.sba, 427.5);
        assert!(id
            .skill_breakdown
            .iter()
            .all(|skill| skill.child_character_type == CharacterType::Pl2000));
    }

    #[test]
    fn late_owner_discovery_keeps_multiple_id_transformations_distinct() {
        let mut parser = Parser::default();

        parser.on_damage_event(id_transformation_damage(4, 100));
        parser.on_damage_event(id_transformation_damage(5, 200));
        parser.on_damage_event(parented_id_transformation_damage(4, 13, 300));
        parser.on_damage_event(parented_id_transformation_damage(5, 14, 400));

        assert_eq!(parser.derived_state.party.len(), 2);
        assert_eq!(parser.derived_state.party[&13].total_damage, 400);
        assert_eq!(parser.derived_state.party[&14].total_damage, 600);
        assert_eq!(
            parser.derived_state.party[&13].character_type,
            CharacterType::Pl1900
        );
        assert_eq!(
            parser.derived_state.party[&14].character_type,
            CharacterType::Pl1900
        );
    }

    #[test]
    fn ambiguous_multiple_id_party_does_not_guess_transformation_owner() {
        let mut parser = Parser::default();
        parser.encounter.player_data[0] = Some(id_player(5));
        parser.encounter.player_data[1] = Some(id_player(6));
        parser.encounter.raw_event_log.push((
            1_000,
            Message::DamageEvent(id_transformation_damage(4, 200)),
        ));

        parser.reparse();

        assert!(parser.derived_state.party.contains_key(&4));
        assert_eq!(
            parser.derived_state.party[&4].character_type,
            CharacterType::Pl2000
        );
    }

    #[test]
    fn live_damage_cap_is_limited_to_local_and_ai_actors() {
        let mut parser = Parser::default();
        parser.encounter.player_data[0] = Some(party_player(10, true));
        parser.encounter.player_data[1] = Some(party_player(11, true));
        parser.encounter.player_data[2] = Some(party_player(12, false));
        parser.on_damage_event(game_2_damage_event(10, 99_999, Some(99_999)));
        parser.on_damage_event(game_2_damage_event(11, 99_999, Some(99_999)));
        parser.on_damage_event(game_2_damage_event(12, 99_999, Some(99_999)));
        parser.on_damage_event(game_2_damage_event(13, 99_999, Some(99_999)));

        assert_eq!(parser.derived_state.party[&10].capped_hits, 1);
        assert_eq!(parser.derived_state.party[&11].capped_hits, 0);
        assert_eq!(parser.derived_state.party[&11].cap_known_hits, 0);
        assert_eq!(parser.derived_state.party[&12].capped_hits, 1);
        assert_eq!(parser.derived_state.party[&13].cap_known_hits, 0);
        let remote_event = parser
            .encounter
            .event_log()
            .find_map(|(_, message)| match message {
                Message::DamageEvent(event) if event.source.parent_index == 11 => Some(event),
                _ => None,
            })
            .unwrap();
        assert_eq!(remote_event.damage_cap, None);
    }

    #[test]
    fn same_character_players_keep_distinct_online_names() {
        let mut parser = Parser::default();

        for (actor_index, party_index, display_name) in [(10, 1, "Player A"), (11, 2, "Player B")] {
            parser.on_player_identity_event(PlayerIdentityEvent {
                character_name: CString::new(display_name).unwrap(),
                display_name: CString::new(display_name).unwrap(),
                character_type: 0x48ADDA36,
                party_index,
                actor_index,
                is_online: true,
            });
        }

        let players = parser
            .encounter
            .player_data
            .iter()
            .flatten()
            .collect::<Vec<_>>();

        assert_eq!(players.len(), 2);
        assert_eq!(players[0].display_name, "Player A");
        assert_eq!(players[1].display_name, "Player B");
        assert_ne!(players[0].actor_index, players[1].actor_index);
        assert_eq!(players[0].character_type, CharacterType::Pl2800);
        assert_eq!(players[1].character_type, CharacterType::Pl2800);
    }

    #[test]
    fn ai_equipment_event_clears_a_reused_online_player_name() {
        let mut parser = Parser::default();
        parser.encounter.player_data[1] = Some(PlayerData {
            actor_index: 10,
            display_name: "Previous Player".to_string(),
            character_name: "Ferry".to_string(),
            character_type: CharacterType::Pl0700,
            sigils: Vec::new(),
            is_online: true,
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: Vec::new(),
        });

        parser.on_player_equipment_event(PlayerEquipmentEvent {
            sigils: Vec::new(),
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: None,
            character_type: 0x48ADDA36,
            party_index: 2,
            actor_index: 10,
            is_online: false,
        });

        assert!(parser.encounter.player_data[1].is_none());
        let ai = parser.encounter.player_data[2].as_ref().unwrap();
        assert!(!ai.is_online);
        assert!(ai.display_name.is_empty());
        assert!(ai.character_name.is_empty());
        assert_eq!(ai.character_type, CharacterType::Pl2800);
    }

    #[test]
    fn live_master_traits_preserve_authoritative_empty_and_selected_builds() {
        let mut parser = Parser::default();
        parser.encounter.player_data[1] = Some(PlayerData {
            actor_index: 10,
            display_name: "Star".to_string(),
            character_name: "Ferry".to_string(),
            character_type: CharacterType::Pl0700,
            sigils: Vec::new(),
            is_online: true,
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: vec![0xDEADBEEF],
        });

        parser.on_player_equipment_event(PlayerEquipmentEvent {
            sigils: Vec::new(),
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: Some(Vec::new()),
            character_type: 0xFBA6615D,
            party_index: 1,
            actor_index: 10,
            is_online: true,
        });
        assert!(parser.encounter.player_data[1]
            .as_ref()
            .unwrap()
            .master_traits
            .is_empty());

        parser.on_player_equipment_event(PlayerEquipmentEvent {
            sigils: Vec::new(),
            weapon_info: None,
            overmastery_info: None,
            player_stats: None,
            master_traits: Some(vec![0x11111111, 0x22222222]),
            character_type: 0xFBA6615D,
            party_index: 1,
            actor_index: 10,
            is_online: true,
        });
        assert_eq!(
            parser.encounter.player_data[1]
                .as_ref()
                .unwrap()
                .master_traits,
            vec![0x11111111, 0x22222222]
        );
    }

    #[test]
    fn recovers_expansion_unique_sigil_traits_from_sigil_ids() {
        for (sigil_id, first_trait_id, second_trait_id) in [
            (1_325_520_586, 1_858_052_470, 4_057_324_496),
            (596_983_764, 2_104_875_268, 3_191_080_121),
            (1_513_492_136, 1_194_869_320, 813_117_847),
            (2_395_713_699, 3_898_603_744, 2_238_888_111),
            (3_457_603_211, 1_415_783_215, 1_159_561_548),
            (3_634_652_401, 2_069_563_421, 2_597_196_811),
        ] {
            let recovered = sigil_from_protocol(protocol::Sigil {
                first_trait_id: 0,
                first_trait_level: 0,
                second_trait_id: 0,
                second_trait_level: 0,
                sigil_id,
                equipped_character: 0,
                sigil_level: 15,
                acquisition_count: 1,
                notification_enum: 1,
            });
            assert_eq!(recovered.first_trait_id, first_trait_id);
            assert_eq!(recovered.first_trait_level, 15);
            assert_eq!(recovered.second_trait_id, second_trait_id);
            assert_eq!(recovered.second_trait_level, 15);
        }
    }

    #[test]
    fn capped_hits_are_aggregated_on_reparse() {
        let mut parser = Parser::default();
        parser.encounter.player_data[0] = Some(party_player(1, true));
        parser.encounter.raw_event_log.push((
            1_000,
            Message::DamageEvent(game_2_damage_event(1, 99_999, Some(99_999))),
        ));
        parser.encounter.raw_event_log.push((
            2_000,
            Message::DamageEvent(game_2_damage_event(1, 100, Some(99_999))),
        ));

        parser.reparse();

        let player = parser.derived_state.party.get(&1).expect("player present");
        assert_eq!(player.capped_hits, 1);
        assert_eq!(player.cap_known_hits, 2);
        assert_eq!(player.skill_breakdown[0].capped_hits, 1);
        assert_eq!(player.skill_breakdown[0].cap_known_hits, 2);
        assert_eq!(player.skill_breakdown[0].hits, 2);
    }

    #[test]
    fn saved_remote_caps_are_ignored_during_reparse() {
        let mut parser = Parser::default();
        parser.encounter.player_data[0] = Some(party_player(1, true));
        parser.encounter.player_data[1] = Some(party_player(2, true));
        parser.encounter.player_data[2] = Some(party_player(3, false));
        for actor_index in 1..=3 {
            parser.encounter.raw_event_log.push((
                actor_index as i64 * 1_000,
                Message::DamageEvent(game_2_damage_event(actor_index, 99_999, Some(99_999))),
            ));
        }

        parser.reparse();

        assert_eq!(parser.derived_state.party[&1].cap_known_hits, 1);
        assert_eq!(parser.derived_state.party[&2].cap_known_hits, 0);
        assert_eq!(parser.derived_state.party[&3].cap_known_hits, 1);
    }

    #[test]
    fn unavailable_remote_cap_is_not_reported_as_zero_percent() {
        let mut parser = Parser::default();
        parser.encounter.player_data[1] = Some(party_player(12, true));
        parser.on_damage_event(game_2_damage_event(
            12,
            cap_detection::UNAVAILABLE_DAMAGE_CAP - 1,
            Some(cap_detection::UNAVAILABLE_DAMAGE_CAP),
        ));

        let player = &parser.derived_state.party[&12];
        assert_eq!(player.capped_hits, 0);
        assert_eq!(player.cap_known_hits, 0);
        assert_eq!(player.skill_breakdown[0].cap_known_hits, 0);
    }

    #[test]
    fn start_time_depends_on_first_event() {
        let mut parser = Parser::default();

        parser.encounter.raw_event_log.push((
            1_000,
            Message::DamageEvent(DamageEvent {
                source: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                target: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                damage: 0,
                flags: 0,
                action_id: ActionType::Normal(0),
                attack_rate: None,
                stun_value: None,
                damage_cap: None,
            }),
        ));

        assert_eq!(parser.start_time(), 1_000);
    }

    #[test]
    fn duration_calculated_from_start_to_current_event() {
        let mut parser = Parser::default();

        parser.encounter.raw_event_log.push((
            1_000,
            Message::DamageEvent(DamageEvent {
                source: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                target: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                damage: 0,
                flags: 0,
                action_id: ActionType::Normal(0),
                attack_rate: None,
                stun_value: None,
                damage_cap: None,
            }),
        ));

        parser.encounter.raw_event_log.push((
            5_000,
            Message::DamageEvent(DamageEvent {
                source: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                target: Actor {
                    index: 0,
                    actor_type: 0,
                    parent_actor_type: 0,
                    parent_index: 0,
                },
                damage: 0,
                flags: 0,
                action_id: ActionType::Normal(0),
                attack_rate: None,
                stun_value: None,
                damage_cap: None,
            }),
        ));

        parser.reparse();

        assert_eq!(parser.derived_state.start_time, 1_000);
        assert_eq!(parser.derived_state.end_time, 5_000);
        assert_eq!(parser.derived_state.duration(), 4_000);
    }

    #[test]
    fn death_before_first_damage_attaches_to_the_exact_actor_and_reparses() {
        let mut parser = Parser::default();

        parser.on_death_event(OnDeathEvent {
            actor_index: 10,
            death_counter: 1,
            is_delta: true,
        });
        parser.on_damage_event(game_2_damage_event(10, 100, Some(100)));
        parser.on_death_event(OnDeathEvent {
            actor_index: 10,
            death_counter: 1,
            is_delta: true,
        });

        assert_eq!(parser.status, ParserStatus::InProgress);
        assert_eq!(parser.derived_state.party[&10].deaths, 2);

        parser.reparse();
        assert_eq!(parser.derived_state.party[&10].deaths, 2);
    }

    #[test]
    fn id_transformation_death_moves_to_base_id_after_owner_discovery() {
        let mut parser = Parser::default();

        parser.on_damage_event(id_transformation_damage(4, 100));
        parser.on_death_event(OnDeathEvent {
            actor_index: 4,
            death_counter: 1,
            is_delta: true,
        });
        parser.on_damage_event(parented_id_transformation_damage(4, 13, 200));

        assert!(!parser.derived_state.party.contains_key(&4));
        assert_eq!(parser.derived_state.party[&13].deaths, 1);
    }

    #[test]
    fn encounter_window_updates_are_coalesced_without_dropping_parser_work() {
        let mut parser = Parser::default();

        parser.queue_encounter_update(1_000);
        assert_eq!(parser.last_encounter_emit_at, 1_000);
        assert!(!parser.encounter_update_pending);

        parser.queue_encounter_update(1_050);
        assert!(parser.encounter_update_pending);
        assert!(!parser.flush_pending_encounter_update(1_099));
        assert!(parser.flush_pending_encounter_update(1_100));
        assert!(!parser.encounter_update_pending);
    }
}
