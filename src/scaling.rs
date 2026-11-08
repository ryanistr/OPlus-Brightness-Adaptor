use crate::constants::{MIN_VISIBLE_HW, STATE_CHANGE_LEVEL};

/// Calculates perceptual gamma exponent based on panel dynamic range.
///
/// Disables secondary compounding (gamma 1.0) on standard 511-step panels (typically AMOLED).
/// Increases gamma up to 1.51 on 4096-step panels (typically high-bit IPS hardware backlights) to avoid mid-range blowout.
pub fn compute_dynamic_gamma(hw_max: i32) -> f32 {
    let bits = (hw_max.max(1) as f32).log2();
    if bits <= 9.0 {
        1.0
    } else {
        (1.0 + (bits - 9.0) * 0.17).clamp(1.0, 1.8)
    }
}

/// Dynamically calculates the minimum visible hardware brightness floor.
///
/// On standard 511-step AMOLED panels, hardware level 5 (~0.98%) is the visible floor.
/// On 4096-step IPS panels, hardware level 10 or below is equivalent to completely OFF.
/// By proportionally predicting the equivalent of baseline level 5 across different dynamic ranges:
/// - hw_max = 511  -> 5
/// - hw_max = 1023 -> 10
/// - hw_max = 2047 -> 20
/// - hw_max = 4096 -> 40
pub fn compute_min_visible_hw(hw_max: i32) -> i32 {
    let min = ((hw_max.max(1) as f32) * (MIN_VISIBLE_HW as f32 / 511.0)).round() as i32;
    min.max(1)
}

/// Dynamically calculates the intermediate level written during screen state transitions.
///
/// Ensures the intermediate pulse level remains visibly illuminated and above the minimum floor:
/// - hw_max = 511  -> 10
/// - hw_max = 4096 -> 80
pub fn compute_state_change_level(hw_max: i32) -> i32 {
    let level = ((hw_max.max(1) as f32) * (STATE_CHANGE_LEVEL as f32 / 511.0)).round() as i32;
    level.max(2)
}

pub(crate) fn scale_brightness_curved(
    val: i32,
    hw_min: i32,
    hw_max: i32,
    input_min: i32,
    input_max: i32,
) -> i32 {
    let effective_min = hw_min.max(compute_min_visible_hw(hw_max));
    if val <= input_min { return effective_min; }
    if val >= input_max { return hw_max; }

    let range_input = (input_max - input_min) as f32;
    let range_hw = (hw_max - effective_min) as f32;
    let ratio = (val - input_min) as f32 / range_input;

    let gamma = compute_dynamic_gamma(hw_max);
    let curve = ratio.powf(gamma);

    (effective_min as f32 + curve * range_hw).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_min_visible_hw() {
        assert_eq!(compute_min_visible_hw(255), 2);
        assert_eq!(compute_min_visible_hw(511), 5);
        assert_eq!(compute_min_visible_hw(1023), 10);
        assert_eq!(compute_min_visible_hw(2047), 20);
        assert_eq!(compute_min_visible_hw(4095), 40);
        assert_eq!(compute_min_visible_hw(4096), 40);
        assert_eq!(compute_min_visible_hw(8191), 80);
    }

    #[test]
    fn test_compute_state_change_level() {
        assert_eq!(compute_state_change_level(255), 5);
        assert_eq!(compute_state_change_level(511), 10);
        assert_eq!(compute_state_change_level(1023), 20);
        assert_eq!(compute_state_change_level(2047), 40);
        assert_eq!(compute_state_change_level(4096), 80);
    }

    #[test]
    fn test_dynamic_gamma_scaling() {
        let g255 = compute_dynamic_gamma(255);
        assert_eq!(g255, 1.0, "g255 = {}", g255);

        let g511 = compute_dynamic_gamma(511);
        assert_eq!(g511, 1.0, "g511 = {}", g511);

        let g1023 = compute_dynamic_gamma(1023);
        assert!((g1023 - 1.17).abs() < 0.02, "g1023 = {}", g1023);

        let g2047 = compute_dynamic_gamma(2047);
        assert!((g2047 - 1.34).abs() < 0.02, "g2047 = {}", g2047);

        let g4095 = compute_dynamic_gamma(4095);
        assert!((g4095 - 1.51).abs() < 0.02, "g4095 = {}", g4095);

        let g4096 = compute_dynamic_gamma(4096);
        assert!((g4096 - 1.51).abs() < 0.02, "g4096 = {}", g4096);

        let g8191 = compute_dynamic_gamma(8191);
        assert!((g8191 - 1.68).abs() < 0.02, "g8191 = {}", g8191);
    }

    #[test]
    fn test_scale_curved_511_vs_4096() {
        // At 35% on 511 panel: gamma is 1.0 (linear), preserves ideal 511 curve
        let val_35_511 = 222 + (0.35 * (8191 - 222) as f32) as i32;
        let out_511 = scale_brightness_curved(val_35_511, 1, 511, 222, 8191);
        // Ratio = 0.35 -> 5 + 0.35 * 506 = 182
        assert_eq!(out_511, 182);

        // At 35% on 4096 panel: gamma is 1.51, prevents mid-range blowout, dynamic floor = 40
        let val_35_4096 = 222 + (0.35 * (8191 - 222) as f32) as i32;
        let out_4096 = scale_brightness_curved(val_35_4096, 1, 4096, 222, 8191);
        // Ratio = 0.35^1.51 ~ 0.2049 -> 40 + 0.2049 * (4096 - 40) = 40 + 831 = 871
        assert_eq!(out_4096, 871);
    }
}
