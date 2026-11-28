//! Playback compressor presets and metering for mpv's labeled lavfi filters.

pub(super) const INPUT_METADATA: &str = "af-metadata/comp_in";
pub(super) const OUTPUT_METADATA: &str = "af-metadata/comp_out";
const ASTATS: &str = "astats=metadata=1:reset=1:length=0.1:measure_perchannel=0:measure_overall=RMS_level+Peak_level";
const LIMITER: &str = ",alimiter=limit=0.95:attack=2:release=50:level=false:latency=true";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Strength {
    Soft,
    #[default]
    Medium,
    Strong,
}

#[derive(Clone, Copy)]
pub struct Preset {
    pub strength: Strength,
    pub id: &'static str,
    pub label: &'static str,
    threshold_db: f32,
    ratio: u32,
    attack_ms: u32,
    release_ms: u32,
    knee: u32,
    makeup_linear: f32,
}

// In Strength order, so the same preset supplies both DSP and metering.
pub const PRESETS: [Preset; 3] = [
    Preset {
        strength: Strength::Soft,
        id: "soft",
        label: "Soft",
        threshold_db: -16.0,
        ratio: 3,
        attack_ms: 8,
        release_ms: 180,
        knee: 4,
        makeup_linear: 1.188_502_2, // +1.5 dB
    },
    Preset {
        strength: Strength::Medium,
        id: "medium",
        label: "Medium",
        threshold_db: -20.0,
        ratio: 6,
        attack_ms: 5,
        release_ms: 160,
        knee: 4,
        makeup_linear: 1.25,
    },
    Preset {
        strength: Strength::Strong,
        id: "strong",
        label: "Strong",
        threshold_db: -26.0,
        ratio: 10,
        attack_ms: 3,
        release_ms: 140,
        knee: 6,
        makeup_linear: 1.5,
    },
];

impl Strength {
    pub fn parse(id: &str) -> Self {
        PRESETS
            .iter()
            .find(|preset| preset.id == id)
            .map(|preset| preset.strength)
            .unwrap_or_default()
    }

    pub fn id(self) -> &'static str {
        self.preset().id
    }

    pub fn label(self) -> &'static str {
        self.preset().label
    }

    fn preset(self) -> Preset {
        PRESETS[self as usize]
    }
}

pub(super) fn build_af_chain(eq_filter: &str, compressor: Option<Strength>) -> String {
    let Some(strength) = compressor else {
        return eq_filter.to_owned();
    };
    let Preset {
        threshold_db,
        ratio,
        attack_ms,
        release_ms,
        knee,
        makeup_linear,
        ..
    } = strength.preset();
    // FFmpeg's threshold and makeup are amplitude multipliers.
    let threshold = 10_f32.powf(threshold_db / 20.0);
    let eq = if eq_filter.is_empty() {
        String::new()
    } else {
        format!("@eq:lavfi=[{eq_filter}],")
    };
    format!(
        "{eq}@comp_in:lavfi=[{ASTATS}],@comp:lavfi=[acompressor=threshold={threshold}:ratio={ratio}:attack={attack_ms}:release={release_ms}:knee={knee}:makeup={makeup_linear}{LIMITER}],@comp_out:lavfi=[{ASTATS}]"
    )
}

fn parse_astats_db(metadata: &str) -> Option<f32> {
    let metadata: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let rms = metadata.get("lavfi.astats.Overall.RMS_level")?;
    let db = match rms {
        serde_json::Value::String(v) => v.parse().ok()?,
        _ => rms.as_f64()? as f32,
    };
    db.is_finite().then_some(db)
}

fn compute_reduction_db(input: f32, output: f32, strength: Strength) -> f32 {
    if input <= -70.0 {
        return 0.0;
    }
    let makeup_db = 20.0 * strength.preset().makeup_linear.log10();
    (input - (output - makeup_db)).clamp(0.0, 30.0)
}

pub(super) fn measure_reduction_db(input: &str, output: &str, strength: Strength) -> Option<f32> {
    Some(compute_reduction_db(
        parse_astats_db(input)?,
        parse_astats_db(output)?,
        strength,
    ))
}

pub(super) fn smooth_reduction_db(previous: f32, measured: f32) -> f32 {
    let weight = if measured > previous { 0.65 } else { 0.25 };
    previous + (measured - previous) * weight
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_round_trips_and_defaults_to_medium() {
        for preset in PRESETS {
            assert_eq!(Strength::parse(preset.id), preset.strength);
            assert_eq!(preset.strength.id(), preset.id);
            assert_eq!(preset.strength.label(), preset.label);
        }
        assert_eq!(Strength::parse(""), Strength::Medium);
        assert_eq!(Strength::parse("unknown"), Strength::Medium);
    }

    #[test]
    fn filter_chain_preserves_eq_and_only_meters_when_enabled() {
        let eq = "equalizer=f=60:width_type=o:w=1.5:g=6,equalizer=f=150:width_type=o:w=1.5:g=4";
        assert_eq!(build_af_chain("", None), "");
        assert_eq!(build_af_chain(eq, None), eq);
        for preset in PRESETS {
            let strength = Some(preset.strength);
            let flat = build_af_chain("", strength);
            assert!(flat.starts_with("@comp_in:lavfi=[astats="));
            assert!(flat.contains("],@comp:lavfi=[acompressor="));
            assert!(flat.ends_with(&format!("],@comp_out:lavfi=[{ASTATS}]")));
            assert!(flat.contains(LIMITER));
            assert_eq!(
                build_af_chain(eq, strength),
                format!("@eq:lavfi=[{eq}],{flat}")
            );
        }
    }

    #[test]
    fn astats_reads_rms_and_rejects_invalid_levels() {
        assert_eq!(
            parse_astats_db(
                r#"{"lavfi.astats.Overall.RMS_level":"-23.062111","lavfi.astats.Overall.Peak_level":"-20"}"#
            ),
            Some(-23.062_11)
        );
        assert_eq!(
            parse_astats_db(r#"{"lavfi.astats.Overall.RMS_level":-22.0}"#),
            Some(-22.0)
        );
        for invalid in [
            "",
            "{}",
            r#"{"lavfi.astats.Overall.RMS_level":"-inf"}"#,
            r#"{"lavfi.astats.Overall.RMS_level":"NaN"}"#,
        ] {
            assert_eq!(parse_astats_db(invalid), None);
            assert_eq!(
                measure_reduction_db(invalid, invalid, Strength::Medium),
                None
            );
        }
    }

    #[test]
    fn reduction_excludes_makeup_and_clamps() {
        let strength = Strength::Medium;
        let makeup_db = 20.0 * strength.preset().makeup_linear.log10();
        let meta = |db| format!(r#"{{"lavfi.astats.Overall.RMS_level":"{db}"}}"#);
        assert_eq!(
            measure_reduction_db(&meta(-50.0), &meta(-50.0 + makeup_db), strength),
            Some(0.0)
        );
        let reduction =
            measure_reduction_db(&meta(-6.0), &meta(-13.0 + makeup_db), strength).unwrap();
        assert!((reduction - 7.0).abs() < 0.001);
        assert_eq!(compute_reduction_db(-70.0, -120.0, strength), 0.0);
        assert_eq!(compute_reduction_db(-10.0, -80.0, strength), 30.0);
        assert_eq!(compute_reduction_db(-10.0, -5.0, strength), 0.0);
    }

    #[test]
    fn smoothing_rises_faster_than_it_falls() {
        assert_eq!(smooth_reduction_db(0.0, 8.0), 5.2);
        assert_eq!(smooth_reduction_db(8.0, 0.0), 6.0);
        assert_eq!(smooth_reduction_db(6.0, 6.0), 6.0);
    }
}
