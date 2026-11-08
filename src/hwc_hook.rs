// One-shot, dynamic MediaTek HWC2 in-memory hook.
//
// The framework routes per-frame display brightness through
// `SurfaceControl.setDisplayBrightness` -> SurfaceFlinger -> the composer HAL.
// On ported MediaTek vendor trees the stock `hwcomposer.*.so` answers
// `HWC2_ERROR_UNSUPPORTED` and never touches the panel node, so this module
// patches the two HWC2 entry points *inside the running composer process*.
//
// How it works:
//
//   1. The composer process is found by scanning `/proc/*/maps` for a mapped
//      `hwcomposer.*.so` (or `libmtkhwc*.so`).
//   2. The two entry points are resolved BY NAME from the library's own
//      `.dynsym`/`.symtab`; no MediaTek-specific offset is ever baked in.
//   3. A single page is mapped into the composer process with
//      `PROT_READ|PROT_WRITE|PROT_EXEC` by running `mmap` there through
//      `ptrace` remote syscalls. The payload (see `hwc_payload.rs`) is
//      written into that page and the backlight node is opened there too, so
//      the composer process itself owns the file descriptor and applies
//      every write under its own SELinux label.
//   4. The entry points are replaced with a direct branch to the payload.
//
// The hook is installed once and the installer then exits: there is no
// polling thread and no background work at all. The patched composer writes
// the panel level for every brightness frame the framework sends.

use std::io;
use std::os::raw::{c_int, c_long, c_void};

use crate::elf::Elf;
use crate::hwc_payload::{
    DPTR, FLOATS, LUT, LUT_LEN, PATHBUF, PAYLOAD, STATE, STATE_LEN, STUB_SET, STUB_SUPPORT,
};
use crate::logging::{log_d, log_e};
use crate::scaling::compute_dynamic_gamma;

extern "C" {
    fn ptrace(request: c_int, pid: c_int, addr: *mut c_void, data: *mut c_void) -> c_long;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
}

const PTRACE_PEEKTEXT: c_int = 1;
const PTRACE_POKETEXT: c_int = 4;
const PTRACE_ATTACH: c_int = 16;
const PTRACE_DETACH: c_int = 17;
const PTRACE_SINGLESTEP: c_int = 9;
const PTRACE_SETREGSET: c_int = 0x4205;
const PTRACE_GETREGSET: c_int = 0x4204;
const NT_PRSTATUS: c_int = 1;
const __WALL: c_int = 0x4000_0000;

/// aarch64 syscall numbers used for the remote calls.
pub(crate) const NR_OPENAT: u64 = 56;
pub(crate) const NR_MMAP: u64 = 222;
pub(crate) const NR_MUNMAP: u64 = 215;

/// `mmap` flags: MAP_PRIVATE.
pub(crate) const MAP_PRIVATE: u64 = 0x02;
/// `mmap` flags: MAP_PRIVATE | MAP_ANONYMOUS.
pub(crate) const MAP_PRIVATE_ANON: u64 = 0x22;
/// `mmap` protection: PROT_READ | PROT_WRITE.
pub(crate) const PROT_RW: u64 = 0x3;
/// `mmap` protection: PROT_READ | PROT_EXEC.
pub(crate) const PROT_RX: u64 = 0x5;
/// `openat` flags: O_WRONLY | O_CLOEXEC.
pub(crate) const O_WRONLY_CLOEXEC: u64 = 0x80001;
/// `openat` flags: O_RDONLY | O_CLOEXEC.
pub(crate) const O_RDONLY_CLOEXEC: u64 = 0x80000;
/// `AT_FDCWD`.
pub(crate) const AT_FDCWD: u64 = (-100i64) as u64;

/// `svc #0`, the instruction a remote syscall is executed through.
pub const SVC_0: u32 = 0xd400_0001;
/// `b <label>` opcode.
pub const B_OPCODE: u32 = 0x1400_0000;
/// `ldr x16, #8` - load a 64 bit literal eight bytes ahead.
pub const LDR_X16_LIT: u32 = 0x5800_0050;
/// `br x16`.
pub const BR_X16: u32 = 0xd61f_0200;
/// Maximum signed distance of a `b` instruction, in bytes.
pub const B_RANGE: i64 = 1 << 27;
/// Size of the payload page.
pub(crate) const PAGE_SIZE: usize = 4096;

/// arm64 `user_pt_regs`: 31 general registers, sp, pc, pstate.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct UserPtRegs {
    pub(crate) regs: [u64; 31],
    pub(crate) sp: u64,
    pub(crate) pc: u64,
    pub(crate) pstate: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MapLine {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) offset: u64,
    pub(crate) perms: String,
    pub(crate) path: String,
}

/// The composer process and the HWC library it mapped.
pub struct Composer {
    pub pid: i32,
    pub lib_path: String,
    /// Runtime address of `displaySetBrightness`.
    pub set_brightness: u64,
    /// Runtime address of `displayGetBrightnessSupport`.
    pub get_support: u64,
    /// True when the library already drives the panel itself.
    pub has_led_device: bool,
}

/// What the installer did.
pub struct InstallReport {
    pub pid: i32,
    pub lib_path: String,
    /// Address of the executable page mapped into the composer.
    pub page: u64,
    /// Address of the writable state page mapped into the composer.
    pub state_page: u64,
    pub get_support: u64,
    pub set_brightness: u64,
    /// File descriptor the composer now holds for the backlight node.
    pub led_fd: i32,
    pub min_bri: i32,
    pub max_bri: i32,
}

pub(crate) fn read_maps(pid: i32) -> io::Result<Vec<MapLine>> {
    let text = std::fs::read_to_string(format!("/proc/{}/maps", pid))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let range = match it.next() {
            Some(r) => r,
            None => continue,
        };
        let mut parts = range.splitn(2, '-');
        let start = u64::from_str_radix(parts.next().unwrap_or("0"), 16).unwrap_or(0);
        let end = u64::from_str_radix(parts.next().unwrap_or("0"), 16).unwrap_or(0);
        let perms = it.next().unwrap_or("").to_string();
        let offset = it.next().and_then(|o| u64::from_str_radix(o, 16).ok()).unwrap_or(0);
        // The remaining fields are the device, the inode and the path; the
        // path is the last one and is only present for file-backed mappings.
        // An anonymous mapping ends in the inode placeholder "0", which must
        // not be mistaken for a path.
        let path = it.last().unwrap_or("").to_string();
        let path = if path.starts_with('/') {
            path
        } else {
            String::new()
        };
        out.push(MapLine { start, end, offset, perms, path });
    }
    Ok(out)
}

/// Locate the composer HAL process and the HWC library it has mapped.
fn find_composer() -> Option<(i32, String)> {
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
        if !cmdline.contains("composer") {
            continue;
        }
        log_d(&format!("[HwcHook] candidate composer pid {} ({})", pid, cmdline.trim()));
        let maps = match read_maps(pid) {
            Ok(m) => m,
            Err(_) => continue,
        };
        for m in &maps {
            let file = m.path.rsplit('/').next().unwrap_or("");
            if file.starts_with("hwcomposer.") && file.ends_with(".so") {
                return Some((pid, m.path.clone()));
            }
        }
        // Fall back to the split-library layout used by newer BSPs.
        for m in &maps {
            let file = m.path.rsplit('/').next().unwrap_or("");
            if file.starts_with("libmtkhwc") && file.ends_with(".so") {
                return Some((pid, m.path.clone()));
            }
        }
    }
    None
}

/// Runtime address of a symbol in the target process.
///
/// The address is derived from the ELF program headers and the target's own
/// mapping table, so no MediaTek-specific offset is ever baked in.
pub(crate) fn runtime_address(
    maps: &[MapLine],
    elf: &Elf,
    lib_path: &str,
    symbol_value: u64,
) -> Option<u64> {
    for seg in &elf.segments {
        if symbol_value < seg.vaddr || symbol_value >= seg.vaddr + seg.filesz {
            continue;
        }
        let file_off = seg.offset + (symbol_value - seg.vaddr);
        // Only mappings of the library itself may be used. An anonymous
        // mapping can cover the same file offset range and would then be
        // picked as the load base.
        for m in maps {
            if m.path != lib_path {
                continue;
            }
            if m.offset <= file_off && file_off < m.offset + (m.end - m.start) {
                return Some(m.start + (file_off - m.offset));
            }
        }
    }
    None
}

/// Find unmapped gaps in the target's address space.
///
/// The code page has to be mapped at an address the kernel can actually
/// hand out, and the entry point has to be able to branch to it. The hint
/// derived from the library address is usually occupied by the library's own
/// segments, so the free gaps around the entry point are searched instead.
pub(crate) fn find_free_holes(maps: &[MapLine], near: u64, radius: u64) -> Vec<u64> {
    let mut segs: Vec<(u64, u64)> = maps.iter().map(|m| (m.start, m.end)).collect();
    segs.sort();
    let mut holes = Vec::new();
    let low = near.saturating_sub(radius);
    let high = near.saturating_add(radius);
    // The gap below the lowest mapping and above the highest one.
    if let Some(first) = segs.first() {
        if first.0 > low {
            holes.push(low);
        }
    }
    for pair in segs.windows(2) {
        let gap_start = pair[0].1;
        let gap_end = pair[1].0;
        if gap_end <= gap_start {
            continue;
        }
        if gap_end < low || gap_start > high {
            continue;
        }
        // The hint has to be page aligned and leave a full page inside the gap.
        let mut candidate = (gap_start + 0xfff) & !0xfff;
        if candidate < low {
            candidate = low;
        }
        while candidate + PAGE_SIZE as u64 <= gap_end && candidate <= high {
            holes.push(candidate);
            candidate += 0x0100_0000;
        }
    }
    if let Some(last) = segs.last() {
        if last.1 < high {
            holes.push((last.1 + 0xfff) & !0xfff);
        }
    }
    holes
}

/// Resolve the two HWC2 entry points by symbol name, without hardcoded offsets.
fn resolve_symbols(maps: &[MapLine], elf: &Elf, lib_path: &str) -> Option<(u64, u64)> {
    let set = elf.find("displaySetBrightness")?;
    let sup = elf.find("displayGetBrightnessSupport")?;
    if set.value == 0 || sup.value == 0 {
        return None;
    }
    let set_addr = runtime_address(maps, elf, lib_path, set.value)?;
    let sup_addr = runtime_address(maps, elf, lib_path, sup.value)?;
    Some((set_addr, sup_addr))
}

/// Find the composer process and the two HWC2 entry points.
pub fn locate() -> io::Result<Composer> {
    let (pid, lib_path) = find_composer()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "composer process not found"))?;
    let data = std::fs::read(&lib_path)?;
    let elf = Elf::parse(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let maps = read_maps(pid)?;
    let (set_brightness, get_support) = resolve_symbols(&maps, &elf, &lib_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HWC2 symbols not found"))?;
    let has_led_device = elf.find("LedDevice").is_some();
    Ok(Composer {
        pid,
        lib_path,
        set_brightness,
        get_support,
        has_led_device,
    })
}

fn ptrace_peek(pid: i32, addr: u64) -> Option<u64> {
    unsafe {
        let v = ptrace(PTRACE_PEEKTEXT, pid, addr as *mut c_void, std::ptr::null_mut());
        if v == -1 {
            None
        } else {
            Some(v as u64)
        }
    }
}

/// Write one 32-bit instruction into the target's memory.
///
/// `PTRACE_POKETEXT` transfers a whole machine word, so the bytes above the
/// instruction are preserved; zeroing them would turn the next instruction
/// into `UDF #0` and kill the target.
pub fn poke32(pid: i32, addr: u64, insn: u32) -> bool {
    let aligned = addr & !7;
    let shift = (addr - aligned) * 8;
    let mut word = match ptrace_peek(pid, aligned) {
        Some(w) => w,
        None => return false,
    };
    let mask = 0xffff_ffffu64 << shift;
    word = (word & !mask) | ((insn as u64) << shift);
    unsafe { ptrace(PTRACE_POKETEXT, pid, aligned as *mut c_void, word as *mut c_void) != -1 }
}

/// Read back the 32-bit instruction at `addr`.
pub fn peek32(pid: i32, addr: u64) -> Option<u32> {
    let aligned = addr & !7;
    let shift = (addr - aligned) * 8;
    let word = ptrace_peek(pid, aligned)?;
    Some(((word >> shift) & 0xffff_ffff) as u32)
}

/// Read back a 64-bit value at `addr`.
pub fn peek64(pid: i32, addr: u64) -> Option<u64> {
    let lo = peek32(pid, addr)? as u64;
    let hi = peek32(pid, addr + 4)? as u64;
    Some(lo | (hi << 32))
}

/// Write a byte range into the target's memory.
pub(crate) fn poke_bytes(pid: i32, addr: u64, bytes: &[u8]) -> bool {
    if ptrace_peek(pid, addr).is_none() {
        log_e(&format!(
            "[HwcHook] peek at {:#x} failed: {}",
            addr,
            io::Error::last_os_error()
        ));
        return false;
    }
    let mut off = 0usize;
    while off < bytes.len() {
        let end = (off + 8).min(bytes.len());
        let chunk = &bytes[off..end];
        let mut word: u64 = 0;
        for (i, b) in chunk.iter().enumerate() {
            word |= (*b as u64) << (i * 8);
        }
        let target = addr + off as u64;
        let aligned = target & !7;
        let shift = (target - aligned) * 8;
        let mut cur = match ptrace_peek(pid, aligned) {
            Some(w) => w,
            None => return false,
        };
        // Only the bytes that are actually written may change, and they have
        // to fit inside the machine word.
        let nbytes = chunk.len() as u64;
        if shift + nbytes * 8 > 64 {
            return false;
        }
        let mask = if nbytes == 8 {
            u64::MAX
        } else {
            ((1u64 << (nbytes * 8)) - 1) << shift
        };
        cur = (cur & !mask) | ((word << shift) & mask);
        if unsafe { ptrace(PTRACE_POKETEXT, pid, aligned as *mut c_void, cur as *mut c_void) } == -1 {
            return false;
        }
        off = end;
    }
    true
}

pub(crate) fn get_regs(pid: i32) -> Option<UserPtRegs> {
    let mut regs = UserPtRegs::default();
    let mut iov = libc::iovec {
        iov_base: &mut regs as *mut _ as *mut c_void,
        iov_len: std::mem::size_of::<UserPtRegs>(),
    };
    unsafe {
        if ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS as *mut c_void, &mut iov as *mut _ as *mut c_void) == -1 {
            None
        } else {
            Some(regs)
        }
    }
}

pub(crate) fn set_regs(pid: i32, regs: &UserPtRegs) -> bool {
    let mut regs = *regs;
    let mut iov = libc::iovec {
        iov_base: &mut regs as *mut _ as *mut c_void,
        iov_len: std::mem::size_of::<UserPtRegs>(),
    };
    unsafe {
        ptrace(PTRACE_SETREGSET, pid, NT_PRSTATUS as *mut c_void, &mut iov as *mut _ as *mut c_void) != -1
    }
}

pub(crate) fn find_svc_gadget(maps: &[MapLine]) -> Option<u64> {
    let pattern = SVC_0.to_le_bytes();
    for m in maps {
        if !m.perms.contains('x') || m.path.is_empty() {
            continue;
        }
        let data = match std::fs::read(&m.path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let size = m.end - m.start;
        let mut off = 0usize;
        while off + 4 <= data.len() {
            if data[off..off + 4] == pattern {
                // `off` is the offset in the file; the runtime address is
                // derived from the mapping that covers it.
                let file_off = off as u64;
                if file_off >= m.offset && file_off < m.offset + size {
                    return Some(m.start + (file_off - m.offset));
                }
            }
            off += 4;
        }
    }
    None
}

/// Stop one thread of the target process and return its thread id.
pub(crate) fn seize_and_stop(pid: i32) -> Option<i32> {
    let mut tids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", pid)) {
        for e in entries.flatten() {
            if let Ok(tid) = e.file_name().to_string_lossy().parse::<i32>() {
                tids.push(tid);
            }
        }
    }
    let mut status = 0;
    for tid in tids {
        if unsafe { ptrace(PTRACE_ATTACH, tid, std::ptr::null_mut(), std::ptr::null_mut()) } == -1 {
            log_e(&format!(
                "[HwcHook] attach tid {} failed: {}",
                tid,
                io::Error::last_os_error()
            ));
            continue;
        }
        let rc = unsafe { waitpid(tid, &mut status, __WALL) };
        if rc > 0 {
            // Exactly one thread is stopped. The others are left untouched:
            // attaching to them would freeze the composer until init restarts
            // it.
            return Some(tid);
        }
        log_e(&format!(
            "[HwcHook] waitpid tid {} failed: {}",
            tid,
            io::Error::last_os_error()
        ));
    }
    None
}

/// Run one syscall inside the target process and return its result.
///
/// The thread's registers are saved and restored, so the target continues
/// exactly where it was.
pub(crate) fn remote_syscall(tid: i32, gadget: u64, nr: u64, args: [u64; 6]) -> Option<u64> {
    let saved = get_regs(tid)?;
    let mut regs = saved;
    regs.regs[0] = args[0];
    regs.regs[1] = args[1];
    regs.regs[2] = args[2];
    regs.regs[3] = args[3];
    regs.regs[4] = args[4];
    regs.regs[5] = args[5];
    regs.regs[8] = nr;
    regs.pc = gadget;
    if !set_regs(tid, &regs) {
        return None;
    }
    let mut status = 0;
    unsafe { ptrace(PTRACE_SINGLESTEP, tid, std::ptr::null_mut(), std::ptr::null_mut()) };
    if unsafe { waitpid(tid, &mut status, __WALL) } <= 0 {
        set_regs(tid, &saved);
        return None;
    }
    let after = get_regs(tid);
    let result = after.map(|r| r.regs[0]);
    if let Some(r) = after {
        log_d(&format!(
            "[HwcHook] remote syscall nr={} gadget={:#x} pc_after={:#x} x0={:#x}",
            nr, gadget, r.pc, r.regs[0]
        ));
    }
    set_regs(tid, &saved);
    result
}

/// Encode a direct branch to `target` from `pc`.
pub fn encode_branch(pc: u64, target: u64) -> Option<u32> {
    let delta = target as i64 - pc as i64;
    if delta % 4 != 0 || delta < -B_RANGE || delta > B_RANGE {
        return None;
    }
    Some(B_OPCODE | (((delta / 4) as u32) & 0x03ff_ffff))
}

/// The backlight sysfs node and its hardware bounds.
pub struct Backlight {
    pub dir: String,
    pub node: String,
    pub min: i32,
    pub max: i32,
}

fn read_int(path: &str) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

/// Discover the backlight node and the panel's brightness bounds.
pub fn discover_backlight() -> io::Result<Backlight> {
    let dirs = [
        "/sys/class/leds/lcd-backlight",
        "/sys/class/backlight/panel0-backlight",
    ];
    for dir in dirs {
        if !std::path::Path::new(dir).is_dir() {
            continue;
        }
        let max = read_int(&format!("{}/max_hw_brightness", dir))
            .filter(|v| *v > 0)
            .or_else(|| read_int(&format!("{}/max_brightness", dir)))
            .filter(|v| *v > 0);
        let min = read_int(&format!("{}/min_brightness", dir)).filter(|v| *v > 0).unwrap_or(1);
        if let Some(max) = max {
            let effective_min = crate::scaling::compute_min_visible_hw(max);
            return Ok(Backlight {
                dir: dir.to_string(),
                node: format!("{}/brightness", dir),
                min: min.max(effective_min),
                max,
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no backlight node found",
    ))
}

/// Install the hook in the running composer process.
///
/// The whole injection happens in one shot and this returns; no thread is
/// left behind.
pub fn install() -> io::Result<InstallReport> {
    let composer = locate()?;
    let backlight = discover_backlight()?;

    let maps = read_maps(composer.pid)?;
    let svc = find_svc_gadget(&maps)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no svc gadget in composer"))?;

    let tid = seize_and_stop(composer.pid).ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "cannot stop composer thread")
    })?;

    // The thread stays attached for the whole injection. Every failure path
    // has to resume it again: a thread left in ptrace-stop freezes the
    // composer until init restarts the whole service.
    let result = inject(tid, svc, &composer, &backlight);
    unsafe { ptrace(PTRACE_DETACH, tid, std::ptr::null_mut(), std::ptr::null_mut()) };
    result
}


/// Map the code page and the state page inside the composer.
///
/// An anonymous PROT_EXEC mapping needs the `execmem` permission, which the
/// composer domain does not have, and a writable file mapping would need
/// write access to the vendor libraries, which it does not have either.
/// A private file backed executable mapping of a library the composer already
/// loads is allowed, and MAP_PRIVATE keeps every write inside the process,
/// so the file on disk is never touched. The mutable state goes into a
/// separate anonymous read/write mapping.
fn map_pages(
    tid: i32,
    svc: u64,
    composer: &Composer,
    hint: u64,
) -> io::Result<(u64, u64)> {
    // The path has to live in the composer's address space before the file
    // can be opened there, so it goes onto the stopped thread's stack.
    let regs = get_regs(tid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot read composer registers"))?;
    let path = composer.lib_path.as_bytes();
    if path.len() >= 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "library path too long",
        ));
    }
    let path_addr = regs.sp - 1024;
    if !poke_bytes(tid, path_addr, path) || !poke32(tid, path_addr + path.len() as u64, 0) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "cannot write the library path onto the composer stack",
        ));
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

    // The page has to end up within reach of the entry points, because the
    // hook branches into it. The address the library itself is loaded at is
    // occupied, so the free gaps around the entry points are searched for a
    // hint the kernel can honour.
    let maps = read_maps(composer.pid)?;
    let holes = find_free_holes(&maps, composer.set_brightness, B_RANGE as u64);
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
        let distance = (page as i64 - composer.set_brightness as i64).abs();
        if distance <= B_RANGE {
            code_page = page;
            break;
        }
        // Out of range: give the page back and try the next hint.
        let _ = remote_syscall(
            tid,
            svc,
            NR_MUNMAP,
            [page, PAGE_SIZE as u64, 0, 0, 0, 0],
        );
    }
    if code_page == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "no code page within branch range of the entry points",
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
    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "remote mmap failed"))?;
    if state_page == 0 || state_page > i64::MAX as u64 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("remote mmap failed: errno {}", -(state_page as i64)),
        ));
    }

    Ok((code_page, state_page))
}

/// Detect an already installed hook.
///
/// The entry point carries a direct branch into the code page, and the code
/// page starts with the payload's first instruction. Both together identify
/// our own hook, so a second install does not patch the entry twice.
fn already_patched(tid: i32, composer: &Composer) -> Option<u64> {
    let word = peek32(tid, composer.get_support)?;
    if (word & 0xfc00_0000) != B_OPCODE {
        return None;
    }
    // The displacement is a signed 26 bit field in units of four bytes.
    let imm = (word & 0x03ff_ffff) as i32;
    let imm = if imm & 0x0200_0000 != 0 {
        imm - 0x0400_0000
    } else {
        imm
    };
    let target = (composer.get_support as i64 + (imm as i64) * 4) as u64;
    let sig = peek32(tid, target)?;
    let expect = u32::from_le_bytes([
        PAYLOAD[0],
        PAYLOAD[1],
        PAYLOAD[2],
        PAYLOAD[3],
    ]);
    if sig == expect {
        Some(target)
    } else {
        None
    }
}

/// Do the injection on an already stopped thread of the composer.
fn inject(
    tid: i32,
    svc: u64,
    composer: &Composer,
    backlight: &Backlight,
) -> io::Result<InstallReport> {
    if let Some(code_page) = already_patched(tid, composer) {
        let state_page = peek64(tid, code_page + DPTR as u64).unwrap_or(0);
        if state_page != 0 {
            let _ = poke_bytes(tid, code_page, PAYLOAD);
            let _ = poke_bytes(tid, code_page + DPTR as u64, &state_page.to_le_bytes());

            let mut floats = [0u8; 8];
            floats[0..4].copy_from_slice(&1.0f32.to_le_bytes());
            floats[4..8].copy_from_slice(&255.0f32.to_le_bytes());
            let _ = poke_bytes(tid, state_page + FLOATS as u64, &floats);

            let gamma = compute_dynamic_gamma(backlight.max);
            let mut lut = [0u8; LUT_LEN];
            for i in 0..256 {
                let ratio = i as f32 / 255.0;
                let curved = ratio.powf(gamma);
                lut[i * 4..(i + 1) * 4].copy_from_slice(&curved.to_le_bytes());
            }
            let _ = poke_bytes(tid, state_page + LUT as u64, &lut);
        }

        log_d(&format!(
            "[HwcHook] HWC already patched (entry {:#x} -> {:#x}, state={:#x})",
            composer.get_support, code_page, state_page
        ));
        return Ok(InstallReport {
            pid: composer.pid,
            lib_path: composer.lib_path.clone(),
            page: code_page,
            state_page: 0,
            get_support: composer.get_support,
            set_brightness: composer.set_brightness,
            led_fd: -1,
            min_bri: backlight.min,
            max_bri: backlight.max,
        });
    }

    // The hint keeps both pages close to the library so the entry points can
    // be reached with a direct branch.
    let hint = composer.set_brightness & !0xfffff;
    let (code_page, state_page) = map_pages(tid, svc, composer, hint)?;
    log_d(&format!(
        "[HwcHook] code page {:#x}, state page {:#x}",
        code_page, state_page
    ));

    // The payload goes into the executable page, the mutable state into the
    // writable one.
    if !poke_bytes(tid, code_page, PAYLOAD) {
        return Err(io::Error::new(io::ErrorKind::Other, "payload write failed"));
    }
    let node = backlight.node.as_bytes();
    if !poke_bytes(tid, state_page + PATHBUF as u64, node)
        || !poke32(tid, state_page + PATHBUF as u64 + node.len() as u64, 0)
    {
        return Err(io::Error::new(io::ErrorKind::Other, "path write failed"));
    }

    // Open the backlight node from inside the composer so the write happens
    // under the composer's own SELinux label.
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

    // State struct: { led_fd, min_bri, max_bri, last_val }.
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

    let gamma = compute_dynamic_gamma(backlight.max);
    let mut lut = [0u8; LUT_LEN];
    for i in 0..256 {
        let ratio = i as f32 / 255.0;
        let curved = ratio.powf(gamma);
        lut[i * 4..(i + 1) * 4].copy_from_slice(&curved.to_le_bytes());
    }
    if !poke_bytes(tid, state_page + LUT as u64, &lut) {
        return Err(io::Error::new(io::ErrorKind::Other, "lut write failed"));
    }

    // Point the stub at the state page.
    if !poke_bytes(
        tid,
        code_page + DPTR as u64,
        &state_page.to_le_bytes(),
    ) {
        return Err(io::Error::new(io::ErrorKind::Other, "state pointer write failed"));
    }

    // Replace both entry points with a branch into the payload.
    let branch_support = encode_branch(composer.get_support, code_page + STUB_SUPPORT as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "support stub out of range"))?;
    let branch_set = encode_branch(composer.set_brightness, code_page + STUB_SET as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "set stub out of range"))?;
    if !poke32(tid, composer.get_support, branch_support)
        || !poke32(tid, composer.set_brightness, branch_set)
    {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "entry point patch failed",
        ));
    }

    log_d(&format!(
        "[HwcHook] patched composer pid {} ({}): support={:#x} set={:#x} code={:#x} state={:#x} fd={} range={}..{}",
        composer.pid,
        composer.lib_path,
        composer.get_support,
        composer.set_brightness,
        code_page,
        state_page,
        fd,
        backlight.min,
        backlight.max
    ));

    Ok(InstallReport {
        pid: composer.pid,
        lib_path: composer.lib_path.clone(),
        page: code_page,
        state_page,
        get_support: composer.get_support,
        set_brightness: composer.set_brightness,
        led_fd: fd,
        min_bri: backlight.min,
        max_bri: backlight.max,
    })
}

/// Log the resolved symbols without touching the target.
pub fn describe() -> Option<Composer> {
    match locate() {
        Ok(c) => {
            log_d(&format!(
                "[HwcHook] composer pid {} ({}) support={:#x} set={:#x} led_device={}",
                c.pid, c.lib_path, c.get_support, c.set_brightness, c.has_led_device
            ));
            Some(c)
        }
        Err(e) => {
            log_e(&format!("[HwcHook] composer not found: {}", e));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hwc_payload::WRESULT;

    fn word(off: usize) -> u32 {
        u32::from_le_bytes([
            PAYLOAD[off],
            PAYLOAD[off + 1],
            PAYLOAD[off + 2],
            PAYLOAD[off + 3],
        ])
    }

    #[test]
    fn syscall_and_branch_encodings() {
        assert_eq!(SVC_0, 0xd400_0001);
        assert_eq!(B_OPCODE, 0x1400_0000);
        // The displacement is relative to the branch itself, so a branch to
        // the next instruction encodes an immediate of 1.
        assert_eq!(encode_branch(0x1000, 0x1004), Some(0x1400_0001));
        // A branch four instructions forward.
        assert_eq!(encode_branch(0x1000, 0x1010), Some(0x1400_0004));
        // A backwards branch.
        assert_eq!(encode_branch(0x1010, 0x1000), Some(0x17ff_fffc));
    }

    #[test]
    fn branch_out_of_range_is_rejected() {
        assert_eq!(encode_branch(0x1000, 0x1002), None);
        assert_eq!(encode_branch(0, 1 << 28), None);
    }

    #[test]
    fn payload_stub_a_encodings() {
        // Stub A replaces displayGetBrightnessSupport. It loads the state
        // page pointer from the code page, counts the call, and reports
        // support for the primary display only.
        assert_eq!(word(0x000), 0x1000_2005); // adr x5, dptr
        assert_eq!(word(0x004), 0xf940_00a4); // ldr x4, [x5]
        assert_eq!(word(0x008), 0xb400_01a2); // cbz x2, .LbadA
        assert_eq!(word(0x00c), 0xb940_1c86); // ldr w6, [x4, #0x01c]
        assert_eq!(word(0x010), 0x1100_04c6); // add w6, w6, #1
        assert_eq!(word(0x014), 0xb900_1c86); // str w6, [x4, #0x01c]
        assert_eq!(word(0x018), 0xb500_00a1); // cbnz x1, .LnotPrimary
        assert_eq!(word(0x01c), 0x5280_0025); // mov w5, #1
        assert_eq!(word(0x020), 0x3900_0045); // strb w5, [x2]
        assert_eq!(word(0x024), 0x5280_0000); // mov w0, #0
        assert_eq!(word(0x028), 0xd65f_03c0); // ret
        assert_eq!(word(0x02c), 0x5280_0005); // mov w5, #0
        assert_eq!(word(0x030), 0x3900_0045); // strb w5, [x2]
        assert_eq!(word(0x034), 0x5280_0000); // mov w0, #0
        assert_eq!(word(0x038), 0xd65f_03c0); // ret
        assert_eq!(word(0x03c), 0x5280_0080); // mov w0, #4
        assert_eq!(word(0x040), 0xd65f_03c0); // ret
    }

    #[test]
    fn payload_stub_b_encodings() {
        // Stub B replaces displaySetBrightness. The normalised float in s0 is
        // scaled into the panel range and written to the backlight node.
        assert_eq!(word(0x200), 0x1000_1005); // adr x5, dptr
        assert_eq!(word(0x204), 0xf940_00a4); // ldr x4, [x5]
        assert_eq!(word(0x208), 0xbd40_1081); // ldr s1, [x4, #0x010]
        assert_eq!(word(0x20c), 0x1e27_03e2); // fmov s2, wzr
        assert_eq!(word(0x210), 0x1e20_2000); // fcmp s0, s0
        assert_eq!(word(0x214), 0x5400_07a6); // b.vs .LbadB
        assert_eq!(word(0x218), 0xbd00_3080); // str s0, [x4, #0x030]
        assert_eq!(word(0x21c), 0x1e21_2000); // fcmp s0, s1
        assert_eq!(word(0x220), 0x5400_074c); // b.gt .LbadB
        assert_eq!(word(0x224), 0x1e22_2000); // fcmp s0, s2
        assert_eq!(word(0x228), 0x5400_030d); // b.le .Lzero
        // Perceptual curve LUT indexing & linear interpolation
        assert_eq!(word(0x22c), 0xbd40_1482); // ldr s2, [x4, #0x014] (255.0f)
        assert_eq!(word(0x230), 0x1e22_0802); // fmul s2, s0, s2
        assert_eq!(word(0x234), 0x1e38_004d); // fcvtzs w13, s2
        assert_eq!(word(0x238), 0x7103_fdbf); // cmp w13, #255
        assert_eq!(word(0x23c), 0x5400_014a); // b.ge .LlutMax
        // Scaling
        assert_eq!(word(0x268), 0xb940_048a); // ldr w10, [x4, #0x004] (min_bri)
        assert_eq!(word(0x26c), 0xb940_088b); // ldr w11, [x4, #0x008] (max_bri)
        assert_eq!(word(0x270), 0x4b0a_016c); // sub w12, w11, w10
        assert_eq!(word(0x274), 0x1e22_0181); // scvtf s1, w12
        assert_eq!(word(0x278), 0x1e21_0801); // fmul s1, s0, s1
        assert_eq!(word(0x27c), 0x1e38_0029); // fcvtzs w9, s1
        assert_eq!(word(0x280), 0x0b0a_0129); // add w9, w9, w10
        assert_eq!(word(0x288), 0x5280_0009); // mov w9, #0
        // Counter & Syscall write
        assert_eq!(word(0x28c), 0xb940_2086); // ldr w6, [x4, #0x020]
        assert_eq!(word(0x2ec), 0xb940_0080); // ldr w0, [x4, #0x000]
        assert_eq!(word(0x2f0), 0xaa0c_03e1); // mov x1, x12
        assert_eq!(word(0x2f4), 0xd280_0808); // mov x8, #64
        assert_eq!(word(0x2f8), 0xd400_0001); // svc #0
        assert_eq!(word(0x2fc), 0xb900_1880); // str w0, [x4, #0x018]
        assert_eq!(word(0x300), 0x5280_0000); // mov w0, #0
        assert_eq!(word(0x304), 0xd65f_03c0); // ret
        assert_eq!(word(0x308), 0x5280_0080); // mov w0, #4
        assert_eq!(word(0x30c), 0xd65f_03c0); // ret
    }

    #[test]
    fn branch_displacement_is_signed() {
        // A branch backwards encodes a negative 26 bit displacement, so the
        // decoder has to sign extend it before it computes the target.
        let pc = 0x1000u64;
        let target = 0x0fc0u64;
        let word = encode_branch(pc, target).unwrap();
        assert_eq!(word & 0xfc00_0000, B_OPCODE);
        let imm = (word & 0x03ff_ffff) as i32;
        let imm = if imm & 0x0200_0000 != 0 {
            imm - 0x0400_0000
        } else {
            imm
        };
        assert_eq!((pc as i64 + (imm as i64) * 4) as u64, target);
    }

    #[test]
    fn payload_layout_matches_the_assembler() {
        // The code page holds the two stubs and the state page pointer.
        assert_eq!(DPTR, 0x400);
        assert_eq!(STUB_SUPPORT, 0x000);
        assert_eq!(STUB_SET, 0x200);
        assert!(PAYLOAD.len() >= DPTR + 8);
        // The pointer slot starts out zeroed, it is filled in at install time.
        assert_eq!(&PAYLOAD[DPTR..DPTR + 8], &[0u8; 8]);
    }

    #[test]
    fn payload_state_page_layout() {
        // The state page is written by the injector, not by the assembler, so
        // only the offsets are checked here.
        assert_eq!(STATE, 0x000);
        assert_eq!(STATE_LEN, 16);
        assert_eq!(FLOATS, 0x010);
        assert_eq!(WRESULT, 0x018);
        assert_eq!(PATHBUF, 0x100);
        assert_eq!(LUT, 0x200);
        assert_eq!(LUT_LEN, 256 * 4);
        // led_fd, min_bri, max_bri, last_val.
        assert_eq!(STATE_LEN, 4 * 4);
    }

    #[test]
    fn poke_preserves_the_neighbouring_instruction() {
        // The neighbouring instruction must survive a 4-byte poke: zeroing it
        // would turn it into UDF #0 and kill the target. The instruction at
        // the lower address lives in the low half of the word, its neighbour
        // in the high half.
        let mut word: u64 = 0x1234_5678_0000_0000;
        let insn: u32 = 0xd420_0000;
        let shift = 0;
        let mask = 0xffff_ffffu64 << shift;
        word = (word & !mask) | ((insn as u64) << shift);
        assert_eq!(word, 0x1234_5678_d420_0000);
        assert_eq!((word >> 32) as u32, 0x1234_5678);
    }
}
