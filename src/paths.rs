#![allow(dead_code)]

// file paths & property keys
pub(crate) fn min_bright_path() -> &'static str { "/sys/class/leds/lcd-backlight/min_brightness" }
pub(crate) fn max_bright_path() -> &'static str { "/sys/class/leds/lcd-backlight/max_hw_brightness" }
pub(crate) fn max_bright_fallback_path() -> &'static str { "/sys/class/leds/lcd-backlight/max_brightness" }
pub(crate) fn bright_path() -> &'static str { "/sys/class/leds/lcd-backlight/brightness" }
pub(crate) fn log_tag() -> &'static str { "Xia-DisplayAdaptor" }
pub(crate) fn persist_dbg() -> &'static str { "persist.sys.rianixia.display-debug" }
pub(crate) fn persist_obright_min() -> &'static str { "persist.sys.rianixia.obright_min" }
pub(crate) fn persist_obright_max() -> &'static str { "persist.sys.rianixia.obright_max" }
pub(crate) fn persist_hw_min() -> &'static str { "persist.sys.rianixia.hw_min" }
pub(crate) fn persist_hw_max() -> &'static str { "persist.sys.rianixia.hw_max" }
