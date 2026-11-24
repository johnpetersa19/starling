//! Glue between the pure simulator (`fluid_sim`) and a Horizon server level.
//!
//! Ocean protection lives here: water the mod does not *manage* (natural oceans,
//! rivers, vanilla-flowing water) reads as `Solid`, so the simulator can never
//! drain it. Positions become managed only through player perturbations
//! (bucket, block placed/broken) or when the simulator itself writes water there.

use std::collections::HashSet;

use mod_api::{BlockState, BlockUpdateFlags, FluidKey, LevelRef, ServerHost};

use crate::fluid_sim::{Cell, FluidWorld, MAX_WATER_LEVEL, Pos};

/// What a block state means to the simulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateClass {
    Air,
    /// A plain `minecraft:water` block (not a waterlogged block).
    WaterBlock,
    Solid,
}

/// Block states resolved once per world and reused on every write/read.
#[derive(Default)]
pub struct StateCache {
    pub air_states: Vec<BlockState>,
    pub water_states: Vec<BlockState>,
    pub sand_states: Vec<BlockState>,
    pub stone_states: Vec<BlockState>,
    /// `by_volume[v]` for `v` in `1..=8` (index 0 = `minecraft:air`).
    pub by_volume: [Option<BlockState>; 9],
    pub water_keys: Vec<FluidKey>,
    pub resolved: bool,
}

impl StateCache {
    pub fn resolve(&mut self, host: &dyn ServerHost) {
        if self.resolved {
            return;
        }
        self.by_volume[0] = host.block_state_id("minecraft:air");
        for v in 1..=MAX_WATER_LEVEL {
            let name = if v == MAX_WATER_LEVEL {
                // Full blocks (volume 8) are saved as ordinary vanilla water sources (level=0).
                "minecraft:water[level=0]".to_owned()
            } else {
                // Vanilla flowing water: level = 8 - amount (1..=7).
                format!("minecraft:water[level={}]", MAX_WATER_LEVEL - v)
            };
            self.by_volume[v as usize] = host
                .block_state_id(&name)
                .or_else(|| (v == MAX_WATER_LEVEL).then(|| host.block_state_id("minecraft:water")).flatten());
        }

        self.air_states = ["minecraft:air", "minecraft:cave_air", "minecraft:void_air"]
            .into_iter()
            .filter_map(|id| host.block_state_id(id))
            .collect();

        self.sand_states = ["minecraft:sand", "minecraft:red_sand", "minecraft:gravel"]
            .into_iter()
            .filter_map(|id| host.block_state_id(id))
            .collect();

        self.stone_states = [
            "minecraft:stone",
            "minecraft:cobblestone",
            "minecraft:deepslate",
            "minecraft:cobbled_deepslate",
            "minecraft:mossy_cobblestone",
            "minecraft:sandstone",
            "minecraft:smooth_stone",
            "minecraft:andesite",
            "minecraft:granite",
            "minecraft:diorite",
            "minecraft:tuff",
        ]
        .into_iter()
        .filter_map(|id| host.block_state_id(id))
        .collect();

        self.water_states.clear();
        if let Some(default_water) = host.block_state_id("minecraft:water") {
            self.water_states.push(default_water);
        }
        for lvl in 0..=15 {
            if let Some(st) = host.block_state_id(&format!("minecraft:water[level={lvl}]")) {
                if !self.water_states.contains(&st) {
                    self.water_states.push(st);
                }
            }
        }

        self.water_keys = ["minecraft:water", "minecraft:flowing_water"]
            .into_iter()
            .filter_map(|id| host.fluid_key(id))
            .collect();

        self.resolved = self.by_volume.iter().all(Option::is_some) && !self.water_keys.is_empty();
    }

    #[inline]
    pub fn classify(&self, state: BlockState) -> StateClass {
        if self.air_states.contains(&state) {
            StateClass::Air
        } else if self.water_states.contains(&state) {
            StateClass::WaterBlock
        } else {
            StateClass::Solid
        }
    }

    #[inline]
    pub fn is_sand(&self, state: BlockState) -> bool {
        self.sand_states.contains(&state)
    }

    #[inline]
    pub fn is_stone(&self, state: BlockState) -> bool {
        self.stone_states.contains(&state)
    }

    /// Hidden natural spring recipe: returns `true` when the player prepared a
    /// stone + sand bed beneath `pos` (either sand over stone, or sand and stone
    /// side-by-side on the floor directly below `pos`).
    pub fn is_natural_spring_bed(&self, host: &dyn ServerHost, level: LevelRef, pos: Pos) -> bool {
        let floor = [pos.0, pos.1 - 1, pos.2];
        let Some(floor_state) = level.block_state(host, floor) else {
            return false;
        };
        let floor_sand = self.is_sand(floor_state);
        let floor_stone = self.is_stone(floor_state);
        if !floor_sand && !floor_stone {
            return false;
        }

        // Layered aquifer: sand on top of stone (or stone on top of sand).
        if let Some(sub_state) = level.block_state(host, [pos.0, pos.1 - 2, pos.2]) {
            if (floor_sand && self.is_stone(sub_state)) || (floor_stone && self.is_sand(sub_state))
            {
                return true;
            }
        }

        // Mixed floor: sand and stone adjacent at y - 1.
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            if let Some(adj) = level.block_state(host, [pos.0 + dx, pos.1 - 1, pos.2 + dz]) {
                if (floor_sand && self.is_stone(adj)) || (floor_stone && self.is_sand(adj)) {
                    return true;
                }
            }
        }
        false
    }
}

/// A `FluidWorld` view of one server level for one simulation step.
pub struct LevelWorld<'a> {
    pub host: &'a mut dyn ServerHost,
    pub level: LevelRef,
    pub cache: &'a StateCache,
    pub managed: &'a mut HashSet<Pos>,
}

impl FluidWorld for LevelWorld<'_> {
    fn get(&self, pos: Pos) -> Option<Cell> {
        let p = [pos.0, pos.1, pos.2];
        let state = self.level.block_state(&*self.host, p)?;
        Some(match self.cache.classify(state) {
            StateClass::Air => Cell::Air,
            StateClass::Solid => Cell::Solid,
            StateClass::WaterBlock => {
                if !self.managed.contains(&pos) {
                    // Natural / vanilla water: never simulated (ocean protection).
                    return Some(Cell::Solid);
                }
                let fluid = self.level.fluid_state(&*self.host, p)?;
                if !self.cache.water_keys.contains(&fluid.fluid) {
                    return Some(Cell::Solid);
                }
                let amount = if fluid.source {
                    MAX_WATER_LEVEL
                } else {
                    fluid.amount
                };
                Cell::from_volume(amount.min(MAX_WATER_LEVEL))
            }
        })
    }

    fn set(&mut self, pos: Pos, cell: Cell) {
        let v = cell.water_volume() as usize;
        let Some(state) = self.cache.by_volume[v] else {
            return;
        };
        if v > 0 {
            self.managed.insert(pos);
        } else {
            self.managed.remove(&pos);
        }
        let _ = self.level.set_block(
            self.host,
            [pos.0, pos.1, pos.2],
            state,
            BlockUpdateFlags::NEIGHBORS | BlockUpdateFlags::CLIENTS,
        );
    }
}

/// Adopt any water blocks in the 7-cell neighborhood around a player perturbation
/// as managed so they participate in the volumetric simulation.
pub fn adopt_neighbourhood(
    host: &dyn ServerHost,
    level: LevelRef,
    cache: &StateCache,
    managed: &mut HashSet<Pos>,
    center: Pos,
    adopt_water: bool,
) {
    const OFFSETS: [(i32, i32, i32); 7] = [
        (0, 0, 0),
        (0, -1, 0),
        (0, 1, 0),
        (1, 0, 0),
        (-1, 0, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];
    for d in OFFSETS {
        let pos = (center.0 + d.0, center.1 + d.1, center.2 + d.2);
        let Some(state) = level.block_state(host, [pos.0, pos.1, pos.2]) else {
            continue;
        };
        if adopt_water && cache.classify(state) == StateClass::WaterBlock {
            managed.insert(pos);
        }
    }
}
