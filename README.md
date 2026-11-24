# Crater Water (`crater_water`)

Native Rust mod (`.hmod`) for **Minecraft Horizon** (26.3) implementing **realistic volumetric water physics**.

## Features

- **Volumetric Water (levels 1–8):** Gravity-first flow, bottom-up crater filling, and basin pressure equalization to form flat lakes.
- **Finite Mode (default):** Strict volume conservation (disables 2×2 infinite water sources on managed water).
- **Spring Mode (`/craterwater spring`):** Player-placed water acts as a continuous spring, rising layer by layer inside enclosed craters and stopping automatically at the rim (with a configurable `spring_max_blocks` limit).
- **Hidden Natural Spring Recipe:** Prepare a crater bed or mountain source with **gravel / sand / red sand + stone** (`stone`, `cobblestone`, `deepslate`, `sandstone`, `andesite`, `granite`, `diorite`, `tuff`, etc.) and pour water on top — it springs naturally even in Finite mode!
- **Ocean & TPS Protection:** Natural oceans and rivers remain untouched unless perturbed by a player; simulation is bounded by `max_updates_per_tick` (default `256`).
- **In-game Config & Commands:**
  - **Options > Mods > Crater Water > Config** or `config/crater_water.json`
  - `/craterwater finite | spring | status | clear`

## Building

```sh
horizon-launcher native-build . --trust-native-code
```
