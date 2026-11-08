# OPlus-Brightness-Adaptor

A Quick Fix and simple for OPlus Ports on MTK Devices.

Native Rust AIDL service (`vendor.oplus.hardware.displaypanelfeature.IDisplayPanelFeature/default`) and dynamic in-process interceptor for OxygenOS and ColorOS ports on MediaTek and Transsion hardware.

For an exhaustive architectural breakdown of why stock OPlus ports fail on MediaTek and how the in-memory injection works, see [HOW_IT_WORKS.md](HOW_IT_WORKS.md).

---

## Compatibility

- **Target Architecture:** ARM64 (`aarch64-linux-android`).
- **Target OS:** Android 14, 15, and 16 ports of OxygenOS and ColorOS.
- **Tested Hardware:**
  - Infinix GT 10 Pro (`X6739`, MediaTek Dimensity 8050 / MT6893, AMOLED panel).
  - Multiple Helio G99 (`MT6789`) Transsion Infinix devices with IPS LCD panels (`hw_max = 511` / `2047`).
  - Broadly compatible with generic MediaTek vendor trees lacking native OPlus `LedDevice` HWC implementations.

---

## Adding the Prebuilt Binary to Your Vendor

Download the package from the [Latest Release](https://github.com/ryanistr/OPlus-Brightness-Adaptor/releases/latest).

### 1. Copy Files to Vendor Tree

Stage the files into your unpacked vendor partition tree:

```text
vendor/
├── bin/hw/
│   └── vendor.xia.display.adaptor-VX@X.X-service      (mode: 0755)
└── etc/
    ├── init/
    │   └── init.xia.display.adaptor.rc                (mode: 0644)
    └── vintf/manifest/
        └── manifest_rianixia_displaypanelfeature_aidl.xml (mode: 0644)
```

### 2. Set SELinux File Contexts & Permissions

ROM unpackers (such as RomTools) unpack partition images alongside a `config/` directory containing filesystem metadata tables.

Do not edit files inside `vendor/etc/selinux/` directly because partition repacking scripts reconstruct contexts from the `config/` metadata directory.

1. **SELinux File Contexts:** In `config/vendor_file_contexts`, add an entry assigning the MediaTek Light HAL execution context (`mtk_hal_light_exec`) so the service transitions properly:
   ```text
   /vendor/bin/hw/vendor\.xia\.display\.adaptor-VX@X\.X-service    u:object_r:mtk_hal_light_exec:s0
   ```
2. **Filesystem Capabilities & Permissions:** In `config/vendor_fs_config`, ensure the binary is registered with executable permissions (`0755`):
   ```text
   vendor/bin/hw/vendor.xia.display.adaptor-VX@X.X-service 0 2000 0755
   vendor/etc/init/init.xia.display.adaptor.rc 0 0 0644
   vendor/etc/vintf/manifest/manifest_rianixia_displaypanelfeature_aidl.xml 0 0 0644
   ```

> [!IMPORTANT]
> **DO NOT delete the stock MediaTek Light HAL.**
> Keep the stock `android.hardware.lights-service.mediatek` binary intact. The OPlus framework uses `IDisplayPanelFeature` and `SurfaceControl.setDisplayBrightness` for primary display illumination, while notifications, buttons, and auxiliary LEDs remain handled by the stock MediaTek Light HAL. Both services coexist.

### 3. Verification & Denials

If the display slider does not respond on first boot, check logcat and dmesg for denials:

```bash
adb logcat -s "DisplayAdaptor" "xia_display_adaptor"
adb shell dmesg | grep -i "avc: denied"
```

Verify that the service process has read and write access to `/sys/class/leds/lcd-backlight/brightness` and read access to `max_brightness`.

---

## Dynamic Perceptual Curve

The adaptor automatically reads `/sys/class/leds/lcd-backlight/max_brightness` and computes the exact gamma exponent for your hardware:

- **Standard Panels (`hw_max <= 511`, 8-bit / 9-bit, typically AMOLED):** $\gamma = 1.00$ (linear passthrough). Prevents double-gamma compounding with Android framework's built-in perceptual curve so the screen does not appear dim indoors. Minimum visible hardware floor is calculated as `5`.
- **High-Bit Backlight Panels (`hw_max = 4096`, 12-bit, typically IPS LCD):** $\gamma = 1.51$. Softens mid-range brightness expansion to prevent the 30% to 40% slider blowout. Dynamically scales the minimum visible hardware floor to `40`, preventing the display from shutting off at low slider positions ($\le 10$).

No property configuration or manual curve tuning is required.

---

## Configuration & Debugging

All obsolete multibrightness and tracing properties are dropped. Only the runtime debug logging property is retained:

| Property | Default | Values | Description |
|---|---|---|---|
| `persist.sys.rianixia.display-debug` | `false` | `true`, `false` | Enables verbose debug logging in Android logcat (`logcat -s DisplayAdaptor`). Disabled by default to minimize log spam. |

To enable debug logs on a running device:

```bash
adb shell setprop persist.sys.rianixia.display-debug true
```
