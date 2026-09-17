use crate::types::LuigiAi;
use anyhow::{anyhow, Error};
use process_memory::{copy_address, ProcessHandle, PutAddress};
use std::{
    io::{self, Write},
    mem,
    sync::{Mutex, OnceLock},
};

#[cfg(target_os = "macos")]
use security_framework::authorization::{Authorization, AuthorizationItemSetBuilder, Flags};

// Caches for addresses - thread-safe singletons
static BASE_ADDRESS_CACHE: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
static MAILBOX_ADDRESS_CACHE: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

fn get_base_cache() -> &'static Mutex<Option<usize>> {
    BASE_ADDRESS_CACHE.get_or_init(|| Mutex::new(None))
}

fn get_mailbox_cache() -> &'static Mutex<Option<usize>> {
    MAILBOX_ADDRESS_CACHE.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "linux")]
fn parse_linux_regions(maps: &str, writable: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some(perms) = fields.next() else { continue };
        if !perms.starts_with('r')
            || (writable && !perms.as_bytes().get(1).is_some_and(|b| *b == b'w'))
        {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let Ok(start) = usize::from_str_radix(start, 16) else {
            continue;
        };
        let Ok(end) = usize::from_str_radix(end, 16) else {
            continue;
        };
        if start <= u32::MAX as usize {
            out.push((start, end.min(u32::MAX as usize)));
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn linux_regions(handle: &ProcessHandle, writable: bool) -> anyhow::Result<Vec<(usize, usize)>> {
    let maps = std::fs::read_to_string(format!("/proc/{}/maps", handle.0))?;
    Ok(parse_linux_regions(&maps, writable))
}

#[cfg(target_os = "linux")]
fn scan_linux_magic(
    handle: &ProcessHandle,
    magic: u32,
    check: u32,
    writable: bool,
) -> anyhow::Result<usize> {
    let needle = [magic.to_le_bytes(), check.to_le_bytes()].concat();
    for (start, end) in linux_regions(handle, writable)? {
        let mut address = start;
        while address < end {
            let size = (64 * 1024).min(end - address);
            if let Ok(bytes) = copy_address(address, size, handle) {
                for offset in (0..bytes.len().saturating_sub(7)).step_by(4) {
                    if bytes[offset..offset + 8] == needle {
                        return Ok(address + offset);
                    }
                }
            }
            address += size;
        }
    }
    Err(anyhow!(
        "memory magic 0x{magic:08X}/0x{check:08X} not found"
    ))
}

#[cfg(target_os = "macos")]
pub fn set_memory_writable(
    handle: &ProcessHandle,
    address: usize,
    size: usize,
) -> anyhow::Result<(), Error> {
    use mach::kern_return::KERN_SUCCESS;
    use mach::port::mach_port_name_t;
    use mach::vm::mach_vm_protect; // Correct mach_vm_protect import
    use mach::vm_prot::{VM_PROT_READ, VM_PROT_WRITE};
    use mach::vm_types::mach_vm_address_t;

    let task = handle.0 as mach_port_name_t;
    let addr = address as mach_vm_address_t;
    let sz = size as mach_vm_address_t;

    eprintln!(
        "Attempting to set memory writable for task {:X} at 0x{:X} for size 0x{:X} with prot {:X}",
        task,
        address,
        size,
        VM_PROT_READ | VM_PROT_WRITE
    );
    io::stderr().flush().unwrap();

    unsafe {
        let ret = mach_vm_protect(task, addr, sz, 0, VM_PROT_READ | VM_PROT_WRITE);
        eprintln!("mach_vm_protect returned: {}", ret);
        io::stderr().flush().unwrap();
        if ret != KERN_SUCCESS {
            return Err(anyhow!("mach_vm_protect failed: {}", ret));
        }
    }
    eprintln!("Successfully set memory writable at 0x{:X}", address);
    io::stderr().flush().unwrap();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_memory_writable(
    handle: &ProcessHandle,
    address: usize,
    size: usize,
) -> anyhow::Result<(), Error> {
    let end = address
        .checked_add(size)
        .ok_or_else(|| anyhow!("address overflow"))?;
    if linux_regions(handle, true)?
        .iter()
        .any(|&(start, stop)| address >= start && end <= stop)
    {
        Ok(())
    } else {
        Err(anyhow!("0x{address:X}..0x{end:X} is not writable"))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn set_memory_writable(
    _handle: &ProcessHandle,
    _address: usize,
    _size: usize,
) -> anyhow::Result<(), Error> {
    Err(anyhow!("set_memory_writable not implemented for this OS"))
}

pub fn write_memory(handle: &ProcessHandle, address: usize, data: &[u8]) -> Result<(), Error> {
    eprintln!(
        "Attempting to write {} bytes to 0x{:X}",
        data.len(),
        address
    );
    io::stderr().flush().unwrap();
    handle
        .put_address(address, data)
        .map_err(|e| anyhow!("Failed to write memory: {:?}", e))
}

/// Walk the target's readable regions for a `magic`/`check` pair and return the
/// address of `magic`. The macOS counterpart of `scan_linux_magic`.
///
/// This replaced three copies that differed only in the constants they looked
/// for. Two of them also indexed `bytes[i + 4..i + 8]` after checking only that
/// `i + 4` was in range, which panics on a magic landing in the last four bytes
/// of a chunk.
#[cfg(target_os = "macos")]
fn scan_macos_magic(
    handle: &ProcessHandle,
    magic: u32,
    check: u32,
    writable: bool,
) -> anyhow::Result<usize, Error> {
    use mach::kern_return::KERN_SUCCESS;
    use mach::port::mach_port_name_t;
    use mach::vm::mach_vm_region;
    use mach::vm_prot::{VM_PROT_READ, VM_PROT_WRITE};
    use mach::vm_region::{
        vm_region_basic_info_data_64_t, vm_region_info_t, VM_REGION_BASIC_INFO_64,
    };
    use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};

    let task = handle.0 as mach_port_name_t;
    let needle = [magic.to_le_bytes(), check.to_le_bytes()].concat();
    let mut address: mach_vm_address_t = 0;
    let mut size: mach_vm_size_t = 0;
    let mut info: vm_region_basic_info_data_64_t = unsafe { mem::zeroed() };

    loop {
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
        let want = VM_PROT_READ | if writable { VM_PROT_WRITE } else { 0 };
        if (info.protection & want) == want {
            let chunk = 64 * 1024usize;
            let region_end = address + size;
            let mut current = address;
            while current < region_end {
                if current > 0xFFFF_FFFF {
                    break;
                }
                // Overlap by the needle so a pair straddling a chunk edge is
                // still found.
                let read = chunk.min((region_end - current) as usize);
                if let Ok(bytes) = copy_address(current as usize, read, handle) {
                    if let Some(off) = bytes
                        .windows(needle.len())
                        .position(|w| w == needle.as_slice())
                    {
                        return Ok(current as usize + off);
                    }
                }
                current += (chunk - needle.len()) as u64;
            }
        }
        address += size;
    }
    Err(anyhow!(
        "magic 0x{magic:08X}/0x{check:08X} not found in memory regions"
    ))
}

#[cfg(target_os = "macos")]
fn scan_for_base_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    scan_macos_magic(handle, 0x64AD_FA4C, 0x7953_3ED9, false)
}

#[cfg(target_os = "macos")]
pub fn get_base_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    let cache = get_base_cache();
    let mut cached = cache.lock().unwrap();

    if let Some(addr) = *cached {
        eprintln!("Using cached base address: 0x{:X}", addr);
        io::stderr().flush().unwrap();
        return Ok(addr);
    }

    // Cache miss - scan for the address
    let addr = scan_for_base_address(handle)?;
    *cached = Some(addr);
    Ok(addr)
}

#[cfg(target_os = "linux")]
pub fn get_base_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    let cache = get_base_cache();
    let mut cached = cache.lock().unwrap();
    if let Some(address) = *cached {
        return Ok(address);
    }
    let address = scan_linux_magic(handle, 0x64AD_FA4C, 0x7953_3ED9, false)?;
    *cached = Some(address);
    Ok(address)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn get_base_address(_handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    Err(anyhow!("get_base_address not implemented for this OS"))
}

#[cfg(target_os = "macos")]
fn scan_for_mailbox_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    scan_macos_magic(handle, 0x64AD_FA4D, 0x7953_3ED8, true)
}

#[cfg(target_os = "macos")]
pub fn get_mailbox_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    let cache = get_mailbox_cache();
    let mut cached = cache.lock().unwrap();

    if let Some(addr) = *cached {
        eprintln!("Using cached mailbox address: 0x{:X}", addr);
        io::stderr().flush().unwrap();
        return Ok(addr);
    }

    // Cache miss - scan for the address
    let addr = scan_for_mailbox_address(handle)?;
    *cached = Some(addr);
    Ok(addr)
}

#[cfg(target_os = "linux")]
pub fn get_mailbox_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    let cache = get_mailbox_cache();
    let mut cached = cache.lock().unwrap();
    if let Some(address) = *cached {
        return Ok(address);
    }
    let address = scan_linux_magic(handle, 0x64AD_FA4D, 0x7953_3ED8, true)?;
    *cached = Some(address);
    Ok(address)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn get_mailbox_address(_handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    Err(anyhow!("get_mailbox_address not implemented for this OS"))
}

pub fn get_luigi_ai(handle: &ProcessHandle) -> Result<LuigiAi, Error> {
    let bytes = copy_address(get_base_address(handle)?, mem::size_of::<LuigiAi>(), handle)?;

    Ok(LuigiAi::from(&bytes))
}

#[cfg(target_os = "macos")]
pub fn check_ipc_thread_status(handle: &ProcessHandle) -> anyhow::Result<bool, Error> {
    let address = scan_macos_magic(handle, 0xBADB_EEF1, 0x7953_3ED9, false)? + 8;
    let bytes = copy_address(address, 4, handle)?;
    Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| anyhow!("short status read"))?) == 1)
}

#[cfg(target_os = "linux")]
pub fn check_ipc_thread_status(handle: &ProcessHandle) -> anyhow::Result<bool, Error> {
    let address = scan_linux_magic(handle, 0xBADB_EEF1, 0x7953_3ED9, false)? + 8;
    let bytes = copy_address(address, 4, handle)?;
    Ok(i32::from_le_bytes(bytes.try_into().map_err(|_| anyhow!("short status read"))?) == 1)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn check_ipc_thread_status(_handle: &ProcessHandle) -> anyhow::Result<bool, Error> {
    Err(anyhow!(
        "check_ipc_thread_status not implemented for this OS"
    ))
}

#[cfg(target_os = "macos")]

pub fn acquire_taskport_right() -> security_framework::base::Result<Authorization> {
    let rights = AuthorizationItemSetBuilder::new()
        .add_right("system.privilege.taskport")?
        .build();

    Authorization::new(
        Some(rights),
        None,
        Flags::EXTEND_RIGHTS | Flags::INTERACTION_ALLOWED | Flags::PREAUTHORIZE,
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::parse_linux_regions;

    #[test]
    fn parses_low_readable_and_writable_proc_maps() {
        let maps = "00400000-00452000 r-xp 00000000 00:00 0\n\
                    00652000-00653000 rw-p 00052000 00:00 0\n\
                    100000000-100001000 rw-p 00000000 00:00 0\n";
        assert_eq!(
            parse_linux_regions(maps, false),
            vec![(0x00400000, 0x00452000), (0x00652000, 0x00653000)]
        );
        assert_eq!(
            parse_linux_regions(maps, true),
            vec![(0x00652000, 0x00653000)]
        );
    }
}

//==================================================================
// Addresses the shim resolved for the running build
//==================================================================
//
// These used to be `const`s in cells.rs and blit.rs. That held only while every
// supported executable agreed on them, which the Steam build ended: its .data
// sits 0x1000 higher and each global moved by a slightly different amount.
//
// The shim resolves the build in-process, where it can fingerprint instructions,
// and publishes the result in StatmindLuigiStatus. This reader takes the
// addresses from there, so `statmind_build.h` stays the only place a build is
// described.

/// Field offsets in StatmindLuigiStatus (SDL-1.2/src/statmind_ipc.h).
const LS_STRUCT_ADDR: usize = 20;
const LS_BUILD_STAMP: usize = 24;
const LS_MAP_OBJECT: usize = 28;
const LS_PLAYER_REC: usize = 32;
const LS_VIEW_ORIGIN: usize = 36;

#[derive(Clone, Copy, Debug)]
pub struct Addrs {
    pub build_stamp: u32,
    pub luigi: usize,
    pub map_object: usize,
    /// None on a build where the record has never been located. It is zero-fill
    /// with nothing referring to it, so there is no way to derive it here; the
    /// reader refuses player queries rather than using another build's value.
    pub player_rec: Option<usize>,
    pub view_origin: usize,
}

static ADDRS_CACHE: OnceLock<Mutex<Option<Addrs>>> = OnceLock::new();

#[cfg(target_os = "macos")]
fn scan_magic(h: &ProcessHandle, m: u32, c: u32, w: bool) -> anyhow::Result<usize> {
    scan_macos_magic(h, m, c, w)
}

#[cfg(target_os = "linux")]
fn scan_magic(h: &ProcessHandle, m: u32, c: u32, w: bool) -> anyhow::Result<usize> {
    scan_linux_magic(h, m, c, w)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn scan_magic(_h: &ProcessHandle, _m: u32, _c: u32, _w: bool) -> anyhow::Result<usize> {
    Err(anyhow!("memory scanning not implemented for this OS"))
}

pub fn get_addrs(handle: &ProcessHandle) -> anyhow::Result<Addrs, Error> {
    let cache = ADDRS_CACHE.get_or_init(|| Mutex::new(None));
    let mut cached = cache.lock().unwrap();
    if let Some(a) = *cached {
        return Ok(a);
    }

    // LUIGI_STATUS_MAGIC / LUIGI_STATUS_CHECK.
    let base = scan_magic(handle, 0xBADB_EEF2, 0x7953_3EDA, true).map_err(|e| {
        anyhow!(
            "no StatmindLuigiStatus in the target ({e}) -- the patched SDL.dll \
             is not loaded, or it predates per-build address publication"
        )
    })?;
    let rd = |off: usize| -> anyhow::Result<u32, Error> {
        let b = copy_address(base + off, 4, handle)?;
        Ok(u32::from_le_bytes(
            b.try_into().map_err(|_| anyhow!("short read"))?,
        ))
    };

    let player_rec = rd(LS_PLAYER_REC)?;
    let addrs = Addrs {
        build_stamp: rd(LS_BUILD_STAMP)?,
        luigi: rd(LS_STRUCT_ADDR)? as usize,
        map_object: rd(LS_MAP_OBJECT)? as usize,
        player_rec: (player_rec != 0).then_some(player_rec as usize),
        view_origin: rd(LS_VIEW_ORIGIN)? as usize,
    };
    if addrs.map_object == 0 || addrs.view_origin == 0 {
        return Err(anyhow!(
            "the shim published no addresses (build stamp 0x{:08X}); it did not \
             recognise this executable",
            addrs.build_stamp
        ));
    }
    *cached = Some(addrs);
    Ok(addrs)
}
