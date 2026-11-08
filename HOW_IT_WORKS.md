# How OPlus-Brightness-Adaptor Works

This document details the internal architecture, failure modes on ported MediaTek devices, in-memory ptrace injection, dynamic symbol scoring, and the mathematical calibration of the perceptual curve.

---

## 1. The Stock OPlus Brightness Pipeline

On stock OxygenOS and ColorOS devices running native hardware, screen brightness is routed through a proprietary path rather than standard AOSP channels:

1. **Framework Calculation:** `DisplayPowerController` maps the user slider integer (`222` to `8191`) into a perceptual float $s_0 \in [0.0, 1.0]$.
2. **Forced SurfaceControl Dispatch:** In standard Android, `LocalDisplayDevice` checks if the Hardware Composer supports brightness. If not, it falls back to `LightsService` (`LogicalLight.setBrightness`). On OPlus, `mForceSurfaceControl` is hardcoded to `true` inside `services.jar`. Brightness dispatch routes exclusively via `SurfaceControl.setDisplayBrightness`. The standard Light HAL fallback remains unexecuted.
3. **SurfaceFlinger Forwarding:** SurfaceFlinger forwards the transaction via `ISurfaceComposer::setDisplayBrightness` to the Hardware Composer HAL (`HWC2::Display::setDisplayBrightness`).
4. **Native Vendor HWC Execution:** Stock OPlus HWC (`hwcomposer.mtk_common.so` or `hwcomposer.qcom.so`) contains an internal `LedDevice` linking to `/sys/class/leds/lcd-backlight/brightness` and `PanelChaplin::PanelChaplinWrapper`. When SurfaceFlinger delivers the float, HWC writes directly to the kernel sysfs node.

---

## 2. The Ported Vendor Failure Mode

When porting OxygenOS or ColorOS to non-OPlus MediaTek hardware (such as Transsion Infinix or Tecno devices):

1. **Vendor Composer Lacks Brightness Logic:** The target device uses its own vendor composer (e.g. `hwcomposer.mt6893.so` or `hwcomposer.mt6789.so`). In this binary, `HWCMediator::displayGetBrightnessSupport` explicitly returns false (`*outSupport = false`), and `HWCMediator::displaySetBrightness` returns `0x8` (`HWC2_ERROR_UNSUPPORTED`).
2. **Silent Drop in SurfaceFlinger:** SurfaceFlinger checks composer brightness capability. Because the composer reports unsupported, SurfaceFlinger drops brightness transactions.
3. **Light HAL Never Called:** Because `mForceSurfaceControl` is true in framework smali, the OS never calls `android.hardware.light.ILights`. The screen stays stuck at default boot brightness.

---

## 3. The Interceptor Architecture

`OPlus-Brightness-Adaptor` provides two complementary mechanisms to resolve this: an AIDL service and dynamic in-memory interceptors.

```
┌────────────────────────────────────────────────────────────────────────┐
│ Android Framework: DisplayPowerController                             │
│   Calculates perceptual float s0 ∈ [0.0, 1.0]                          │
└──────────────────┬─────────────────────────────────────────────────────┘
                   │
         [PATH A]  │ IPC / Binder
                   ▼
┌────────────────────────────────────────────────────────────────────────┐
│ system_server: libgui.so (SurfaceComposerClient)                       │
│                                                                        │
│   • gui_hook intercepts:                                               │
│       - getDisplayBrightnessSupport                                    │
│       - setDisplayBrightness                                           │
│   • Replaced with: Branch to injected AArch64 payload                  │
│   • Action: Reads float s0, interpolates LUT, writes /sys/class/leds   │
└──────────────────┬─────────────────────────────────────────────────────┘
                   │
         [PATH B]  │ SurfaceFlinger
                   ▼
┌────────────────────────────────────────────────────────────────────────┐
│ vendor.hwcomposer-*-service: hwcomposer.*.so / libmtkhwc*.so          │
│                                                                        │
│   • hwc_hook intercepts:                                               │
│       - displayGetBrightnessSupport                                    │
│       - displaySetBrightness                                           │
│   • Replaced with: Branch to injected AArch64 payload                  │
│   • Action: In-process direct write under composer SELinux context     │
└──────────────────┬─────────────────────────────────────────────────────┘
                   │
         [PATH C]  │ Native OOS AIDL Path
                   ▼
┌────────────────────────────────────────────────────────────────────────┐
│ vendor.xia.display.adaptor-VX@X.X-service                              │
│   Implements: IDisplayPanelFeature & android.hardware.light            │
│   Directly called by OOS light controller via AIDL                     │
└────────────────────────────────────────────────────────────────────────┘
```

### A. Dynamic In-Memory Hooking (GUI Hook & HWC Hook)

Instead of modifying system binaries on disk, the adaptor uses Linux `ptrace` remote syscalls to patch running target processes:

1. **Target Discovery:** Finds `system_server` or the composer daemon via `/proc/[pid]/cmdline` and `/proc/[pid]/maps`.
2. **Dynamic In-Memory Symbol Scoring:** The ELF parser (`src/elf.rs`) parses `.dynsym` and `.dynstr` directly from memory to locate target functions regardless of compiler mangling variations across Android versions (supports struct ABI, direct float ABI, and dual float ABI).
3. **Remote Page Allocation:** Maps a code page (`PROT_READ|PROT_EXEC`) and a shared state page (`PROT_READ|PROT_WRITE`) inside the target process.
4. **Direct Kernel Syscall Stub:** Injects a position-independent AArch64 payload. When the framework attempts to set brightness, execution branches to the payload. The payload performs an in-process Linux kernel syscall (`svc #0` with `__NR_write`), executing under the target process's existing SELinux label.
5. **Hot-Reloading:** The installer detects previous hook installations and updates the code page instructions and lookup tables in place without restarting target daemons.

### B. AIDL `IDisplayPanelFeature` Service

The background service implements `vendor.oplus.hardware.displaypanelfeature.IDisplayPanelFeature/default`:

- **Panel Compatibility:** Handles OPlus display panel feature codes invoked by framework services.
- **Fingerprint Delegation:** Codes related to optical fingerprint (`0xd3` / `FP_TYPE`, `0x1c` / `FP_PRESS`, `0x16` / `DIMLAY_HBM`, `0x17` / `DIMLAYER_BL_EN`, and `179`) return `StatusCode::UnknownTransaction`. This intentionally triggers the framework's native MediaTek SurfaceFlinger fallback (`transact 20004` / `NOTIFY_DISP_FINGER_LAYER`), preventing lockscreen flickering or display corruption.

---

## 4. The Dynamic Perceptual Curve

Different display panels vary widely in their hardware dynamic range:
- Standard 8-bit / 9-bit panels (typically AMOLED / OLED, driven by display IC PWM or low-step registers): `hw_max = 511`
- 10-bit panels: `hw_max = 1023`
- 11-bit panels: `hw_max = 2047`
- 12-bit hardware-backlight panels (typically IPS LCD with high-resolution LED driver chips): `hw_max = 4096`

### The Double-Compounding Problem

The Android framework's `DisplayPowerController` already converts the linear slider into a non-linear perceptual float ($s_0$):
- At 50% slider, $s_0 \approx 0.1004$.
- At manual maximum, $s_0 \approx 0.4999$ (reserving $0.50$ to $1.00$ for outdoor Sunlight HBM).

If an external HAL applies an additional gamma exponent (e.g. $\gamma = 1.8$) on top of this float for a 511-count panel:
$$0.1004^{1.8} = 0.0157 \implies 0.0157 \times 511 \approx 8 \text{ counts}$$
This compounds the curve twice, making indoor brightness excessively dim.

However, pure linear scaling on a 4096-count panel causes the expanded dynamic range to ramp up too quickly:
$$0.2508 \times 4095 \approx 1027 \text{ counts} \ (>500 \text{ nits indoors at 35\% slider})$$

### The Continuous Auto-Gamma Formulation

The adaptor automatically detects the panel hardware bit depth from `max_brightness` and calculates the gamma exponent:

$$\text{bits} = \log_2(hw\_max)$$
$$\gamma = \begin{cases} 1.00 & \text{if } \text{bits} \le 9.0 \ (hw\_max \le 511) \\ \text{clamp}(1.0 + (\text{bits} - 9.0) \times 0.17, 1.0, 1.8) & \text{if } \text{bits} > 9.0 \end{cases}$$

- **$hw\_max \le 511$ (9-bit):** $\gamma = 1.00$ (linear passthrough preserves the framework calibration).
- **$hw\_max = 1023$ (10-bit):** $\gamma = 1.17$
- **$hw\_max = 2047$ (11-bit):** $\gamma = 1.34$
- **$hw\_max = 4096$ (12-bit):** $\gamma = 1.51$ (softens the wide range and prevents mid-range blowout).

### 256-Entry Float Lookup Table (LUT)

Floating-point library functions (`powf`, `log2`) cannot be linked inside the raw in-process assembly stub. During initialization, the adaptor precalculates a 256-entry float lookup table in the shared state page:

$$\text{lut}[i] = \left(\frac{i}{255.0}\right)^\gamma$$

The AArch64 stub uses piecewise-linear interpolation between adjacent table entries in twelve machine instructions:

```armasm
ldr   s2, [x4, #0x014]       // 255.0f
fmul  s2, s0, s2             // s0 * 255.0
fcvtzs w13, s2               // index i = floor(s2)
// load lut[i] and lut[i+1]
// calculate fractional part and interpolate via fmadd
```

This guarantees branchless, exact curve calculation with zero external library dependencies.

### Dynamic Minimum Luminescence Floor

Display drivers require a minimum conduction threshold before the backlight or emitter emits visible light. On standard 511-step AMOLED panels, hardware level 5 ($\approx 0.98\%$) represents the baseline visible floor. On 12-bit 4096-step IPS hardware backlights, writing 10 or below drops below the LED driver's conduction threshold, shutting off the backlight completely.

The adaptor dynamically predicts the visible hardware floor proportionally from the baseline:

$$\text{effective\_min} = \max\left(hw\_min, \text{round}\left(hw\_max \times \frac{5.0}{511.0}\right)\right)$$

- **$hw\_max = 511$ (AMOLED):** $\text{effective\_min} = 5$
- **$hw\_max = 1023$ (10-bit):** $\text{effective\_min} = 10$
- **$hw\_max = 2047$ (11-bit):** $\text{effective\_min} = 20$
- **$hw\_max = 4096$ (IPS Backlight):** $\text{effective\_min} = 40$

This prevents the screen from going pitch black when lowering brightness to minimum on 4096-step displays, while preserving the exact calibrated low-end response on 511-step panels.

Similarly, screen wake state-transition intermediate pulses are calculated dynamically:

$$\text{state\_change\_level} = \max\left(2, \text{round}\left(hw\_max \times \frac{10.0}{511.0}\right)\right)$$

yielding `10` on 511 and `80` on 4096.
