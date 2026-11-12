#![allow(dead_code)]

use std::os::raw::c_int;

pub(crate) const LOG_DEBUG: c_int = 3;
pub(crate) const LOG_ERROR: c_int = 6;
pub(crate) const FALLBACK_MAX: i32 = 8191;
pub(crate) const FALLBACK_MIN: i32 = 222;
pub(crate) const BRIGHTNESS_OFF: i32 = 0;
pub(crate) const OS14_MAX: i32 = 5118;
pub(crate) const OS14_MIN: i32 = 22;

/// Intermediate level written while the screen state changes, before the
/// target level.
///
/// Writing it first makes both writes a real value change, so the kernel's
/// LED core applies each one instead of skipping the target as a repeat of the
/// level it cached while the panel was off.
pub(crate) const STATE_CHANGE_LEVEL: i32 = 10;

/// Gap between the intermediate level and the target level.
///
/// The panel driver takes the backlight through a command queue, so two writes
/// in the same instant collapse into one and only the last survives. The gap
/// makes them two separate commands.
pub(crate) const STATE_CHANGE_DELAY_MS: u64 = 30;

pub const SCREEN_STATE_OFF: i32 = 0;
pub const SCREEN_STATE_OFF_AOD: i32 = 1;
pub const SCREEN_STATE_ON: i32 = 2;
pub const SCREEN_STATE_DOZE: i32 = 3;
pub const SCREEN_STATE_DOZE_SUSPEND: i32 = 4;

pub const SERVICE_NAME: &str =
    "vendor.oplus.hardware.displaypanelfeature.IDisplayPanelFeature/default";
pub const PANEL_ID_SLOTS: usize = 5;
pub const ADDON_DIR: &str = "/data/addon/oplus_display";

pub mod feature {
    pub const SILENCE_FLAG: i32 = 0x0;
    pub const MEMC: i32 = 0x1;
    pub const POWER: i32 = 0x3;
    pub const SEED: i32 = 0x4;
    pub const PANEL_ID: i32 = 0x5;
    pub const FFL: i32 = 0x6;
    pub const AOD: i32 = 0x7;
    pub const MAX_BRIGHTNESS: i32 = 0x8;
    pub const PANEL_INFO_MANUFACTURER: i32 = 0x9;
    pub const PANEL_INFO_PRODUCTION: i32 = 0xb;
    pub const HBM: i32 = 0xc;
    pub const DIM_ALPHA: i32 = 0xd;
    pub const DIM_DC_ALPHA: i32 = 0xe;
    pub const DUMP_INFO: i32 = 0x10;
    pub const DSC: i32 = 0x11;
    pub const POWER_STATUS: i32 = 0x12;
    pub const CLOSE_BL_FLAG: i32 = 0x14;
    pub const REG: i32 = 0x15;
    pub const DIMLAY_HBM: i32 = 0x16;
    pub const DIMLAYER_BL_EN: i32 = 0x17;
    pub const SPR: i32 = 0x19;
    pub const ROUND_CORNER: i32 = 0x1a;
    pub const DOC: i32 = 0x1b;
    pub const FP_PRESS: i32 = 0x1c;
    pub const O_BRIGHTNESS: i32 = 0x1d;
    pub const LCD_MAX_BRIGHTNESS: i32 = 0x1e;
    pub const ROUND_CORNER_R: i32 = 0x20;
    pub const LCD_MAX_BRIGHTNESS_ALT: i32 = 0x21;
    pub const KERNEL_LOG_SWITCH: i32 = 0x43;
    pub const PQ_TRIGGER: i32 = 0x44;
    pub const BPP: i32 = 0x45;
    pub const PANEL_INFO_DEV_INFO: i32 = 0x53;
    pub const CABC_STATUS: i32 = 0xb8;
    pub const DRE_STATUS: i32 = 0xb9;
    pub const VIDEO: i32 = 0xc1;
    pub const DYNAMIC_TE: i32 = 0xc2;
    pub const ULTRA_LOW_POWER_AOD: i32 = 0xc3;
    pub const IRIS_LOOPBACK: i32 = 0xc6;
    pub const PWM_TURBO: i32 = 0xc7;
    pub const PWM_PULSE: i32 = 0xc9;
    pub const SHUTDOWN_FLAG: i32 = 0xd2;
    pub const FP_TYPE: i32 = 0xd3;
    pub const FAKE_AOD: i32 = 0xd5;
    pub const PANEL_TYPE: i32 = 0xd6;
    pub const HBM_MAX: i32 = 0xd8;
    pub const DC_COMPENSATE: i32 = 0xdc;
    pub const FOSS: i32 = 0xe1;
    pub const UIR: i32 = 0xe2;
    pub const DEMURA: i32 = 0xf1;
    pub const PANEL_INFO_DMR: i32 = 0xf4;
    pub const BRIGHTNESS_MODE: i32 = 0xf5;
    pub const ISU_LOG: i32 = 0xf6;
    pub const SUNLIGHT_MODE: i32 = 0xf7;
}

pub fn split_feature_id(id: i32) -> (i32, i32) {
    (id >> 0xc, id & 0xfff)
}

pub fn feature_to_filename(feature: i32) -> Option<&'static str> {
    match feature {
        feature::SEED => Some("seed"),
        feature::PANEL_ID => Some("panel_id"),
        feature::FFL => Some("ffl_set"),
        feature::AOD => Some("aod_light_mode"),
        feature::HBM => Some("hbm"),
        feature::DIM_ALPHA => Some("dim_alpha"),
        feature::DIM_DC_ALPHA => Some("dim_dc_alpha"),
        feature::DUMP_INFO => Some("dump_info"),
        feature::DSC => Some("dsc"),
        feature::POWER_STATUS => Some("power_status"),
        feature::CLOSE_BL_FLAG => Some("close_bl_flag"),
        feature::REG => Some("reg"),
        feature::DIMLAY_HBM => Some("dimlayer_hbm"),
        feature::DIMLAYER_BL_EN => Some("dimlayer_bl_en"),
        feature::SPR => Some("spr"),
        feature::ROUND_CORNER => Some("roundcorner"),
        feature::DOC => Some("doc"),
        feature::FP_PRESS => Some("notify_fppress"),
        feature::O_BRIGHTNESS => Some("oplus_brightness"),
        feature::LCD_MAX_BRIGHTNESS | feature::LCD_MAX_BRIGHTNESS_ALT => {
            Some("oplus_max_brightness")
        }
        feature::MAX_BRIGHTNESS => Some("max_brightness"),
        feature::ROUND_CORNER_R => Some("roundcorner_r"),
        feature::KERNEL_LOG_SWITCH => Some("kernel_log_switch"),
        feature::PQ_TRIGGER => Some("pq_trigger"),
        feature::BPP => Some("bpp"),
        feature::CABC_STATUS => Some("cabc_status"),
        feature::DRE_STATUS => Some("dre_status"),
        feature::VIDEO => Some("video"),
        feature::DYNAMIC_TE => Some("dynamic_te"),
        feature::ULTRA_LOW_POWER_AOD => Some("ultra_low_power_aod"),
        feature::IRIS_LOOPBACK => Some("iris_loopback"),
        feature::PWM_TURBO => Some("pwm_turbo"),
        feature::PWM_PULSE => Some("pwm_pulse"),
        feature::SHUTDOWN_FLAG => Some("shutdown_flag"),
        feature::FP_TYPE => Some("fp_type"),
        feature::FAKE_AOD => Some("fake_aod"),
        feature::PANEL_TYPE => Some("panel_type"),
        feature::HBM_MAX => Some("hbm_max"),
        feature::DEMURA => Some("demura"),
        feature::BRIGHTNESS_MODE => Some("brightness_mode"),
        feature::ISU_LOG => Some("isu_log"),
        feature::SUNLIGHT_MODE => Some("sunlight_mode"),
        feature::DC_COMPENSATE => Some("dc_compensate"),
        feature::FOSS => Some("foss"),
        feature::UIR => Some("uir"),
        _ => None,
    }
}
