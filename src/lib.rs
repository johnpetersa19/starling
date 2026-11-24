//! Crater Water — Native Horizon Mod (`crater_water`)
//!
//! Volumetric water physics that flows down into holes and craters, accumulates
//! from the bottom up, equalizes pressure, and forms level lakes while leaving
//! untouched natural oceans and rivers dormant.

pub mod config;
pub mod fluid_sim;
pub mod horizon_bridge;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use mod_api::{
    BreakBlockEvent, BucketEmptyEvent, ClientHost, Command, Config, ConfigKind,
    ConfigLoadingEvent, ConfigReloadingEvent, CreateFluidSourceEvent, EntityPlaceEvent,
    FillBucketEvent, FluidTickEvent, Flow, Hooks, HostSide, LevelDimension, LevelHost,
    RegisterCommandsEvent, RegisterConfigsEvent, Registrar, RegistrationContext,
    RunModCommandEvent, ServerHost, ServerTickEvent, ServerTickPhase, SettingChangedEvent,
    SettingSpec, Text,
};

use crate::config::{CONFIG_FILE_NAME, default_config_json};
use crate::fluid_sim::{FluidSimulator, Pos, SimConfig, SimMode};
use crate::horizon_bridge::{LevelWorld, StateCache, StateClass, adopt_neighbourhood};

pub const MOD_ID: &str = "crater_water";

/// Per-dimension simulation state.
#[derive(Default)]
struct DimensionSim {
    sim: FluidSimulator,
    managed: HashSet<Pos>,
}

/// Shared interior-mutable mod state.
///
/// Lock discipline: never hold this lock across `LevelRef::set_block` or other
/// host writes that can synchronously re-enter a hook.
struct ModState {
    config: SimConfig,
    cache: StateCache,
    dimensions: HashMap<String, DimensionSim>,
}

impl Default for ModState {
    fn default() -> Self {
        Self {
            config: SimConfig::default(),
            cache: StateCache::default(),
            dimensions: HashMap::new(),
        }
    }
}

#[derive(Default)]
pub struct CraterWaterMod {
    state: Mutex<ModState>,
}

impl CraterWaterMod {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, ModState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn apply_config_json(&self, text: &str) {
        if let Ok(parsed) = SimConfig::from_json(text) {
            self.lock_state().config = parsed;
        }
    }

    fn dimension_key_str(dim: LevelDimension) -> &'static str {
        dim.key().id()
    }

    fn acting_dimension(host: &dyn ServerHost) -> String {
        host.players()
            .acting_player()
            .and_then(|p| p.dimension(host))
            .map(|id| id.to_string())
            .unwrap_or_else(|| "minecraft:overworld".to_owned())
    }
}

#[mod_api::horizon_mod]
impl Hooks for CraterWaterMod {
    /// Register client-facing settings for **Options > Mods** (`Crater Water > Config`).
    fn register(&self, r: &mut dyn Registrar, _: &RegistrationContext) {
        let _ = r.register_setting(
            SettingSpec::toggle(
                "enabled",
                "crater_water.setting.enabled",
                true,
            )
            .tooltip("crater_water.setting.enabled.tooltip"),
        );

        let _ = r.register_setting(
            SettingSpec::choice(
                "mode",
                "crater_water.setting.mode",
                [
                    "crater_water.setting.mode.finite",
                    "crater_water.setting.mode.spring",
                ],
                0,
            )
            .tooltip("crater_water.setting.mode.tooltip"),
        );

        let _ = r.register_setting(
            SettingSpec::range(
                "max_updates_per_tick",
                "crater_water.setting.max_updates_per_tick",
                16.0,
                2048.0,
                16.0,
                256.0,
            )
            .tooltip("crater_water.setting.max_updates_per_tick.tooltip"),
        );

        let _ = r.register_setting(
            SettingSpec::range(
                "spring_max_blocks",
                "crater_water.setting.spring_max_blocks",
                64.0,
                32768.0,
                64.0,
                4096.0,
            )
            .tooltip("crater_water.setting.spring_max_blocks.tooltip"),
        );

        let _ = r.register_setting(
            SettingSpec::range(
                "flow_interval_ticks",
                "crater_water.setting.flow_interval_ticks",
                1.0,
                20.0,
                1.0,
                2.0,
            )
            .tooltip("crater_water.setting.flow_interval_ticks.tooltip"),
        );
    }

    /// Declare `config/crater_water.json` so Horizon creates it when missing and reloads on edit.
    fn register_configs(&self, _: HostSide, event: &mut RegisterConfigsEvent<'_>) {
        event.configs.push(
            Config::new(MOD_ID, ConfigKind::Common, &default_config_json())
                .file(CONFIG_FILE_NAME),
        );
    }

    fn config_loading(&self, _: HostSide, event: &mut ConfigLoadingEvent<'_>) {
        if event.config.mod_id == MOD_ID && event.config.file == CONFIG_FILE_NAME {
            self.apply_config_json(&event.config.text);
        }
    }

    fn config_reloading(&self, _: HostSide, event: &mut ConfigReloadingEvent<'_>) {
        if event.config.mod_id == MOD_ID && event.config.file == CONFIG_FILE_NAME {
            self.apply_config_json(&event.config.text);
        }
    }

    /// Sync changes made in **Options > Mods** directly into the live `SimConfig`.
    fn setting_changed(&self, host: &mut dyn ClientHost, event: &mut SettingChangedEvent<'_>) {
        if event.mod_id != MOD_ID {
            return;
        }
        let mut st = self.lock_state();
        match event.key {
            "enabled" => {
                if let Some(v) = host.setting_bool("enabled") {
                    st.config.enabled = v;
                }
            }
            "mode" => {
                if let Some(choice) = host.setting_choice("mode") {
                    st.config.mode = if choice == 1 {
                        SimMode::Spring
                    } else {
                        SimMode::Finite
                    };
                }
            }
            "max_updates_per_tick" => {
                if let Some(v) = host.setting_f64("max_updates_per_tick") {
                    st.config.max_updates_per_tick = (v as usize).clamp(1, 16_384);
                }
            }
            "spring_max_blocks" => {
                if let Some(v) = host.setting_f64("spring_max_blocks") {
                    st.config.spring_max_blocks = (v as usize).clamp(1, 262_144);
                }
            }
            "flow_interval_ticks" => {
                if let Some(v) = host.setting_f64("flow_interval_ticks") {
                    st.config.flow_interval_ticks = (v as u32).clamp(1, 40);
                }
            }
            _ => {}
        }
    }

    /// Register `/craterwater` commands (`finite`, `spring`, `status`, `clear`).
    fn register_commands(&self, _: &mut dyn ServerHost, event: &mut RegisterCommandsEvent<'_>) {
        event.commands.push(Command::new("craterwater finite"));
        event.commands.push(Command::new("craterwater spring"));
        event.commands.push(Command::new("craterwater status"));
        event.commands.push(Command::new("craterwater clear"));
    }

    fn run_mod_command(&self, host: &mut dyn ServerHost, event: &mut RunModCommandEvent<'_>) -> Flow {
        let lits = &event.command.literals;
        if lits.first().map(String::as_str) != Some("craterwater") {
            return Flow::Continue;
        }
        let sub = lits.get(1).map(String::as_str).unwrap_or("status");
        let reply = {
            let mut st = self.lock_state();
            match sub {
                "finite" => {
                    st.config.mode = SimMode::Finite;
                    "[CraterWater] Mode set to Finite Realistic (volume conserved).".to_owned()
                }
                "spring" => {
                    st.config.mode = SimMode::Spring;
                    format!(
                        "[CraterWater] Mode set to Spring / Fill Crater (limit: {} blocks).",
                        st.config.spring_max_blocks
                    )
                }
                "clear" => {
                    for d in st.dimensions.values_mut() {
                        d.sim.clear_springs();
                    }
                    "[CraterWater] Cleared all active water springs.".to_owned()
                }
                _ => {
                    let active_cells: usize =
                        st.dimensions.values().map(|d| d.sim.active_count()).sum();
                    let active_springs: usize =
                        st.dimensions.values().map(|d| d.sim.spring_count()).sum();
                    format!(
                        "[CraterWater] enabled={}, mode={:?}, budget={}/tick, active_cells={}, springs={}",
                        st.config.enabled,
                        st.config.mode,
                        st.config.max_updates_per_tick,
                        active_cells,
                        active_springs
                    )
                }
            }
        };
        let _ = host.commands().feedback(Text::literal(reply));
        Flow::Cancel
    }

    /// Suppress vanilla 2x2 infinite water source formation on managed water when in Finite mode.
    fn create_fluid_source(
        &self,
        _: &dyn LevelHost,
        event: &mut CreateFluidSourceEvent,
    ) {
        let pos = (event.pos.x, event.pos.y, event.pos.z);
        let st = self.lock_state();
        if !st.config.enabled || st.config.mode != SimMode::Finite {
            return;
        }
        let dim = Self::dimension_key_str(event.dimension);
        if let Some(dsim) = st.dimensions.get(dim) {
            if dsim.managed.contains(&pos) || dsim.sim.is_tracked(pos) {
                event.can_convert = false;
            }
        }
    }

    /// Cancel vanilla `FlowingFluid.tick` for water blocks managed by Crater Water so vanilla
    /// spreading does not duplicate or fight our volumetric solver. Untouched natural oceans
    /// and rivers are not in `managed`, so they keep normal vanilla behavior.
    fn fluid_tick(&self, _: &dyn LevelHost, event: &mut FluidTickEvent<'_>) -> Flow {
        if event.fluid != "water" && event.fluid != "minecraft:water" && event.fluid != "minecraft:flowing_water" {
            return Flow::Continue;
        }
        let pos = (event.pos.x, event.pos.y, event.pos.z);
        let st = self.lock_state();
        if !st.config.enabled {
            return Flow::Continue;
        }
        let dim = Self::dimension_key_str(event.dimension);
        if let Some(dsim) = st.dimensions.get(dim) {
            if dsim.managed.contains(&pos) || dsim.sim.is_tracked(pos) {
                return Flow::Cancel;
            }
        }
        Flow::Continue
    }

    /// Player empties a water bucket -> mark the target block as managed volumetric water
    /// (and register as a spring if in Spring mode).
    fn bucket_empty(&self, host: &mut dyn ServerHost, event: &mut BucketEmptyEvent<'_>) -> Flow {
        if event.fluid != "minecraft:water" && event.fluid != "water" {
            return Flow::Continue;
        }
        let pos: Pos = (event.pos[0], event.pos[1], event.pos[2]);
        let dim = Self::acting_dimension(host);
        let mut st = self.lock_state();
        if !st.config.enabled {
            return Flow::Continue;
        }
        st.cache.resolve(host);
        let mode = st.config.mode;
        let is_natural_spring = host
            .level(&dim)
            .is_some_and(|level| st.cache.is_natural_spring_bed(host, level, pos));
        let dsim = st.dimensions.entry(dim).or_default();
        dsim.managed.insert(pos);
        if mode == SimMode::Spring || is_natural_spring {
            dsim.sim.register_spring(pos);
        } else {
            dsim.sim.perturb_neighborhood(pos);
        }
        Flow::Continue
    }

    /// Also listen to `fill_bucket` (fires for both mainhand and offhand bucket clicks, as well
    /// as scooping water with an empty bucket) so holding another item in the other hand never
    /// prevents water placement or spring creation from being tracked.
    fn fill_bucket(&self, host: &mut dyn ServerHost, event: &mut FillBucketEvent<'_>) -> Flow {
        let item_id = event.stack.item().id(host);
        let is_water_bucket = matches!(
            item_id.as_str(),
            "minecraft:water_bucket"
                | "minecraft:cod_bucket"
                | "minecraft:salmon_bucket"
                | "minecraft:pufferfish_bucket"
                | "minecraft:tropical_fish_bucket"
                | "minecraft:axolotl_bucket"
                | "minecraft:tadpole_bucket"
        );
        let is_empty_bucket = item_id == "minecraft:bucket";
        if !is_water_bucket && !is_empty_bucket {
            return Flow::Continue;
        }
        let pos: Pos = (event.pos[0], event.pos[1], event.pos[2]);
        let dim = Self::acting_dimension(host);
        let mut st = self.lock_state();
        if !st.config.enabled {
            return Flow::Continue;
        }
        st.cache.resolve(host);
        let mode = st.config.mode;
        let is_natural_spring = is_water_bucket
            && host
                .level(&dim)
                .is_some_and(|level| st.cache.is_natural_spring_bed(host, level, pos));
        let dsim = st.dimensions.entry(dim).or_default();
        if is_water_bucket {
            dsim.managed.insert(pos);
            if mode == SimMode::Spring || is_natural_spring {
                dsim.sim.register_spring(pos);
            } else {
                dsim.sim.perturb_neighborhood(pos);
            }
        } else {
            dsim.sim.unregister_spring(pos);
            dsim.sim.perturb_neighborhood(pos);
        }
        Flow::Continue
    }

    /// Player breaks a block -> wake any adjacent water so it can flow into the newly opened cavity.
    fn break_block(&self, host: &mut dyn ServerHost, event: &mut BreakBlockEvent<'_>) -> Flow {
        let pos: Pos = (event.pos[0], event.pos[1], event.pos[2]);
        let dim = Self::acting_dimension(host);
        let Some(level) = host.level(&dim) else {
            return Flow::Continue;
        };
        let mut st = self.lock_state();
        if !st.config.enabled {
            return Flow::Continue;
        }
        st.cache.resolve(host);
        let ModState {
            ref cache,
            ref mut dimensions,
            ..
        } = *st;
        let dsim = dimensions.entry(dim).or_default();
        dsim.sim.unregister_spring(pos);
        adopt_neighbourhood(host, level, cache, &mut dsim.managed, pos, true);
        dsim.sim.perturb_neighborhood(pos);
        Flow::Continue
    }

    /// Player (or entity) places a block -> if it displaces or sits beside managed water, wake
    /// the neighborhood; if a player places a water block directly, manage it.
    fn entity_place(&self, host: &mut dyn ServerHost, event: &mut EntityPlaceEvent) -> Flow {
        if event.placer.is_none() {
            return Flow::Continue;
        }
        let pos: Pos = (event.pos.x, event.pos.y, event.pos.z);
        let dim = Self::acting_dimension(host);
        let Some(level) = host.level(&dim) else {
            return Flow::Continue;
        };
        let mut st = self.lock_state();
        if !st.config.enabled {
            return Flow::Continue;
        }
        st.cache.resolve(host);
        let is_water_placed = st.cache.classify(event.state) == StateClass::WaterBlock;
        let mode = st.config.mode;
        let ModState {
            ref cache,
            ref mut dimensions,
            ..
        } = *st;
        let dsim = dimensions.entry(dim).or_default();
        if is_water_placed {
            dsim.managed.insert(pos);
            if mode == SimMode::Spring {
                dsim.sim.register_spring(pos);
            } else {
                dsim.sim.perturb_neighborhood(pos);
            }
        } else {
            dsim.managed.remove(&pos);
            dsim.sim.unregister_spring(pos);
            adopt_neighbourhood(host, level, cache, &mut dsim.managed, pos, false);
            dsim.sim.perturb_neighborhood(pos);
        }
        Flow::Continue
    }

    /// Run one volumetric simulation step per dimension at the end of each authoritative server tick.
    fn server_tick(&self, host: &mut dyn ServerHost, event: &mut ServerTickEvent<'_>) {
        if !matches!(event.phase, ServerTickPhase::ServerTickEnd { .. }) {
            return;
        }

        // Take out the state we need so we do NOT hold the Mutex while calling `level.set_block`
        // (which can synchronously fire `block_changed` / other hooks).
        let (config, cache, mut dims) = {
            let mut st = self.lock_state();
            if !st.config.enabled || st.dimensions.is_empty() {
                return;
            }
            st.cache.resolve(host);
            if !st.cache.resolved {
                return;
            }
            let cfg = st.config;
            let cache = StateCache {
                air_states: st.cache.air_states.clone(),
                water_states: st.cache.water_states.clone(),
                sand_states: st.cache.sand_states.clone(),
                stone_states: st.cache.stone_states.clone(),
                by_volume: st.cache.by_volume,
                water_keys: st.cache.water_keys.clone(),
                resolved: st.cache.resolved,
            };
            let dims = std::mem::take(&mut st.dimensions);
            (cfg, cache, dims)
        };

        for (dim_id, dsim) in &mut dims {
            if dsim.sim.active_count() == 0 && dsim.sim.spring_count() == 0 {
                continue;
            }
            let Some(level) = host.level(dim_id) else {
                continue;
            };
            let mut world = LevelWorld {
                host,
                level,
                cache: &cache,
                managed: &mut dsim.managed,
            };
            dsim.sim.tick(&mut world, &config);
        }

        // Merge any concurrent perturbations that may have arrived during `set_block` callbacks.
        let mut st = self.lock_state();
        for (dim_id, mut ran) in dims {
            if let Some(mut concurrent) = st.dimensions.remove(&dim_id) {
                for p in concurrent.managed {
                    ran.managed.insert(p);
                }
                for p in concurrent.sim.drain_active() {
                    ran.sim.enqueue(p);
                }
                for (spos, used) in concurrent.sim.drain_springs() {
                    ran.sim.merge_spring(spos, used);
                }
            }
            st.dimensions.insert(dim_id, ran);
        }
    }
}
