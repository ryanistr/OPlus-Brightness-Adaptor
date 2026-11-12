#![allow(dead_code)]

use std::process::Command;
use crate::logging::{log_d, log_e};
use crate::properties::{get_prop_int, set_prop};
use crate::paths::{
    max_bright_fallback_path, max_bright_path, min_bright_path, persist_hw_max, persist_hw_min,
    persist_obright_max, persist_obright_min,
};

// panoramic aod check
pub(crate) fn is_panoramic_aod_enabled(dbg: bool) -> bool {
    let output = Command::new("settings")
                             .arg("get")
                             .arg("secure")
                             .arg("panoramic_aod_enable")
                             .output();

    match output {
        Ok(out) => {
            if out.status.success() {
                let result = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if dbg { log_d(&format!("[DisplayAdaptor] panoramic_aod_enable result: '{}'", result)); }
                result == "1"
            } else {
                if dbg { log_e(&format!("[DisplayAdaptor] 'settings get' command failed: {}", String::from_utf8_lossy(&out.stderr))); }
                false
            }
        },
        Err(e) => {
            if dbg { log_e(&format!("[DisplayAdaptor] Failed to execute 'settings' command: {}", e)); }
            false
        }
    }
}

// file & property readers
pub(crate) fn read_file_int(path: &str) -> Option<i32> {
    if let Ok(content) = std::fs::read_to_string(path) {
        let numeric_part: String = content
            .trim()
            .chars()
            .take_while(|c| c.is_digit(10))
            .collect();
        numeric_part.parse().ok()
    } else {
        None
    }
}

// hardware brightness getter
pub(crate) fn get_max_brightness(dbg: bool) -> i32 {
    // Static override wins over everything; read once at init.
    if let Some(override_max) = get_prop_int(persist_obright_max()) {
        if override_max > 0 {
            if dbg { log_d(&format!("[DisplayAdaptor] Using obright_max override: {}", override_max)); }
            return override_max;
        }
    }

    if let Some(cached_max) = get_prop_int(persist_hw_max()) {
        if dbg { log_d(&format!("[DisplayAdaptor] Using cached hw_max: {}", cached_max)); }
        return cached_max;
    }

    let detected = read_file_int(max_bright_path())
        .or_else(|| read_file_int(max_bright_fallback_path()));

    match detected {
        Some(val) => {
            if dbg { log_d(&format!("[DisplayAdaptor] Detected hw_max: {}. Saving to prop.", val)); }
            set_prop(persist_hw_max(), &val.to_string());
            val
        },
        None => {
            if dbg { log_d("[DisplayAdaptor] Failed to detect hw_max, using default 511"); }
            511
        }
    }
}

pub(crate) fn get_min_brightness(dbg: bool) -> i32 {
    // Static override wins over everything; read once at init.
    if let Some(override_min) = get_prop_int(persist_obright_min()) {
        if override_min > 0 {
            if dbg { log_d(&format!("[DisplayAdaptor] Using obright_min override: {}", override_min)); }
            return override_min;
        }
    }

    if let Some(cached_min) = get_prop_int(persist_hw_min()) {
        if cached_min > 0 {
            if dbg { log_d(&format!("[DisplayAdaptor] Using cached hw_min: {}", cached_min)); }
            return cached_min;
        } else {
             // Force correction if cached value is invalid (e.g. 0)
             if dbg { log_d(&format!("[DisplayAdaptor] Cached hw_min invalid ({}), forcing to 1.", cached_min)); }
             set_prop(persist_hw_min(), "1");
             return 1;
        }
    }

    match read_file_int(min_bright_path()) {
        Some(mut val) => {
            if val <= 0 {
                if dbg { log_d("[DisplayAdaptor] Detected hw_min <= 0 (screen off?), falling back to 1."); }
                val = 1;
            }
            if dbg { log_d(&format!("[DisplayAdaptor] Saving hw_min: {} to prop.", val)); }
            set_prop(persist_hw_min(), &val.to_string());
            val
        },
        None => {
            if dbg { log_d("[DisplayAdaptor] Failed to detect hw_min, using default 1"); }
            1
        }
    }
}
