//! Differential value scanning, for finding globals LuigiAI does not expose.
//!
//! Beta 17.1 leaves `LuigiAi.actionReady` and `LuigiAi.player` permanently zero,
//! so the turn clock and the player's own state have to be located in the game's
//! structures directly. Both are displayed on the HUD, which makes them findable
//! the classic way: scan for a known value, change it, rescan, intersect.
//!
//! Candidates live in the server rather than being round-tripped, because a first
//! pass on a small integer legitimately matches millions of addresses.

use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};

/// Only the low 4 GiB is interesting: Cogmind is a 32-bit PE under Wine.
const ADDR_LIMIT: u64 = 0xFFFF_FFFF;
const CHUNK: usize = 256 * 1024;
/// A first pass on a value like `4` can match a great many addresses. Cap the
/// working set so a stray scan cannot exhaust memory.
const MAX_CANDIDATES: usize = 4_000_000;

#[cfg(target_os = "macos")]
fn writable_regions(handle: &ProcessHandle) -> Result<Vec<(u64, u64)>> {
    use mach::kern_return::KERN_SUCCESS;
    use mach::port::mach_port_name_t;
    use mach::vm::mach_vm_region;
    use mach::vm_prot::{VM_PROT_READ, VM_PROT_WRITE};
    use mach::vm_region::{
        vm_region_basic_info_data_64_t, vm_region_info_t, VM_REGION_BASIC_INFO_64,
    };
    use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};
    use std::mem;

    let task = handle.0 as mach_port_name_t;
    let mut address: mach_vm_address_t = 0;
    let mut size: mach_vm_size_t = 0;
    let mut out = Vec::new();

    loop {
        let mut info: vm_region_basic_info_data_64_t = unsafe { mem::zeroed() };
        let mut count =
            (mem::size_of::<vm_region_basic_info_data_64_t>() / mem::size_of::<i32>()) as u32;
        let mut object_name: mach_port_name_t = 0;
        let ret = unsafe {
            mach_vm_region(
                task,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                (&mut info as *mut _) as vm_region_info_t,
                &mut count,
                &mut object_name,
            )
        };
        if ret != KERN_SUCCESS {
            break;
        }
        if address > ADDR_LIMIT {
            break;
        }
        if (info.protection & VM_PROT_READ) != 0 && (info.protection & VM_PROT_WRITE) != 0 {
            let end = (address + size).min(ADDR_LIMIT);
            if end > address {
                out.push((address, end));
            }
        }
        address += size.max(1);
    }
    Ok(out)
}

#[cfg(not(target_os = "macos"))]
fn writable_regions(_handle: &ProcessHandle) -> Result<Vec<(u64, u64)>> {
    Err(anyhow!(
        "region enumeration is macOS-only; a Linux port needs /proc/<pid>/maps"
    ))
}

/// Fresh scan across every writable region. Returns candidate addresses holding
/// `value` as a little-endian i32, 4-byte aligned.
pub fn scan_new(handle: &ProcessHandle, value: i32) -> Result<Vec<usize>> {
    let needle = value.to_le_bytes();
    let mut hits = Vec::new();

    for (start, end) in writable_regions(handle)? {
        let mut addr = start;
        while addr < end {
            let want = CHUNK.min((end - addr) as usize);
            if let Ok(buf) = copy_address(addr as usize, want, handle) {
                let mut i = 0;
                while i + 4 <= buf.len() {
                    if buf[i] == needle[0]
                        && buf[i + 1] == needle[1]
                        && buf[i + 2] == needle[2]
                        && buf[i + 3] == needle[3]
                    {
                        hits.push(addr as usize + i);
                        if hits.len() >= MAX_CANDIDATES {
                            return Ok(hits);
                        }
                    }
                    i += 4;
                }
            }
            addr += want as u64;
        }
    }
    Ok(hits)
}

/// Keep only the candidates that now hold `value`. This is the step that makes
/// the technique work: an address must hold the old value *and then* the new one.
pub fn scan_filter(handle: &ProcessHandle, cands: &[usize], value: i32) -> Vec<usize> {
    let needle = value.to_le_bytes();
    cands
        .iter()
        .copied()
        .filter(|&a| {
            copy_address(a, 4, handle)
                .map(|b| b[..] == needle[..])
                .unwrap_or(false)
        })
        .collect()
}

/// Keep candidates whose value *changed* (any new value), reporting the new
/// values. Useful when the new value is not known in advance -- e.g. finding the
/// player's coordinates by moving and seeing which pair moved with you.
pub fn scan_changed(
    handle: &ProcessHandle,
    cands: &[usize],
    old: i32,
) -> Vec<(usize, i32)> {
    cands
        .iter()
        .copied()
        .filter_map(|a| {
            let b = copy_address(a, 4, handle).ok()?;
            let v = i32::from_le_bytes(b.try_into().ok()?);
            if v != old {
                Some((a, v))
            } else {
                None
            }
        })
        .collect()
}

/// Find 4-byte-aligned words whose value lands inside `[lo, hi]`.
///
/// For pointer chaining. An exact-value pointer scan usually finds nothing,
/// because pointers point at an object's start while the field of interest sits
/// somewhere in the middle -- so scan for a *range* covering the object instead.
/// Returns (referencing address, the pointer value it holds).
pub fn scan_ptr_to(handle: &ProcessHandle, lo: u32, hi: u32) -> Result<Vec<(usize, u32)>> {
    let mut hits = Vec::new();
    for (start, end) in writable_regions(handle)? {
        let mut addr = start;
        while addr < end {
            let want = CHUNK.min((end - addr) as usize);
            if let Ok(buf) = copy_address(addr as usize, want, handle) {
                let mut i = 0;
                while i + 4 <= buf.len() {
                    let v = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
                    if v >= lo && v <= hi {
                        hits.push((addr as usize + i, v));
                        if hits.len() >= 200_000 {
                            return Ok(hits);
                        }
                    }
                    i += 4;
                }
            }
            addr += want as u64;
        }
    }
    Ok(hits)
}

/// Keep candidates that have *all* of `values` as i32s within `window` bytes
/// either side.
///
/// Temporal narrowing needs a value you can change on demand. Some do not
/// qualify -- matter only moves when you pick some up. But related stats live
/// together in one struct, so requiring several known HUD numbers to co-occur in
/// a small window narrows just as hard without touching the game.
pub fn scan_context(
    handle: &ProcessHandle,
    cands: &[usize],
    values: &[i32],
    window: usize,
) -> Vec<(usize, Vec<(i32, i32)>)> {
    let mut out = Vec::new();
    for &a in cands {
        let lo = a.saturating_sub(window);
        let span = window * 2 + 4;
        let Ok(buf) = copy_address(lo, span, handle) else { continue };
        let words: Vec<i32> = buf
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        // Where does each wanted value sit, relative to the candidate?
        let mut found: Vec<(i32, i32)> = Vec::new();
        let mut all = true;
        for &v in values {
            match words.iter().position(|&w| w == v) {
                Some(i) => {
                    let off = (lo + i * 4) as isize - a as isize;
                    found.push((v, off as i32));
                }
                None => {
                    all = false;
                    break;
                }
            }
        }
        if all {
            out.push((a, found));
        }
    }
    out
}

/// Read a handful of i32s at once, for inspecting a candidate's neighbourhood.
pub fn read_window(handle: &ProcessHandle, addr: usize, words: usize) -> Result<Vec<i32>> {
    let bytes = copy_address(addr, words * 4, handle)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}
