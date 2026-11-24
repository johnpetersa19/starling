//! Configuration for Crater Water (`config/crater_water.json`).
//!
//! Zero-dependency JSON serializer and parser so the native mod requires no
//! external crates.io dependencies.

use crate::fluid_sim::{SimConfig, SimMode};

pub const CONFIG_FILE_NAME: &str = "crater_water.json";

/// Default JSON written to `config/crater_water.json` when the file is missing.
pub fn default_config_json() -> String {
    SimConfig::default().to_json()
}

impl SimConfig {
    /// Serialize the configuration as pretty-printed JSON.
    pub fn to_json(&self) -> String {
        let mode_str = match self.mode {
            SimMode::Finite => "finite",
            SimMode::Spring => "spring",
        };
        format!(
            "{{\n  \"enabled\": {},\n  \"mode\": \"{}\",\n  \"max_updates_per_tick\": {},\n  \"spring_max_blocks\": {},\n  \"flow_interval_ticks\": {},\n  \"pressure_search_limit\": {}\n}}\n",
            self.enabled,
            mode_str,
            self.max_updates_per_tick,
            self.spring_max_blocks,
            self.flow_interval_ticks,
            self.pressure_search_limit,
        )
    }

    /// Parse a flat JSON object into `SimConfig`, keeping defaults for any
    /// omitted fields and clamping numeric bounds to safe ranges.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let inner = trimmed
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .ok_or_else(|| "config JSON must be a top-level object".to_string())?;

        let mut cfg = Self::default();
        for entry in split_json_entries(inner) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (raw_key, raw_val) = entry
                .split_once(':')
                .ok_or_else(|| format!("malformed JSON entry: {entry:?}"))?;
            let key = unquote(raw_key.trim())
                .ok_or_else(|| format!("expected quoted key: {raw_key:?}"))?;
            let val = raw_val.trim();

            match key {
                "enabled" => {
                    cfg.enabled = parse_bool(val)
                        .ok_or_else(|| format!("invalid boolean for enabled: {val:?}"))?;
                }
                "mode" => {
                    let s = unquote(val)
                        .ok_or_else(|| format!("expected string for mode: {val:?}"))?;
                    cfg.mode = match s.to_ascii_lowercase().as_str() {
                        "finite" => SimMode::Finite,
                        "spring" => SimMode::Spring,
                        other => return Err(format!("unknown mode {other:?} (expected finite or spring)")),
                    };
                }
                "max_updates_per_tick" => {
                    let n = val
                        .parse::<usize>()
                        .map_err(|_| format!("invalid integer for max_updates_per_tick: {val:?}"))?;
                    cfg.max_updates_per_tick = n.clamp(1, 16_384);
                }
                "spring_max_blocks" => {
                    let n = val
                        .parse::<usize>()
                        .map_err(|_| format!("invalid integer for spring_max_blocks: {val:?}"))?;
                    cfg.spring_max_blocks = n.clamp(1, 262_144);
                }
                "flow_interval_ticks" => {
                    let n = val
                        .parse::<u32>()
                        .map_err(|_| format!("invalid integer for flow_interval_ticks: {val:?}"))?;
                    cfg.flow_interval_ticks = n.clamp(1, 40);
                }
                "pressure_search_limit" => {
                    let n = val
                        .parse::<usize>()
                        .map_err(|_| format!("invalid integer for pressure_search_limit: {val:?}"))?;
                    cfg.pressure_search_limit = n.clamp(8, 4_096);
                }
                _ => {
                    // Ignore unknown keys for forward compatibility.
                }
            }
        }
        Ok(cfg)
    }
}

fn split_json_entries(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                ',' => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
    }
    if start <= s.len() {
        out.push(&s[start..]);
    }
    out
}

fn unquote(s: &str) -> Option<&str> {
    s.strip_prefix('"').and_then(|rest| rest.strip_suffix('"'))
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_config_json() {
        let cfg = SimConfig::default();
        let json = cfg.to_json();
        let parsed = SimConfig::from_json(&json).expect("valid default json");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn parses_spring_mode_and_clamps_bounds() {
        let json = r#"{
            "enabled": false,
            "mode": "spring",
            "max_updates_per_tick": 999999,
            "spring_max_blocks": 512,
            "flow_interval_ticks": 0
        }"#;
        let parsed = SimConfig::from_json(json).expect("valid json");
        assert!(!parsed.enabled);
        assert_eq!(parsed.mode, SimMode::Spring);
        assert_eq!(parsed.max_updates_per_tick, 16_384);
        assert_eq!(parsed.spring_max_blocks, 512);
        assert_eq!(parsed.flow_interval_ticks, 1);
    }

    #[test]
    fn rejects_invalid_mode() {
        let err = SimConfig::from_json(r#"{ "mode": "ocean" }"#).unwrap_err();
        assert!(err.contains("unknown mode"));
    }
}
