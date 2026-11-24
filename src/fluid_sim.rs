//! Pure Rust volumetric water physics simulator.
//!
//! Deliberately has zero dependencies on Horizon types so it can be tested
//! exhaustively with `cargo test` in milliseconds.

use std::collections::{HashMap, HashSet, VecDeque};

/// Maximum discrete volume stored in a single water block (8 = vanilla source block).
pub const MAX_WATER_LEVEL: u8 = 8;

/// 3D integer block position `(x, y, z)`.
pub type Pos = (i32, i32, i32);

/// Abstract cell state for the fluid simulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Empty passable space (volume 0).
    Air,
    /// Volumetric water with discrete volume in `1..=8` (8 = full source block).
    Water(u8),
    /// Impassable solid block (or non-water fluid/block).
    Solid,
}

impl Cell {
    /// Discrete water volume of this cell (`0` for `Air` or `Solid`).
    #[inline]
    pub const fn water_volume(self) -> u8 {
        match self {
            Self::Water(v) => v,
            Self::Air | Self::Solid => 0,
        }
    }

    /// Whether water can enter this cell (`Air` or `Water`).
    #[inline]
    pub const fn is_passable(self) -> bool {
        matches!(self, Self::Air | Self::Water(_))
    }

    /// Build a `Cell` from a volume (`0` -> `Air`, `1..=8` -> `Water(v)`).
    #[inline]
    pub const fn from_volume(v: u8) -> Self {
        if v == 0 {
            Self::Air
        } else if v >= MAX_WATER_LEVEL {
            Self::Water(MAX_WATER_LEVEL)
        } else {
            Self::Water(v)
        }
    }
}

/// World read/write interface implemented by both the Horizon bridge and unit-test grids.
pub trait FluidWorld {
    /// Read the cell at `pos`. Return `None` if the chunk is unloaded or out of world bounds.
    fn get(&self, pos: Pos) -> Option<Cell>;

    /// Write `cell` (`Air` or `Water(1..=8)`) to `pos`.
    fn set(&mut self, pos: Pos, cell: Cell);
}

/// Simulation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimMode {
    /// Strict volume conservation (no 2x2 infinite sources).
    Finite,
    /// Player-placed water sources act as continuous springs up to `spring_max_blocks`,
    /// filling craters up to the spring's Y height.
    Spring,
}

/// Simulator configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimConfig {
    pub enabled: bool,
    pub mode: SimMode,
    pub max_updates_per_tick: usize,
    pub spring_max_blocks: usize,
    pub flow_interval_ticks: u32,
    pub pressure_search_limit: usize,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: SimMode::Finite,
            max_updates_per_tick: 256,
            spring_max_blocks: 4096,
            flow_interval_ticks: 2,
            pressure_search_limit: 256,
        }
    }
}

const HORIZONTAL_OFFSETS: [(i32, i32, i32); 4] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 0, 1),
    (0, 0, -1),
];

const NEIGHBOR_OFFSETS: [(i32, i32, i32); 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (1, 0, 0),
    (-1, 0, 0),
    (0, 0, 1),
    (0, 0, -1),
];

#[inline]
fn offset(pos: Pos, d: (i32, i32, i32)) -> Pos {
    (pos.0 + d.0, pos.1 + d.1, pos.2 + d.2)
}

/// Stateful per-dimension volumetric fluid simulator.
///
/// Only cells explicitly perturbed by player actions (or woken by an already-active
/// cell) are simulated. Untouched natural oceans and rivers remain dormant.
#[derive(Debug, Default)]
pub struct FluidSimulator {
    active_queue: VecDeque<Pos>,
    active_set: HashSet<Pos>,
    /// Active springs (used in `SimMode::Spring`), mapping spring position -> volume units emitted.
    springs: HashMap<Pos, usize>,
    tick_counter: u64,
}

impl FluidSimulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently queued active cells.
    pub fn active_count(&self) -> usize {
        self.active_queue.len()
    }

    /// Number of registered springs.
    pub fn spring_count(&self) -> usize {
        self.springs.len()
    }

    /// Whether `pos` is currently queued or is a registered spring.
    pub fn is_tracked(&self, pos: Pos) -> bool {
        self.active_set.contains(&pos) || self.springs.contains_key(&pos)
    }

    /// Enqueue a single position if not already queued.
    pub fn enqueue(&mut self, pos: Pos) {
        if self.active_set.insert(pos) {
            self.active_queue.push_back(pos);
        }
    }

    /// Wake `pos` and its 6 direct neighbors (used when a player places/breaks a block or uses a bucket).
    pub fn perturb_neighborhood(&mut self, pos: Pos) {
        self.enqueue(pos);
        for d in NEIGHBOR_OFFSETS {
            self.enqueue(offset(pos, d));
        }
    }

    /// Register a player-placed water source as a spring (active when `mode == SimMode::Spring`).
    pub fn register_spring(&mut self, pos: Pos) {
        self.springs.entry(pos).or_insert(0);
        self.perturb_neighborhood(pos);
    }

    /// Remove a spring (e.g., when broken or scooped up with a bucket).
    pub fn unregister_spring(&mut self, pos: Pos) {
        self.springs.remove(&pos);
    }

    /// Clear all registered springs.
    pub fn clear_springs(&mut self) {
        self.springs.clear();
    }

    /// Drain all queued active positions.
    pub fn drain_active(&mut self) -> Vec<Pos> {
        self.active_set.clear();
        self.active_queue.drain(..).collect()
    }

    /// Drain all registered springs.
    pub fn drain_springs(&mut self) -> Vec<(Pos, usize)> {
        self.springs.drain().collect()
    }

    /// Merge a spring entry.
    pub fn merge_spring(&mut self, pos: Pos, used: usize) {
        let entry = self.springs.entry(pos).or_insert(0);
        *entry = (*entry).max(used);
    }

    /// Advance the simulation by one server tick, respecting `config.flow_interval_ticks`
    /// and `config.max_updates_per_tick`.
    ///
    /// Returns the number of active cells processed in this tick.
    pub fn tick<W: FluidWorld>(&mut self, world: &mut W, config: &SimConfig) -> usize {
        if !config.enabled {
            return 0;
        }
        self.tick_counter = self.tick_counter.wrapping_add(1);
        let interval = config.flow_interval_ticks.max(1) as u64;
        if self.tick_counter % interval != 0 {
            return 0;
        }
        self.step(world, config)
    }

    /// Run one immediate simulation step up to `config.max_updates_per_tick` cell updates.
    ///
    /// Returns the number of cells processed.
    pub fn step<W: FluidWorld>(&mut self, world: &mut W, config: &SimConfig) -> usize {
        if !config.enabled || config.max_updates_per_tick == 0 {
            return 0;
        }

        if !self.springs.is_empty() {
            self.step_springs(world, config);
        }

        let budget = config.max_updates_per_tick;
        let mut processed = 0;

        while processed < budget {
            let Some(pos) = self.active_queue.pop_front() else {
                break;
            };
            self.active_set.remove(&pos);
            processed += 1;

            self.update_cell(world, pos, config);
        }

        processed
    }

    /// Replenish springs and push water into any unfilled basin cell.
    ///
    /// If the spring sits at the bottom of an enclosed crater and the current layer is
    /// already full, it rises one layer upward as long as the next layer is enclosed by
    /// solid walls, stopping automatically at the crater rim.
    fn step_springs<W: FluidWorld>(&mut self, world: &mut W, config: &SimConfig) {
        const SPRING_FLOW_RATE: u8 = 4;
        const MAX_SPRING_RISE: i32 = 24;

        let max_volume = config.spring_max_blocks.saturating_mul(MAX_WATER_LEVEL as usize);
        let spring_positions: Vec<Pos> = self.springs.keys().copied().collect();

        for spos in spring_positions {
            let Some(cell) = world.get(spos) else {
                continue;
            };
            if matches!(cell, Cell::Solid) {
                self.springs.remove(&spos);
                continue;
            }

            let used = self.springs.get(&spos).copied().unwrap_or(0);
            if used >= max_volume {
                continue;
            }

            let current = cell.water_volume();
            if current < MAX_WATER_LEVEL {
                let added = (MAX_WATER_LEVEL - current).min(SPRING_FLOW_RATE) as usize;
                let remaining = max_volume.saturating_sub(used);
                let grant = added.min(remaining) as u8;
                if grant > 0 {
                    world.set(spos, Cell::from_volume(current + grant));
                    if let Some(entry) = self.springs.get_mut(&spos) {
                        *entry = entry.saturating_add(grant as usize);
                    }
                    self.perturb_neighborhood(spos);
                }
                continue;
            }

            // Climb up through full water blocks above the spring to find the current lake surface.
            let mut top = spos;
            while top.1 - spos.1 < MAX_SPRING_RISE {
                let above = offset(top, (0, 1, 0));
                if matches!(world.get(above), Some(Cell::Water(MAX_WATER_LEVEL))) {
                    top = above;
                } else {
                    break;
                }
            }

            // First try to fill any unfilled cell on the current surface layer (or below it).
            let target = self
                .find_basin_target(world, top, MAX_WATER_LEVEL, true, config)
                .or_else(|| {
                    // Current layer is full: if the layer above is enclosed by crater walls, rise!
                    if top.1 - spos.1 >= MAX_SPRING_RISE {
                        return None;
                    }
                    let above = offset(top, (0, 1, 0));
                    let above_cell = world.get(above)?;
                    if above_cell.is_passable()
                        && above_cell.water_volume() < MAX_WATER_LEVEL
                        && self.is_layer_enclosed(world, above, config)
                    {
                        Some(above)
                    } else {
                        None
                    }
                });

            if let Some(target) = target {
                if let Some(tcell) = world.get(target) {
                    let tv = tcell.water_volume();
                    if tv < MAX_WATER_LEVEL {
                        let space = (MAX_WATER_LEVEL - tv) as usize;
                        let remaining = max_volume.saturating_sub(used);
                        let transfer = space.min(remaining).min(SPRING_FLOW_RATE as usize) as u8;
                        if transfer > 0 {
                            world.set(target, Cell::from_volume(tv + transfer));
                            if let Some(entry) = self.springs.get_mut(&spos) {
                                *entry = entry.saturating_add(transfer as usize);
                            }
                            self.perturb_neighborhood(target);
                            self.enqueue(spos);
                        }
                    }
                }
            }
        }
    }

    /// Returns `true` if the horizontal slice at `origin.1` containing `origin` is completely
    /// enclosed by `Solid` walls within `config.pressure_search_limit` cells.
    fn is_layer_enclosed<W: FluidWorld>(&self, world: &W, origin: Pos, config: &SimConfig) -> bool {
        let limit = config.pressure_search_limit.max(1);
        let mut visited: HashSet<Pos> = HashSet::with_capacity(limit.min(64));
        let mut queue: VecDeque<Pos> = VecDeque::with_capacity(limit.min(64));
        visited.insert(origin);
        queue.push_back(origin);

        while let Some(curr) = queue.pop_front() {
            if visited.len() > limit {
                return false;
            }
            for d in HORIZONTAL_OFFSETS {
                let next = offset(curr, d);
                if !visited.insert(next) {
                    continue;
                }
                let Some(ncell) = world.get(next) else {
                    return false;
                };
                if ncell.is_passable() {
                    if visited.len() > limit {
                        return false;
                    }
                    queue.push_back(next);
                }
            }
        }
        true
    }

    fn update_cell<W: FluidWorld>(&mut self, world: &mut W, pos: Pos, config: &SimConfig) {
        let Some(Cell::Water(mut v)) = world.get(pos) else {
            return;
        };
        if v == 0 {
            world.set(pos, Cell::Air);
            return;
        }
        if v > MAX_WATER_LEVEL {
            v = MAX_WATER_LEVEL;
        }

        // 1. Gravity first: transfer as much volume as possible to the cell directly below.
        let below = offset(pos, (0, -1, 0));
        if let Some(below_cell) = world.get(below) {
            if below_cell.is_passable() {
                let bv = below_cell.water_volume();
                if bv < MAX_WATER_LEVEL {
                    let transfer = v.min(MAX_WATER_LEVEL - bv);
                    let new_v = v - transfer;
                    let new_bv = bv + transfer;
                    world.set(below, Cell::from_volume(new_bv));
                    world.set(pos, Cell::from_volume(new_v));
                    self.perturb_neighborhood(below);
                    self.perturb_neighborhood(pos);
                    return;
                }
            }
        }

        // 2. Direct horizontal equalization: move volume to strictly lower neighbors
        //    whenever the difference is >= 2 (guarantees strict Lyapunov decrease and conservation).
        let mut changed = false;
        let mut neighbors: [(Pos, u8); 4] = [(pos, u8::MAX); 4];
        let mut n_count = 0usize;
        for d in HORIZONTAL_OFFSETS {
            let npos = offset(pos, d);
            if let Some(ncell) = world.get(npos) {
                if ncell.is_passable() {
                    neighbors[n_count] = (npos, ncell.water_volume());
                    n_count += 1;
                }
            }
        }

        if n_count > 0 {
            let slice = &mut neighbors[..n_count];
            // Repeatedly pour 1 unit into the currently lowest neighbor while `v - min >= 2`.
            while v >= 2 {
                let mut best_idx = None;
                let mut best_vol = u8::MAX;
                for (i, &(_, nv)) in slice.iter().enumerate() {
                    if nv < best_vol {
                        best_vol = nv;
                        best_idx = Some(i);
                    }
                }
                let Some(idx) = best_idx else {
                    break;
                };
                if v < best_vol.saturating_add(2) {
                    break;
                }
                v -= 1;
                slice[idx].1 += 1;
                changed = true;
            }

            if changed {
                world.set(pos, Cell::from_volume(v));
                self.perturb_neighborhood(pos);
                for &(npos, nv) in slice.iter() {
                    if world.get(npos).map(Cell::water_volume) != Some(nv) {
                        world.set(npos, Cell::from_volume(nv));
                        self.perturb_neighborhood(npos);
                    }
                }
                return;
            }
        }

        // 3. Basin pressure equalization (BFS):
        //    - If water is stacked above `pos` (hydrostatic pressure), push volume to any
        //      connected cell at `y - 1` or `y` with volume < 8.
        //    - Otherwise (surface cell), search the connected basin at level `y` for either:
        //        a) a deeper pocket at `y - 1` with volume < 8 (water finds the drain/floor), or
        //        b) a cell at `y` with volume <= v - 2 (flattens stairs/ripples across the lake).
        let under_pressure = world
            .get(offset(pos, (0, 1, 0)))
            .is_some_and(|c| c.water_volume() > 0);

        if let Some(target) = self.find_basin_target(world, pos, v, under_pressure, config) {
            if let Some(tcell) = world.get(target) {
                let tv = tcell.water_volume();
                let transfer = if target.1 < pos.1 {
                    // Dropping into a deeper pocket: transfer as much as fits.
                    v.min(MAX_WATER_LEVEL - tv)
                } else {
                    // Same-layer pressure/surface leveling: transfer 1 unit.
                    1.min(v).min(MAX_WATER_LEVEL - tv)
                };
                if transfer > 0 {
                    world.set(pos, Cell::from_volume(v - transfer));
                    world.set(target, Cell::from_volume(tv + transfer));
                    self.perturb_neighborhood(pos);
                    self.perturb_neighborhood(target);
                }
            }
        }
    }

    /// Bounded BFS across connected water at `origin.y` to locate the best equalization target.
    ///
    /// Priority 1: a passable cell at `origin.y - 1` with volume < 8 (fills craters bottom-up).
    /// Priority 2: a passable cell at `origin.y` with the lowest volume satisfying:
    ///   - `tv < 8` when `under_pressure` is true, or
    ///   - `tv <= origin_vol - 2` for surface leveling.
    fn find_basin_target<W: FluidWorld>(
        &self,
        world: &W,
        origin: Pos,
        origin_vol: u8,
        under_pressure: bool,
        config: &SimConfig,
    ) -> Option<Pos> {
        let limit = config.pressure_search_limit.max(1);
        let mut visited: HashSet<Pos> = HashSet::with_capacity(limit.min(64));
        let mut queue: VecDeque<Pos> = VecDeque::with_capacity(limit.min(64));

        visited.insert(origin);
        queue.push_back(origin);

        let mut best_same_layer: Option<(Pos, u8)> = None;
        let mut inspected = 0usize;

        while let Some(curr) = queue.pop_front() {
            if inspected >= limit {
                break;
            }
            inspected += 1;

            // Check directly below `curr` for a deeper pocket (Priority 1).
            if curr != origin {
                let below = offset(curr, (0, -1, 0));
                if let Some(bcell) = world.get(below) {
                    if bcell.is_passable() && bcell.water_volume() < MAX_WATER_LEVEL {
                        return Some(below);
                    }
                }
            }

            // Expand horizontally at `origin.y`.
            for d in HORIZONTAL_OFFSETS {
                let next = offset(curr, d);
                if !visited.insert(next) {
                    continue;
                }
                let Some(ncell) = world.get(next) else {
                    continue;
                };
                if !ncell.is_passable() {
                    continue;
                }
                let nv = ncell.water_volume();

                // Check if `next` itself sits above a deeper pocket (even if `next` is Air).
                let next_below = offset(next, (0, -1, 0));
                if let Some(bcell) = world.get(next_below) {
                    if bcell.is_passable() && bcell.water_volume() < MAX_WATER_LEVEL {
                        return Some(next_below);
                    }
                }

                // Consider `next` as a same-layer leveling target.
                let eligible = if under_pressure {
                    nv < MAX_WATER_LEVEL
                } else {
                    origin_vol >= nv.saturating_add(2)
                };
                if eligible {
                    match best_same_layer {
                        None => best_same_layer = Some((next, nv)),
                        Some((_, best_v)) if nv < best_v => best_same_layer = Some((next, nv)),
                        _ => {}
                    }
                }

                // Only continue BFS traversal through cells that actually contain water
                // (prevents teleporting across dry air barriers).
                if nv > 0 {
                    queue.push_back(next);
                }
            }
        }

        best_same_layer.map(|(p, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple bounded 3D grid for deterministic unit tests.
    struct GridWorld {
        cells: HashMap<Pos, Cell>,
    }

    impl GridWorld {
        fn new() -> Self {
            Self {
                cells: HashMap::new(),
            }
        }

        fn set_solid(&mut self, pos: Pos) {
            self.cells.insert(pos, Cell::Solid);
        }

        fn set_air(&mut self, pos: Pos) {
            self.cells.insert(pos, Cell::Air);
        }

        fn set_water(&mut self, pos: Pos, level: u8) {
            self.cells.insert(pos, Cell::from_volume(level));
        }

        /// Create a walled box with interior `0..w`, `0..h`, `0..d` surrounded by `Solid`.
        fn crater(w: i32, h: i32, d: i32) -> Self {
            let mut world = Self::new();
            for x in -1..=w {
                for y in -1..=h {
                    for z in -1..=d {
                        let inside = (0..w).contains(&x)
                            && (0..h).contains(&y)
                            && (0..d).contains(&z);
                        if inside {
                            world.set_air((x, y, z));
                        } else if y < h {
                            world.set_solid((x, y, z));
                        } else {
                            // Open sky above the crater rim.
                            world.set_air((x, y, z));
                        }
                    }
                }
            }
            world
        }

        fn total_water_volume(&self) -> usize {
            self.cells
                .values()
                .map(|c| c.water_volume() as usize)
                .sum()
        }
    }

    impl FluidWorld for GridWorld {
        fn get(&self, pos: Pos) -> Option<Cell> {
            self.cells.get(&pos).copied()
        }

        fn set(&mut self, pos: Pos, cell: Cell) {
            self.cells.insert(pos, cell);
        }
    }

    #[test]
    fn gravity_drops_vertical_column_to_floor() {
        let mut world = GridWorld::crater(1, 6, 1);
        world.set_water((0, 5, 0), 8);

        let mut sim = FluidSimulator::new();
        sim.perturb_neighborhood((0, 5, 0));

        let cfg = SimConfig {
            flow_interval_ticks: 1,
            ..SimConfig::default()
        };

        for _ in 0..20 {
            sim.step(&mut world, &cfg);
        }

        assert_eq!(world.get((0, 0, 0)), Some(Cell::Water(8)));
        for y in 1..=5 {
            assert_eq!(world.get((0, y, 0)), Some(Cell::Air));
        }
        assert_eq!(world.total_water_volume(), 8);
    }

    #[test]
    fn finite_mode_conserves_total_volume_strictly() {
        let mut world = GridWorld::crater(5, 4, 5);
        // Pour 12 full source blocks (96 units) in an irregular stack.
        for x in 1..=2 {
            for z in 1..=2 {
                for y in 1..=3 {
                    world.set_water((x, y, z), 8);
                }
            }
        }
        let initial_volume = world.total_water_volume();
        assert_eq!(initial_volume, 96);

        let mut sim = FluidSimulator::new();
        for x in 1..=2 {
            for z in 1..=2 {
                for y in 1..=3 {
                    sim.perturb_neighborhood((x, y, z));
                }
            }
        }

        let cfg = SimConfig {
            mode: SimMode::Finite,
            flow_interval_ticks: 1,
            ..SimConfig::default()
        };

        for _ in 0..200 {
            sim.step(&mut world, &cfg);
            assert_eq!(
                world.total_water_volume(),
                initial_volume,
                "volume must be conserved on every single step"
            );
        }

        // All 96 units should sit on y = 0 across the 25 floor cells (3 or 4 per cell, max diff 1).
        let mut min_v = u8::MAX;
        let mut max_v = 0u8;
        for x in 0..5 {
            for z in 0..5 {
                let v = world.get((x, 0, z)).unwrap().water_volume();
                min_v = min_v.min(v);
                max_v = max_v.max(v);
                for y in 1..4 {
                    assert_eq!(world.get((x, y, z)), Some(Cell::Air));
                }
            }
        }
        assert!(
            max_v - min_v <= 1,
            "surface must be flat within 1 discrete unit (min={min_v}, max={max_v})"
        );
    }

    #[test]
    fn crater_5x5x3_fills_completely_and_levels_into_flat_lake_in_finite_mode() {
        // 5x5x3 crater: 25 cells per layer. Pouring 50 full source blocks (400 units)
        // from above should fill y=0 and y=1 completely to level 8, leaving y=2 empty.
        let mut world = GridWorld::crater(5, 5, 5);
        let mut sim = FluidSimulator::new();
        let cfg = SimConfig {
            mode: SimMode::Finite,
            flow_interval_ticks: 1,
            max_updates_per_tick: 256,
            ..SimConfig::default()
        };

        // Drip 50 full buckets into the center column (2, 4, 2) over time.
        let mut poured = 0usize;
        for _ in 0..600 {
            if poured < 50 && world.get((2, 4, 2)) == Some(Cell::Air) {
                world.set_water((2, 4, 2), 8);
                sim.perturb_neighborhood((2, 4, 2));
                poured += 1;
            }
            sim.step(&mut world, &cfg);
        }
        assert_eq!(poured, 50);
        assert_eq!(world.total_water_volume(), 400);

        // Verify y=0 and y=1 are 100% full level-8 water blocks (a flat 2-deep lake).
        for y in 0..2 {
            for x in 0..5 {
                for z in 0..5 {
                    assert_eq!(
                        world.get((x, y, z)),
                        Some(Cell::Water(8)),
                        "expected full water at ({x}, {y}, {z})"
                    );
                }
            }
        }
        for x in 0..5 {
            for z in 0..5 {
                assert_eq!(world.get((x, 2, z)), Some(Cell::Air));
            }
        }
    }

    #[test]
    fn spring_mode_fills_5x5x3_crater_to_source_height_and_stops() {
        let mut world = GridWorld::crater(5, 3, 5);
        let spring_pos = (2, 2, 2);
        world.set_water(spring_pos, 8);

        let mut sim = FluidSimulator::new();
        sim.register_spring(spring_pos);

        let cfg = SimConfig {
            mode: SimMode::Spring,
            flow_interval_ticks: 1,
            max_updates_per_tick: 256,
            spring_max_blocks: 100, // crater holds 5*5*3 = 75 blocks
            ..SimConfig::default()
        };

        for _ in 0..400 {
            sim.step(&mut world, &cfg);
        }

        // All 75 cells in the 5x5x3 crater should be full level-8 water.
        for y in 0..3 {
            for x in 0..5 {
                for z in 0..5 {
                    assert_eq!(
                        world.get((x, y, z)),
                        Some(Cell::Water(8)),
                        "expected full lake at ({x}, {y}, {z})"
                    );
                }
            }
        }
        assert_eq!(world.total_water_volume(), 75 * 8);

        // One more step should do nothing (settled lake).
        let before = world.total_water_volume();
        sim.step(&mut world, &cfg);
        assert_eq!(world.total_water_volume(), before);
    }

    #[test]
    fn bottom_spring_rises_layer_by_layer_and_stops_at_crater_rim() {
        // 5x5x3 crater with open sky at y=3 (not enclosed).
        // Spring placed at the BOTTOM (2, 0, 2) should rise through y=0, y=1, y=2
        // and stop cleanly at the rim without overflowing into y=3.
        let mut world = GridWorld::crater(5, 3, 5);
        let spring_pos = (2, 0, 2);
        world.set_water(spring_pos, 8);

        let mut sim = FluidSimulator::new();
        sim.register_spring(spring_pos);

        let cfg = SimConfig {
            mode: SimMode::Finite, // natural hidden spring works even in Finite mode!
            flow_interval_ticks: 1,
            max_updates_per_tick: 256,
            spring_max_blocks: 200,
            ..SimConfig::default()
        };

        for _ in 0..500 {
            sim.step(&mut world, &cfg);
        }

        for y in 0..3 {
            for x in 0..5 {
                for z in 0..5 {
                    assert_eq!(
                        world.get((x, y, z)),
                        Some(Cell::Water(8)),
                        "expected bottom spring to fill ({x}, {y}, {z})"
                    );
                }
            }
        }
        // Must not overflow above the rim (y=3 is open sky).
        assert_eq!(world.get((2, 3, 2)), Some(Cell::Air));
    }

    #[test]
    fn respects_max_updates_per_tick_budget() {
        let mut world = GridWorld::crater(10, 4, 10);
        let mut sim = FluidSimulator::new();

        for x in 0..10 {
            for z in 0..10 {
                world.set_water((x, 3, z), 8);
                sim.enqueue((x, 3, z));
            }
        }
        assert_eq!(sim.active_count(), 100);

        let cfg = SimConfig {
            max_updates_per_tick: 15,
            flow_interval_ticks: 1,
            ..SimConfig::default()
        };

        let processed = sim.step(&mut world, &cfg);
        assert_eq!(processed, 15, "must never exceed max_updates_per_tick");
        assert!(sim.active_count() > 0, "remaining cells stay queued for next ticks");
    }

    #[test]
    fn ocean_protection_leaves_unperturbed_water_dormant() {
        let mut world = GridWorld::crater(6, 4, 6);
        // Natural ocean suspended at y=2 (would fall if simulated).
        for x in 0..6 {
            for z in 0..6 {
                world.set_water((x, 2, z), 8);
            }
        }

        let mut sim = FluidSimulator::new();
        let cfg = SimConfig {
            flow_interval_ticks: 1,
            ..SimConfig::default()
        };

        // No player perturbation -> 0 updates, ocean stays untouched.
        let processed = sim.step(&mut world, &cfg);
        assert_eq!(processed, 0);
        assert_eq!(world.get((2, 2, 2)), Some(Cell::Water(8)));
        assert_eq!(world.get((2, 1, 2)), Some(Cell::Air));
    }
}
