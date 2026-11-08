use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rsbinder::{BinderResult, Interface, SIBinder, Strong};

use crate::constants::{
    feature, split_feature_id, BRIGHTNESS_OFF, PANEL_ID_SLOTS, SERVICE_NAME,
};
use crate::logging::{log_d, log_e};
use crate::modes::dbg_on;
use crate::panel_bindings::vendor::oplus::hardware::displaypanelfeature::IDisplayPanelFeature::{
    BnDisplayPanelFeature, IDisplayPanelFeature,
};
use crate::range::BrightnessRange;
use crate::scaling::scale_brightness_curved;
use crate::state::{FeatureStore, Value};
use crate::writer::write_brightness;

pub struct Inner {
    pub store: FeatureStore,
    pub hw_min: i32,
    pub hw_max: i32,
    pub range: Arc<RwLock<BrightnessRange>>,
    pub last_hw_val: Mutex<i32>,
    pub last_raw_val: AtomicI32,
    pub has_brightness: AtomicBool,
    pub screen_state: AtomicI32,
    pub sysfs_fd: Option<i32>,
}

impl Inner {
    fn write_sysfs(&self, target_hw: i32, dbg: bool) {
        if let Some(fd) = self.sysfs_fd {
            if let Ok(mut last) = self.last_hw_val.lock() {
                write_brightness(fd, target_hw, &mut last, dbg);
            }
        }
    }

    /// Write the brightness, bypassing the writer's same-value skip.
    ///
    /// The panel re-initialises while it is off, so a write of the value it
    /// already had is skipped by the kernel's LED core and the panel stays
    /// dark. Forcing the write makes the driver re-apply the level.
    fn write_sysfs_forced(&self, target_hw: i32, dbg: bool) {
        if let Some(fd) = self.sysfs_fd {
            if let Ok(mut last) = self.last_hw_val.lock() {
                let prev = *last;
                *last = -1;
                write_brightness(fd, target_hw, &mut last, dbg);
                if *last < 0 {
                    *last = prev;
                }
            }
        }
    }

    /// Whether the panel is fully off.
    ///
    /// `debug.tracing.screen_state` reports 2 = ON, 3 = DOZE (AOD), 4 =
    /// DOZE_SUSPEND and 0/1 = OFF. Only 2 means the panel is lit for the user.
    pub fn is_screen_off(&self) -> bool {
        let state = self.screen_state.load(Ordering::SeqCst);
        state == crate::constants::SCREEN_STATE_OFF
            || state == crate::constants::SCREEN_STATE_OFF_AOD
    }

    /// The level to write when the panel is being turned off.
    ///
    /// The panel driver applies a zero write even while the panel is in its
    /// AOD mode, and that blanks the AOD image the driver lit itself when the
    /// doze state was entered. A non-zero write is dropped by the driver in
    /// that mode instead, so the AOD-capable states are floored at 1.
    fn off_level(&self) -> i32 {
        match self.screen_state.load(Ordering::SeqCst) {
            crate::constants::SCREEN_STATE_OFF_AOD
            | crate::constants::SCREEN_STATE_DOZE
            | crate::constants::SCREEN_STATE_DOZE_SUSPEND => crate::constants::AOD_MIN_HW,
            _ => BRIGHTNESS_OFF,
        }
    }

    /// Apply a screen state transition.
    ///
    /// The panel must not be left holding the brightness it had when the
    /// screen goes off: the panel re-initialises on the next wake, and the
    /// kernel's LED core skips a write whose value it already cached, so
    /// re-writing the same value leaves the panel dark. Going off therefore
    /// drops the panel to 0, and coming back to ON raises it to the brightness
    /// that was requested, as a real change the driver cannot skip.
    pub fn apply_screen_state(&self, state: i32, dbg: bool) {
        let prev = self.screen_state.swap(state, Ordering::SeqCst);
        if prev == state {
            return;
        }

        if state == crate::constants::SCREEN_STATE_ON {
            if dbg {
                log_d(&format!(
                    "[PanelService] screen state {} -> ON, raising brightness back",
                    prev
                ));
            }
            let raw = self.last_raw_val.load(Ordering::SeqCst);
            if self.has_brightness.load(Ordering::SeqCst) && raw > 0 {
                self.apply_brightness_forced(raw);
            }
            return;
        }

        if dbg {
            log_d(&format!(
                "[PanelService] screen state {} -> {}, dropping brightness",
                prev, state
            ));
        }

        if state == crate::constants::SCREEN_STATE_OFF_AOD
            || state == crate::constants::SCREEN_STATE_DOZE
            || state == crate::constants::SCREEN_STATE_DOZE_SUSPEND
        {
            // AOD keeps the panel lit; the framework drives the doze level.
            // Never write 0 here: the driver applies a zero write even in AOD
            // mode and that blanks the AOD image.
            self.write_sysfs(crate::constants::AOD_MIN_HW, dbg);
            return;
        }

        self.write_sysfs(BRIGHTNESS_OFF, dbg);
    }

    /// Apply the panel power state sent over the panel-feature AIDL.
    ///
    /// This encoding is the panel feature's own (0/3 = off, 1/2 = on), not
    /// `debug.tracing.screen_state`, so it is kept separate from that.
    pub fn apply_power_state(&self, on: bool, dbg: bool) {
        if on {
            if dbg {
                log_d("[PanelService] panel power on, raising brightness back");
            }
            let raw = self.last_raw_val.load(Ordering::SeqCst);
            if self.has_brightness.load(Ordering::SeqCst) && raw > 0 {
                self.apply_brightness(raw);
            }
        } else {
            if dbg {
                log_d("[PanelService] panel power off, dropping brightness");
            }
            self.write_sysfs(self.off_level(), dbg);
        }
    }

    pub fn apply_brightness(&self, raw: i32) {
        self.apply_brightness_inner(raw, false);
    }

    /// Apply the brightness, forcing the write through even when the value is
    /// the one the writer last wrote.
    ///
    /// The panel re-initialises while the screen is off, so the kernel's LED
    /// core drops a write whose value it already cached and the panel stays
    /// dark. This is the wake path.
    pub fn apply_brightness_forced(&self, raw: i32) {
        self.apply_brightness_inner(raw, true);
    }

    /// The levels to write, in order, when the brightness changes with the
    /// screen state.
    ///
    /// The panel re-initialises while it is off, so the level it is restored
    /// to can be the one it already had, and the kernel's LED core skips that
    /// write. Going through an intermediate level makes both writes a real
    /// value change, so the driver applies the target.
    pub fn state_change_levels(&self, target_hw: i32) -> [i32; 2] {
        [crate::scaling::compute_state_change_level(self.hw_max), target_hw]
    }

    fn apply_brightness_inner(&self, raw: i32, force: bool) {
        let dbg = dbg_on();
        self.last_raw_val.store(raw, Ordering::SeqCst);
        self.has_brightness.store(true, Ordering::SeqCst);

        if self.is_screen_off() {
            // The panel is off: keep it at the off level rather than applying
            // the user brightness, so the value it is restored to on wake
            // differs from the one it was left at.
            self.write_sysfs(self.off_level(), dbg);
            return;
        }

        let target_hw = if raw <= 0 {
            BRIGHTNESS_OFF
        } else {
            let range = *self.range.read().unwrap();
            scale_brightness_curved(raw, self.hw_min, self.hw_max, range.min, range.max)
        };

        if dbg {
            log_d(&format!(
                "[PanelService] apply_brightness: raw={} -> scaled hw={}",
                raw, target_hw
            ));
        }

        // The panel backlight node is a 1..511 scale (max_hw_brightness=511) and is what the
        // mediatek panel driver actually reads. The light HAL's backlight id is a 0..255 light, so
        // it cannot express the upper half of the panel range and is only a fallback.
        if self.sysfs_fd.is_some() {
            if force {
                let levels = self.state_change_levels(target_hw);
                for (i, level) in levels.iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(
                            crate::constants::STATE_CHANGE_DELAY_MS,
                        ));
                    }
                    self.write_sysfs_forced(*level, dbg);
                }
            } else {
                self.write_sysfs(target_hw, dbg);
            }
        } else {
            log_e("[PanelService] no backlight sysfs node is open; brightness not applied");
        }

        // The Oplus 11-bit brightness is tracked in last_raw_val; the OS14 addon file mirror is
        // not used on OS15+ and the write is rejected for the service uid.
        // Persist to disk so last known brightness survives reboot
        if raw > 0 {
            let _ = std::fs::write("/data/addon/oplus_display/last_brightness", format!("{}\n", raw));
        }
    }

    pub fn force_restore_brightness(&self, source: &str) {
        let dbg = dbg_on();

        // Invalidate cached hardware brightness so writer does not skip write
        if let Ok(mut last) = self.last_hw_val.lock() {
            *last = -1;
        }

        let mut target_raw = -1;

        // 1. Try the persistent file backup.
        if target_raw <= 0 {
            if let Ok(content) = std::fs::read_to_string("/data/addon/oplus_display/last_brightness") {
                if let Ok(val) = content.trim().parse::<i32>() {
                    if val > 0 {
                        target_raw = val;
                        if dbg {
                            log_d(&format!(
                                "[DisplayAdaptor] Restore ({}): fetched from /data/addon/oplus_display/last_brightness: raw={}",
                                source, target_raw
                            ));
                        }
                    }
                }
            }
        }

        // 2. Try in-memory last_raw_val
        if target_raw <= 0 {
            let val = self.last_raw_val.load(Ordering::SeqCst);
            if val > 0 {
                target_raw = val;
            }
        }

        // 3. Default range minimum
        if target_raw <= 0 {
            target_raw = self.range.read().map(|r| r.min).unwrap_or(crate::constants::FALLBACK_MIN);
        }

        log_d(&format!(
            "[DisplayAdaptor] Restoring brightness (source={}): raw={}",
            source, target_raw
        ));

        self.apply_brightness(target_raw);
    }
}

pub struct PanelService {
    pub inner: Arc<Inner>,
}

impl PanelService {
    pub fn new(
        hw_min: i32,
        hw_max: i32,
        range: Arc<RwLock<BrightnessRange>>,
        sysfs_fd: Option<i32>,
    ) -> (Self, Arc<Inner>) {
        let persisted_raw = std::fs::read_to_string("/data/addon/oplus_display/last_brightness")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .filter(|&v| v > 0);
        let has_initial = persisted_raw.is_some();
        let initial_raw = persisted_raw.unwrap_or_else(|| {
            range.read().map(|r| r.min).unwrap_or(crate::constants::FALLBACK_MIN)
        });
        let inner = Arc::new(Inner {
            store: FeatureStore::default(),
            hw_min,
            hw_max,
            range,
            last_hw_val: Mutex::new(-1),
            last_raw_val: AtomicI32::new(initial_raw),
            has_brightness: AtomicBool::new(has_initial),
            screen_state: AtomicI32::new(2), // 2 = ON
            sysfs_fd,
        });
        (Self { inner: Arc::clone(&inner) }, inner)
    }

    pub fn register(self) -> Result<Strong<dyn IDisplayPanelFeature>, rsbinder::Status> {
        let service: Strong<dyn IDisplayPanelFeature> = BnDisplayPanelFeature::new_binder(self);
        rsbinder::hub::add_service(SERVICE_NAME, service.as_binder())?;
        Ok(service)
    }
}

impl Interface for PanelService {
    fn as_binder(&self) -> SIBinder {
        panic!("PanelService is not a binder; wrap it with BnDisplayPanelFeature::new_binder")
    }
}

pub fn is_fp_feature(feature: i32) -> bool {
    matches!(
        feature,
        feature::FP_TYPE
            | feature::FP_PRESS
            | feature::DIMLAY_HBM
            | feature::DIMLAYER_BL_EN
            | 179
    )
}

impl IDisplayPanelFeature for PanelService {
    fn r#getDisplayPanelFeatureValue(
        &self,
        feature_id: i32,
        feature_values: &mut Vec<i32>,
    ) -> BinderResult<i32> {
        let (display, feature) = split_feature_id(feature_id);
        let slots = feature_values.len();
        let dbg = dbg_on();

        if is_fp_feature(feature) {
            if dbg {
                log_d(&format!(
                    "[PanelService] getDisplayPanelFeatureValue(0x{:x}) dropped (fingerprint feature; falling back to non-HAL path)",
                    feature
                ));
            }
            return Err(rsbinder::StatusCode::UnknownTransaction.into());
        }

        if feature == feature::MAX_BRIGHTNESS
            || feature == feature::LCD_MAX_BRIGHTNESS
            || feature == feature::LCD_MAX_BRIGHTNESS_ALT
        {
            if dbg {
                log_d(&format!(
                    "[PanelService] getDisplayPanelFeatureValue(MAX_BRIGHTNESS): {}",
                    self.inner.hw_max
                ));
            }
            let mut values = vec![self.inner.hw_max];
            values.resize(slots, 0);
            *feature_values = values;
            return Ok(0);
        }

        if feature == feature::O_BRIGHTNESS {
            let raw = self.inner.last_raw_val.load(Ordering::SeqCst);
            if dbg {
                log_d(&format!(
                    "[PanelService] getDisplayPanelFeatureValue(O_BRIGHTNESS): {}",
                    raw
                ));
            }
            let mut values = vec![raw];
            values.resize(slots, 0);
            *feature_values = values;
            return Ok(0);
        }

        if feature == feature::PANEL_ID {
            let mut produced = vec![0; PANEL_ID_SLOTS];
            produced.resize(slots, 0);
            *feature_values = produced;
            return Ok(0);
        }

        if let Some(value) = self.inner.store.get(display, feature) {
            let mut produced: Vec<i32> = match &value {
                Value::Ints(v) => v.clone(),
                Value::Int(v) => vec![*v],
                Value::Strings(_) => Vec::new(),
            };
            produced.resize(slots, 0);
            *feature_values = produced;

            if dbg {
                log_d(&format!(
                    "[PanelService] getDisplayPanelFeatureValue(0x{:x}) -> {:?}",
                    feature, feature_values
                ));
            }
            return Ok(0);
        }

        if dbg {
            log_d(&format!(
                "[PanelService] getDisplayPanelFeatureValue(0x{:x}) unsupported",
                feature
            ));
        }
        Err(rsbinder::StatusCode::UnknownTransaction.into())
    }

    fn r#setDisplayPanelFeatureValue(
        &self,
        feature_id: i32,
        feature_values: &[i32],
    ) -> BinderResult<i32> {
        let (display, feature) = split_feature_id(feature_id);
        let dbg = dbg_on();
        let val = feature_values.first().copied().unwrap_or(0);

        if is_fp_feature(feature) {
            if dbg {
                log_d(&format!(
                    "[PanelService] setDisplayPanelFeatureValue: id=0x{:x} (feature=0x{:x}) dropped (fingerprint feature; falling back to non-HAL path)",
                    feature_id, feature
                ));
            }
            return Err(rsbinder::StatusCode::UnknownTransaction.into());
        }

        if dbg {
            log_d(&format!(
                "[PanelService] setDisplayPanelFeatureValue: id=0x{:x} (feature=0x{:x}) values={:?}",
                feature_id, feature, feature_values
            ));
        }

        if feature == feature::O_BRIGHTNESS {
            self.inner.apply_brightness(val);
            return Ok(0);
        }

        if feature == feature::POWER
            || feature == feature::POWER_STATUS
            || feature == feature::CLOSE_BL_FLAG
        {
            // Panel-feature encoding: 0/3 = off, 1/2 = on.
            self.inner.apply_power_state(val == 1 || val == 2, dbg);
            self.inner.store.set(display, feature, &Value::Int(val));
            return Ok(0);
        }

        let value = if feature_values.len() == 1 {
            Value::Int(feature_values[0])
        } else {
            Value::Ints(feature_values.to_vec())
        };

        self.inner.store.set(display, feature, &value);
        Ok(0)
    }

    fn r#getDisplayPanelInfo(
        &self,
        feature_id: i32,
        panel_info: &mut Vec<String>,
    ) -> BinderResult<i32> {
        let (display, feature) = split_feature_id(feature_id);
        let mut info = self.inner.store.info(display, feature).unwrap_or_default();
        if info.is_empty() {
            info = default_panel_info(feature);
        }
        *panel_info = info;
        Ok(0)
    }
}

fn default_panel_info(feature: i32) -> Vec<String> {
    match feature {
        feature::PANEL_INFO_MANUFACTURER => vec!["OnePlus".to_string()],
        feature::PANEL_INFO_PRODUCTION => vec!["2024".to_string()],
        feature::PANEL_INFO_DEV_INFO => vec!["CPH2493".to_string()],
        feature::PANEL_INFO_DMR => vec![String::new()],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_max_brightness_returns_hw_max() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (service, _inner) = PanelService::new(1, 511, range, None);

        let mut values = vec![0i32; 1];
        assert_eq!(
            service.r#getDisplayPanelFeatureValue(feature::MAX_BRIGHTNESS, &mut values).unwrap(),
            0
        );
        assert_eq!(values, vec![511]);
    }

    #[test]
    fn get_panel_id_returns_5_slots() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (service, _inner) = PanelService::new(1, 511, range, None);

        let mut values = vec![0i32; 5];
        assert_eq!(
            service.r#getDisplayPanelFeatureValue(feature::PANEL_ID, &mut values).unwrap(),
            0
        );
        assert_eq!(values.len(), 5);
        assert_eq!(values, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn set_o_brightness_updates_store() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (service, inner) = PanelService::new(1, 511, range, None);

        assert_eq!(
            service.r#setDisplayPanelFeatureValue(feature::O_BRIGHTNESS, &[4000]).unwrap(),
            0
        );
        assert_eq!(inner.last_raw_val.load(Ordering::SeqCst), 4000);

        let mut values = vec![0i32; 1];
        assert_eq!(
            service.r#getDisplayPanelFeatureValue(feature::O_BRIGHTNESS, &mut values).unwrap(),
            0
        );
        assert_eq!(values, vec![4000]);
    }

    #[test]
    fn test_scaling_math() {
        assert_eq!(scale_brightness_curved(222, 1, 511, 222, 8191), 5);
        assert_eq!(scale_brightness_curved(8191, 1, 511, 222, 8191), 511);
        let mid = scale_brightness_curved(4000, 1, 511, 222, 8191);
        assert!(mid > 200 && mid < 260);
    }

    #[test]
    fn scaling_floor_is_five() {
        // hw_min below the floor is raised to dynamic floor (5 on 511), never lower.
        assert_eq!(scale_brightness_curved(222, 1, 511, 222, 8191), 5);
        // a hw_min at or above the floor is left alone
        assert_eq!(scale_brightness_curved(222, 10, 511, 222, 8191), 10);
        // on 4096, the floor is dynamically calculated as 40
        assert_eq!(scale_brightness_curved(222, 1, 4096, 222, 8191), 40);
        assert_eq!(scale_brightness_curved(222, 50, 4096, 222, 8191), 50);
    }

    #[test]
    fn state_change_writes_intermediate_level_first() {
        // The intermediate level goes first, then the target.
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (_service, inner) = PanelService::new(1, 511, range.clone(), None);
        assert_eq!(
            inner.state_change_levels(5),
            [10, 5]
        );
        let (_service_4096, inner_4096) = PanelService::new(1, 4096, range, None);
        assert_eq!(
            inner_4096.state_change_levels(40),
            [80, 40]
        );
        // The intermediate is above the floor, so it is a level the panel lights at.
        assert!(crate::scaling::compute_state_change_level(511) > crate::scaling::compute_min_visible_hw(511));
        assert!(crate::scaling::compute_state_change_level(4096) > crate::scaling::compute_min_visible_hw(4096));
    }

    #[test]
    fn screen_state_off_drops_brightness_and_on_restores_it() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (service, inner) = PanelService::new(1, 511, range, None);

        // A brightness request while the screen is on is stored.
        service
            .r#setDisplayPanelFeatureValue(feature::O_BRIGHTNESS, &[4000])
            .unwrap();
        assert_eq!(inner.last_raw_val.load(Ordering::SeqCst), 4000);

        // Going off must not leave the panel holding that brightness.
        inner.apply_screen_state(crate::constants::SCREEN_STATE_OFF, false);
        assert!(inner.is_screen_off());

        // Coming back on restores the requested brightness.
        inner.apply_screen_state(crate::constants::SCREEN_STATE_ON, false);
        assert!(!inner.is_screen_off());
        assert_eq!(inner.last_raw_val.load(Ordering::SeqCst), 4000);
    }

    #[test]
    fn doze_keeps_the_panel_lit() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (_service, inner) = PanelService::new(1, 511, range, None);
        inner.apply_screen_state(crate::constants::SCREEN_STATE_DOZE, false);
        assert!(!inner.is_screen_off());
    }

    #[test]
    fn fp_features_are_dropped_with_unknown_transaction() {
        let range = Arc::new(RwLock::new(BrightnessRange::new(222, 8191)));
        let (service, _inner) = PanelService::new(1, 511, range, None);

        let mut values = vec![0i32; 1];
        assert!(service
            .r#getDisplayPanelFeatureValue(feature::FP_TYPE, &mut values)
            .is_err());
        assert!(service
            .r#getDisplayPanelFeatureValue(feature::FP_PRESS, &mut values)
            .is_err());
        assert!(service
            .r#getDisplayPanelFeatureValue(feature::DIMLAY_HBM, &mut values)
            .is_err());
        assert!(service
            .r#setDisplayPanelFeatureValue(feature::FP_PRESS, &[1])
            .is_err());
        assert!(service
            .r#setDisplayPanelFeatureValue(feature::DIMLAY_HBM, &[1])
            .is_err());
    }
}
