use std::io;

use crate::elf::{Elf, Symbol};
use crate::gui_payload::{
    ABI_MODE, COUNTA, COUNTB, DPTR, FLOATS, LUT, PATHBUF, PAYLOAD, STATE, STATE_LEN,
    STRUCT_OFFSET, STUB_SET, STUB_SUPPORT, SUPPORT_ABI, WRESULT,
};
use crate::hwc_hook::{
    discover_backlight, encode_branch, find_free_holes, find_svc_gadget, poke_bytes, poke32,
    peek32, peek64, read_maps, remote_syscall, runtime_address, seize_and_stop, Backlight, InstallReport,
    MapLine, AT_FDCWD, B_OPCODE, B_RANGE, MAP_PRIVATE, MAP_PRIVATE_ANON, NR_MMAP, NR_MUNMAP,
    NR_OPENAT, O_RDONLY_CLOEXEC, O_WRONLY_CLOEXEC, PAGE_SIZE, PROT_RW, PROT_RX,
};
use crate::logging::log_d;
use crate::scaling::build_brightness_lut;

extern "C" {
    fn ptrace(
        request: std::os::raw::c_int,
        pid: std::os::raw::c_int,
        addr: *mut std::os::raw::c_void,
        data: *mut std::os::raw::c_void,
    ) -> std::os::raw::c_long;
}

const PTRACE_DETACH: std::os::raw::c_int = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetBrightnessAbi {
    /// Takes pointer to DisplayBrightness struct in x1.
    Struct { offset: u32 },
    /// Takes direct normalized float in s0.
    DirectFloat0,
    /// Takes (sdrBrightness, displayBrightness) floats in s0, s1.
    DirectFloat1,
}

impl SetBrightnessAbi {
    pub fn mode_code(&self) -> u32 {
        match self {
            SetBrightnessAbi::Struct { .. } => 0,
            SetBrightnessAbi::DirectFloat0 => 1,
            SetBrightnessAbi::DirectFloat1 => 2,
        }
    }

    pub fn struct_offset(&self) -> u32 {
        match self {
            SetBrightnessAbi::Struct { offset } => *offset,
            _ => 0x10,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportAbi {
    /// Returns bool directly in w0 (SurfaceComposerClient style).
    ReturnBool,
    /// Writes bool to *x1 and returns status_t (0) in w0 (BpSurfaceComposer style).
    OutPointer,
}

impl SupportAbi {
    pub fn mode_code(&self) -> u32 {
        match self {
            SupportAbi::ReturnBool => 0,
            SupportAbi::OutPointer => 1,
        }
    }
}

pub struct GuiTarget {
    pub pid: i32,
    pub lib_path: String,
    pub set_brightness: u64,
    pub get_support: u64,
    pub set_name: String,
    pub support_name: String,
    pub abi_mode: u32,
    pub support_abi: u32,
    pub struct_offset: u32,
}

pub fn score_set_brightness(name: &str) -> Option<(u32, SetBrightnessAbi)> {
    let method_idx = name.find("setDisplayBrightness")?;
    let param_sig = &name[method_idx + "setDisplayBrightness".len()..];

    let has_struct_param = param_sig.contains("DisplayBrightness");
    let abi = if has_struct_param {
        SetBrightnessAbi::Struct { offset: 0x10 }
    } else if param_sig.ends_with("ff") || param_sig.ends_with("ffE") || param_sig.contains("EEEff") {
        SetBrightnessAbi::DirectFloat1
    } else if param_sig.ends_with("f") || param_sig.ends_with("fE") || param_sig.contains("EEEf") {
        SetBrightnessAbi::DirectFloat0
    } else {
        SetBrightnessAbi::Struct { offset: 0x10 }
    };

    let mut score = 50;
    if name.contains("SurfaceComposerClient") {
        score += 40;
    } else if name.contains("BpSurfaceComposer") {
        score += 30;
    } else if name.contains("SurfaceComposer") {
        score += 20;
    }
    if has_struct_param {
        score += 10;
    }
    Some((score, abi))
}

pub fn score_get_support(name: &str) -> Option<(u32, SupportAbi)> {
    if !name.contains("getDisplayBrightnessSupport") {
        return None;
    }
    let abi = if name.contains("BpSurfaceComposer")
        || name.ends_with("Pb")
        || name.ends_with("PbE")
        || name.contains("Pb")
    {
        SupportAbi::OutPointer
    } else {
        SupportAbi::ReturnBool
    };

    let mut score = 50;
    if name.contains("SurfaceComposerClient") {
        score += 40;
    } else if name.contains("BpSurfaceComposer") {
        score += 30;
    } else if name.contains("SurfaceComposer") {
        score += 20;
    }
    Some((score, abi))
}

pub fn find_system_server() -> Option<(i32, String)> {
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = match entry.ok() {
            Some(e) => e,
            None => continue,
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let pid: i32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !cmdline.contains("system_server") {
            continue;
        }
        log_d(&format!("[GuiHook] candidate system_server pid {} ({})", pid, cmdline.trim()));
        let maps = match read_maps(pid) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for m in &maps {
            if m.path.ends_with("/libgui.so")
                || m.path.ends_with("/libgui_window.so")
                || m.path.ends_with("/libsurfaceflinger_client.so")
            {
                return Some((pid, m.path.clone()));
            }
        }
    }
    None
}

fn resolve_symbols(
    maps: &[MapLine],
    elf: &Elf,
    lib_path: &str,
) -> Option<(u64, u64, String, String, SetBrightnessAbi, SupportAbi)> {
    let mut best_set: Option<(&Symbol, u32, SetBrightnessAbi)> = None;
    let mut best_sup: Option<(&Symbol, u32, SupportAbi)> = None;

    for s in &elf.symbols {
        if s.value == 0 {
            continue;
        }
        if let Some((score, abi)) = score_set_brightness(&s.name) {
            let bonus = if s.dynamic { 5 } else { 0 };
            let total = score + bonus;
            if best_set.as_ref().map(|b| total > b.1).unwrap_or(true) {
                best_set = Some((s, total, abi));
            }
        }
        if let Some((score, abi)) = score_get_support(&s.name) {
            let bonus = if s.dynamic { 5 } else { 0 };
            let total = score + bonus;
            if best_sup.as_ref().map(|b| total > b.1).unwrap_or(true) {
                best_sup = Some((s, total, abi));
            }
        }
    }

    let (set_sym, set_score, set_abi) = best_set?;
    let (sup_sym, sup_score, sup_abi) = best_sup?;

    log_d(&format!(
        "[GuiHook] matched setDisplayBrightness: \"{}\" (score={}, abi={:?})",
        set_sym.name, set_score, set_abi
    ));
    log_d(&format!(
        "[GuiHook] matched getDisplayBrightnessSupport: \"{}\" (score={}, abi={:?})",
        sup_sym.name, sup_score, sup_abi
    ));

    let set_addr = runtime_address(maps, elf, lib_path, set_sym.value)?;
    let sup_addr = runtime_address(maps, elf, lib_path, sup_sym.value)?;

    Some((
        set_addr,
        sup_addr,
        set_sym.name.clone(),
        sup_sym.name.clone(),
        set_abi,
        sup_abi,
    ))
}

pub fn locate() -> io::Result<GuiTarget> {
    let (pid, lib_path) = find_system_server().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "system_server process not found")
    })?;
    let data = std::fs::read(&lib_path)?;
    let elf = Elf::parse(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let maps = read_maps(pid)?;
    let (set_brightness, get_support, set_name, support_name, set_abi, sup_abi) =
        resolve_symbols(&maps, &elf, &lib_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "SurfaceComposerClient symbols not found",
            )
        })?;
    Ok(GuiTarget {
        pid,
        lib_path,
        set_brightness,
        get_support,
        set_name,
        support_name,
        abi_mode: set_abi.mode_code(),
        support_abi: sup_abi.mode_code(),
        struct_offset: set_abi.struct_offset(),
    })
}

fn already_patched(tid: i32, target: &GuiTarget) -> Option<u64> {
    let word = peek32(tid, target.get_support)?;
    if (word & 0xfc00_0000) != B_OPCODE {
        return None;
    }
    let imm = (word & 0x03ff_ffff) as i32;
    let imm = if imm & 0x0200_0000 != 0 {
        imm - 0x0400_0000
    } else {
        imm
    };
    let dest = (target.get_support as i64 + (imm as i64) * 4) as u64;
    let sig = peek32(tid, dest)?;
    let expect = u32::from_le_bytes([PAYLOAD[0], PAYLOAD[1], PAYLOAD[2], PAYLOAD[3]]);
    if sig == expect {
        Some(dest)
    } else {
        None
    }
}

fn map_pages(
    tid: i32,
    svc: u64,
    target: &GuiTarget,
    hint: u64,
) -> io::Result<(u64, u64)> {
    let regs = crate::hwc_hook::get_regs(tid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot read registers"))?;
    let path = target.lib_path.as_bytes();
    if path.len() >= 512 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "library path too long"));
    }
    let path_addr = regs.sp - 1024;
    if !poke_bytes(tid, path_addr, path) || !poke32(tid, path_addr + path.len() as u64, 0) {
        return Err(io::Error::new(io::ErrorKind::Other, "stack path write failed"));
    }

    let lib_fd = remote_syscall(
        tid,
        svc,
        NR_OPENAT,
        [AT_FDCWD, path_addr, O_RDONLY_CLOEXEC, 0, 0, 0],
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "remote openat failed"))? as i64;
    if lib_fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("remote openat failed: errno {}", -lib_fd),
        ));
    }

    let maps = read_maps(target.pid)?;
    let holes = find_free_holes(&maps, target.set_brightness, B_RANGE as u64);
    let mut code_page = 0u64;
    for candidate in holes.iter().take(16) {
        let page = remote_syscall(
            tid,
            svc,
            NR_MMAP,
            [
                *candidate,
                PAGE_SIZE as u64,
                PROT_RX,
                MAP_PRIVATE,
                lib_fd as u64,
                0,
            ],
        )
        .unwrap_or(0);
        if page == 0 || page > i64::MAX as u64 {
            continue;
        }
        let distance = (page as i64 - target.set_brightness as i64).abs();
        if distance <= B_RANGE {
            code_page = page;
            break;
        }
        let _ = remote_syscall(tid, svc, NR_MUNMAP, [page, PAGE_SIZE as u64, 0, 0, 0, 0]);
    }
    if code_page == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "no code page within branch reach",
        ));
    }

    let state_page = remote_syscall(
        tid,
        svc,
        NR_MMAP,
        [
            hint,
            PAGE_SIZE as u64,
            PROT_RW,
            MAP_PRIVATE_ANON,
            u64::MAX,
            0,
        ],
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "remote state mmap failed"))?;
    if state_page == 0 || state_page > i64::MAX as u64 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("remote mmap failed: errno {}", -(state_page as i64)),
        ));
    }

    Ok((code_page, state_page))
}


fn inject(
    tid: i32,
    svc: u64,
    target: &GuiTarget,
    backlight: &Backlight,
) -> io::Result<InstallReport> {
    if let Some(code_page) = already_patched(tid, target) {
        let state_page = peek64(tid, code_page + DPTR as u64).unwrap_or(0);
        if state_page != 0 {
            let _ = poke_bytes(tid, code_page, PAYLOAD);
            let _ = poke_bytes(tid, code_page + DPTR as u64, &state_page.to_le_bytes());

            let mut floats = [0u8; 8];
            floats[0..4].copy_from_slice(&1.0f32.to_le_bytes());
            floats[4..8].copy_from_slice(&255.0f32.to_le_bytes());
            let _ = poke_bytes(tid, state_page + FLOATS as u64, &floats);

            let lut = build_brightness_lut(backlight.max);
            let _ = poke_bytes(tid, state_page + LUT as u64, &lut);
        }

        let support_calls = peek32(tid, state_page + COUNTA as u64).unwrap_or(0);
        let set_calls = peek32(tid, state_page + COUNTB as u64).unwrap_or(0);
        let last_val = peek32(tid, state_page + 0x0c).unwrap_or(0);
        let wresult = peek32(tid, state_page + WRESULT as u64).unwrap_or(0);
        let abi_mode = peek32(tid, state_page + ABI_MODE as u64).unwrap_or(0);
        let support_abi = peek32(tid, state_page + SUPPORT_ABI as u64).unwrap_or(0);
        let struct_offset = peek32(tid, state_page + STRUCT_OFFSET as u64).unwrap_or(0x10);
        let raw_float = f32::from_bits(peek32(tid, state_page + 0x030).unwrap_or(0));
        log_d(&format!(
            "[GuiHook] system_server already patched (entry {:#x} -> {:#x}, state={:#x}): support_calls={} set_calls={} raw_float={:.4} last_val={} wresult={} (abi_mode={}, support_abi={}, struct_offset={:#x})",
            target.get_support, code_page, state_page, support_calls, set_calls, raw_float, last_val, wresult, abi_mode, support_abi, struct_offset
        ));
        return Ok(InstallReport {
            pid: target.pid,
            lib_path: target.lib_path.clone(),
            page: code_page,
            state_page: 0,
            get_support: target.get_support,
            set_brightness: target.set_brightness,
            led_fd: -1,
            min_bri: backlight.min,
            max_bri: backlight.max,
        });
    }

    let hint = target.set_brightness & !0xfffff;
    let (code_page, state_page) = map_pages(tid, svc, target, hint)?;
    log_d(&format!(
        "[GuiHook] code page {:#x}, state page {:#x}",
        code_page, state_page
    ));

    if !poke_bytes(tid, code_page, PAYLOAD) {
        return Err(io::Error::new(io::ErrorKind::Other, "payload write failed"));
    }
    let node = backlight.node.as_bytes();
    if !poke_bytes(tid, state_page + PATHBUF as u64, node)
        || !poke32(tid, state_page + PATHBUF as u64 + node.len() as u64, 0)
    {
        return Err(io::Error::new(io::ErrorKind::Other, "path write failed"));
    }

    let fd = remote_syscall(
        tid,
        svc,
        NR_OPENAT,
        [
            AT_FDCWD,
            state_page + PATHBUF as u64,
            O_WRONLY_CLOEXEC,
            0,
            0,
            0,
        ],
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "remote openat failed"))? as i32;
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("remote openat failed: errno {}", -fd),
        ));
    }

    let mut state = [0u8; STATE_LEN];
    state[0..4].copy_from_slice(&fd.to_le_bytes());
    state[4..8].copy_from_slice(&backlight.min.to_le_bytes());
    state[8..12].copy_from_slice(&backlight.max.to_le_bytes());
    state[12..16].copy_from_slice(&(-1i32).to_le_bytes());
    if !poke_bytes(tid, state_page + STATE as u64, &state) {
        return Err(io::Error::new(io::ErrorKind::Other, "state write failed"));
    }
    let mut floats = [0u8; 8];
    floats[0..4].copy_from_slice(&1.0f32.to_le_bytes());
    floats[4..8].copy_from_slice(&255.0f32.to_le_bytes());
    let _ = poke_bytes(tid, state_page + FLOATS as u64, &floats);

    let lut = build_brightness_lut(backlight.max);
    if !poke_bytes(tid, state_page + LUT as u64, &lut) {
        return Err(io::Error::new(io::ErrorKind::Other, "lut write failed"));
    }

    let _ = poke32(tid, state_page + ABI_MODE as u64, target.abi_mode);
    let _ = poke32(tid, state_page + SUPPORT_ABI as u64, target.support_abi);
    let _ = poke32(tid, state_page + STRUCT_OFFSET as u64, target.struct_offset);

    if !poke_bytes(tid, code_page + DPTR as u64, &state_page.to_le_bytes()) {
        return Err(io::Error::new(io::ErrorKind::Other, "dptr write failed"));
    }

    let branch_support = encode_branch(target.get_support, code_page + STUB_SUPPORT as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "support stub out of reach"))?;
    let branch_set = encode_branch(target.set_brightness, code_page + STUB_SET as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "set stub out of reach"))?;

    if !poke32(tid, target.get_support, branch_support)
        || !poke32(tid, target.set_brightness, branch_set)
    {
        return Err(io::Error::new(io::ErrorKind::Other, "entry point patch failed"));
    }

    log_d(&format!(
        "[GuiHook] patched system_server pid {} ({}): support={:#x} set={:#x} code={:#x} state={:#x} fd={} range={}..{} (abi_mode={}, support_abi={})",
        target.pid,
        target.lib_path,
        target.get_support,
        target.set_brightness,
        code_page,
        state_page,
        fd,
        backlight.min,
        backlight.max,
        target.abi_mode,
        target.support_abi
    ));

    Ok(InstallReport {
        pid: target.pid,
        lib_path: target.lib_path.clone(),
        page: code_page,
        state_page,
        get_support: target.get_support,
        set_brightness: target.set_brightness,
        led_fd: fd,
        min_bri: backlight.min,
        max_bri: backlight.max,
    })
}

pub fn install() -> io::Result<InstallReport> {
    let target = locate()?;
    let backlight = discover_backlight()?;
    let tid = seize_and_stop(target.pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot stop system_server thread"))?;
    let maps = read_maps(target.pid)?;
    let svc = find_svc_gadget(&maps)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no svc gadget in target"))?;

    let res = inject(tid, svc, &target, &backlight);
    unsafe { ptrace(PTRACE_DETACH, tid, std::ptr::null_mut(), std::ptr::null_mut()) };
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_payload::{
        ABI_MODE, COUNTA, COUNTB, DPTR, LUT, LUT_LEN, PAYLOAD, STRUCT_OFFSET, STUB_SET,
        STUB_SUPPORT, SUPPORT_ABI,
    };

    #[test]
    fn gui_payload_layout_matches() {
        assert_eq!(PAYLOAD.len(), 1032);
        assert_eq!(STUB_SUPPORT, 0x000);
        assert_eq!(STUB_SET, 0x200);
        assert_eq!(DPTR, 0x400);
        assert_eq!(COUNTA, 0x01c);
        assert_eq!(COUNTB, 0x020);
        assert_eq!(ABI_MODE, 0x024);
        assert_eq!(SUPPORT_ABI, 0x028);
        assert_eq!(STRUCT_OFFSET, 0x02c);
        assert_eq!(LUT, 0x200);
        assert_eq!(LUT_LEN, 256 * 4);
    }

    #[test]
    fn score_set_brightness_standard_gui_display_brightness() {
        let sym = "_ZN7android21SurfaceComposerClient20setDisplayBrightnessERKNS_2spINS_7IBinderEEERKNS_3gui17DisplayBrightnessE";
        let (score, abi) = score_set_brightness(sym).expect("should match");
        assert_eq!(score, 100);
        assert_eq!(abi, SetBrightnessAbi::Struct { offset: 0x10 });
        assert_eq!(abi.mode_code(), 0);
    }

    #[test]
    fn score_set_brightness_nested_namespace() {
        let sym = "_ZN7android4aidl3gui21SurfaceComposerClient20setDisplayBrightnessERKNS_2spINS_7IBinderEEERKNS1_17DisplayBrightnessE";
        let (score, abi) = score_set_brightness(sym).expect("should match");
        assert_eq!(score, 100);
        assert_eq!(abi, SetBrightnessAbi::Struct { offset: 0x10 });
    }

    #[test]
    fn score_set_brightness_direct_float() {
        let sym = "_ZN7android21SurfaceComposerClient20setDisplayBrightnessERKNS_2spINS_7IBinderEEEf";
        let (score, abi) = score_set_brightness(sym).expect("should match");
        assert_eq!(score, 90);
        assert_eq!(abi, SetBrightnessAbi::DirectFloat0);
        assert_eq!(abi.mode_code(), 1);
    }

    #[test]
    fn score_set_brightness_dual_float() {
        let sym = "_ZN7android21SurfaceComposerClient20setDisplayBrightnessERKNS_2spINS_7IBinderEEEff";
        let (score, abi) = score_set_brightness(sym).expect("should match");
        assert_eq!(score, 90);
        assert_eq!(abi, SetBrightnessAbi::DirectFloat1);
        assert_eq!(abi.mode_code(), 2);
    }

    #[test]
    fn score_set_brightness_bp_surface_composer() {
        let sym = "_ZN7android3gui17BpSurfaceComposer20setDisplayBrightnessERKNS_2spINS_7IBinderEEERKNS0_17DisplayBrightnessE";
        let (score, abi) = score_set_brightness(sym).expect("should match");
        assert_eq!(score, 90);
        assert_eq!(abi, SetBrightnessAbi::Struct { offset: 0x10 });
    }

    #[test]
    fn score_get_support_variants() {
        let sym1 = "_ZN7android21SurfaceComposerClient27getDisplayBrightnessSupportERKNS_2spINS_7IBinderEEE";
        let (score1, abi1) = score_get_support(sym1).expect("should match");
        assert_eq!(score1, 90);
        assert_eq!(abi1, SupportAbi::ReturnBool);
        assert_eq!(abi1.mode_code(), 0);

        let sym2 = "_ZN7android3gui17BpSurfaceComposer27getDisplayBrightnessSupportERKNS_2spINS_7IBinderEEEPb";
        let (score2, abi2) = score_get_support(sym2).expect("should match");
        assert_eq!(score2, 80);
        assert_eq!(abi2, SupportAbi::OutPointer);
        assert_eq!(abi2.mode_code(), 1);
    }

    #[test]
    fn gui_payload_stub_a_encodings() {
        let u32_at = |off: usize| {
            u32::from_le_bytes([
                PAYLOAD[off],
                PAYLOAD[off + 1],
                PAYLOAD[off + 2],
                PAYLOAD[off + 3],
            ])
        };
        assert_eq!(u32_at(0x000), 0x1000_2005); // adr x5, dptr
        assert_eq!(u32_at(0x004), 0xf940_00a4); // ldr x4, [x5]
    }

    #[test]
    fn gui_payload_stub_b_encodings() {
        let u32_at = |off: usize| {
            u32::from_le_bytes([
                PAYLOAD[off],
                PAYLOAD[off + 1],
                PAYLOAD[off + 2],
                PAYLOAD[off + 3],
            ])
        };
        assert_eq!(u32_at(0x200), 0x1000_1005); // adr x5, dptr
        assert_eq!(u32_at(0x204), 0xf940_00a4); // ldr x4, [x5]
    }
}
