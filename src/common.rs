use crate::types::LuigiAi;
use anyhow::{anyhow, Error};
use process_memory::{copy_address, ProcessHandle, PutAddress};
use std::{
    io::{self, Write},
    mem,
    sync::{Mutex, OnceLock},
}; // Added Mutex and OnceLock for caching

#[cfg(target_os = "macos")]
use security_framework::authorization::{Authorization, AuthorizationItemSetBuilder, Flags};

// Caches for addresses - thread-safe singletons
static BASE_ADDRESS_CACHE: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
static MAILBOX_ADDRESS_CACHE: OnceLock<Mutex<Option<usize>>> = OnceLock::new();
static IPC_THREAD_STATUS_CACHE: OnceLock<Mutex<Option<(usize, bool)>>> = OnceLock::new();

fn get_base_cache() -> &'static Mutex<Option<usize>> {
    BASE_ADDRESS_CACHE.get_or_init(|| Mutex::new(None))
}

fn get_mailbox_cache() -> &'static Mutex<Option<usize>> {
    MAILBOX_ADDRESS_CACHE.get_or_init(|| Mutex::new(None))
}

fn get_ipc_cache() -> &'static Mutex<Option<(usize, bool)>> {
    IPC_THREAD_STATUS_CACHE.get_or_init(|| Mutex::new(None))
}

// New function: set_memory_writable
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

#[cfg(not(target_os = "macos"))]
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

#[cfg(target_os = "macos")]
fn scan_for_base_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    use mach::kern_return::KERN_SUCCESS;
    use mach::port::mach_port_name_t;
    use mach::vm::mach_vm_region;
    use mach::vm_prot::VM_PROT_READ;
    use mach::vm_region::{
        vm_region_basic_info_data_64_t, vm_region_info_t, VM_REGION_BASIC_INFO_64,
    };
    use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};

    let task = handle.0 as mach_port_name_t;
    let magic_value: i32 = 0x64AD_FA4C;
    let magic_value2: i32 = 0x7953_3ED9;
    let mut address: mach_vm_address_t = 0;
    let mut size: mach_vm_size_t = 0;
    let mut info: vm_region_basic_info_data_64_t = unsafe { mem::zeroed() };

    eprintln!("Scanning memory regions for base address...");
    io::stderr().flush()?;

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

        // Check if readable
        if (info.protection & VM_PROT_READ) != 0 {
            io::stderr().flush().unwrap();
            let chunk_size = 64 * 1024;
            let region_end = address + size;
            let mut current_addr = address;

            while current_addr < region_end {
                let read_size =
                    std::cmp::min(chunk_size, (region_end - current_addr) as usize) as usize;

                // Only scan 32-bit address space for efficiency as game is 32-bit
                if current_addr > 0xFFFFFFFF {
                    break;
                }

                match copy_address(current_addr as usize, read_size, handle) {
                    Ok(bytes) => {
                        for i in (0..bytes.len()).step_by(4) {
                            if i + 4 <= bytes.len() {
                                let val = i32::from_le_bytes(bytes[i..i + 4].try_into()?);
                                if val == magic_value {
                                    let val2 = i32::from_le_bytes(bytes[i + 4..i + 8].try_into()?);
                                    if val2 == magic_value2 {
                                        let base_addr = (current_addr as usize) + i;
                                        eprintln!("Base address found at 0x{:X}", base_addr);
                                        io::stderr().flush()?;
                                        return Ok(base_addr);
                                    }
                                }
                            }
                            io::stderr().flush()?;
                        }
                    }
                    Err(_) => {}
                }
                current_addr += chunk_size as u64;
            }
        }

        address += size;
    }

    Err(anyhow!("Could not find base address"))
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

#[cfg(not(target_os = "macos"))]
pub fn get_base_address(_handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    Err(anyhow!("Not implemented for this OS"))
}

#[cfg(target_os = "macos")]
fn scan_for_mailbox_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    use mach::kern_return::KERN_SUCCESS;
    use mach::port::mach_port_name_t;
    use mach::vm::mach_vm_region;
    use mach::vm_prot::{VM_PROT_READ, VM_PROT_WRITE};
    use mach::vm_region::{
        vm_region_basic_info_data_64_t, vm_region_info_t, VM_REGION_BASIC_INFO_64,
    };
    use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};

    let task = handle.0 as mach_port_name_t;
    // Search for "STAT" byte sequence (matches grep)
    let magic_value: i32 = 0x64AD_FA4D;
    let magic_check_value: i32 = 0x7953_3ED8;

    let mut address: mach_vm_address_t = 0;
    let mut size: mach_vm_size_t = 0;
    let mut info: vm_region_basic_info_data_64_t = unsafe { mem::zeroed() };

    eprintln!("Scanning memory regions for mailbox...");
    io::stderr().flush()?;

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

        // Check if readable
        if (info.protection & VM_PROT_READ) != 0 && info.protection & VM_PROT_WRITE != 0 {
            io::stderr().flush().unwrap();
            let chunk_size = 64 * 1024;
            let region_end = address + size;
            let mut current_addr = address;

            while current_addr < region_end {
                let read_size =
                    std::cmp::min(chunk_size, (region_end - current_addr) as usize) as usize;

                // Only scan 32-bit address space for efficiency as game is 32-bit
                if current_addr > 0xFFFFFFFF {
                    break;
                }

                match copy_address(current_addr as usize, read_size, handle) {
                    Ok(bytes) => {
                        for i in (0..bytes.len()).step_by(4) {
                            if i + 4 <= bytes.len() {
                                let val = i32::from_le_bytes(bytes[i..i + 4].try_into()?);
                                if val == magic_value {
                                    let val2 = i32::from_le_bytes(bytes[i + 4..i + 8].try_into()?);
                                    if val2 == magic_check_value {
                                        let mailbox_addr = (current_addr as usize) + i;
                                        eprintln!("Mailbox found at 0x{:X}", mailbox_addr);
                                        io::stderr().flush()?;
                                        return Ok(mailbox_addr);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
                current_addr += chunk_size as u64;
            }
        }

        address += size;
    }

    Err(anyhow!("Mailbox not found in memory regions"))
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

#[cfg(not(target_os = "macos"))]
pub fn get_mailbox_address(_handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    Err(anyhow!("Not implemented for this OS"))
}

pub fn get_luigi_ai(handle: &ProcessHandle) -> Result<LuigiAi, Error> {
    let bytes = copy_address(get_base_address(handle)?, mem::size_of::<LuigiAi>(), handle)?;

    Ok(LuigiAi::from(&bytes))
}

#[cfg(target_os = "macos")]

pub fn check_ipc_thread_status(handle: &ProcessHandle) -> anyhow::Result<bool, Error> {
    use mach::kern_return::KERN_SUCCESS;

    use mach::port::mach_port_name_t;

    use mach::vm::mach_vm_region;

    use mach::vm_prot::VM_PROT_READ;

    use mach::vm_region::{
        vm_region_basic_info_data_64_t, vm_region_info_t, VM_REGION_BASIC_INFO_64,
    };

    use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};

    let task = handle.0 as mach_port_name_t;

    let magic_value: u32 = 0xBADBEEF1; // THREAD_STATUS_MAGIC
    let magic_check_value = 0x7953_3ED9;

    let mut address: mach_vm_address_t = 0;

    let mut size: mach_vm_size_t = 0;

    let mut info: vm_region_basic_info_data_64_t = unsafe { mem::zeroed() };

    eprintln!("Scanning memory regions for IPC thread status...");

    io::stderr().flush().unwrap();

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

        if (info.protection & VM_PROT_READ) != 0 {
            io::stderr().flush().unwrap();

            let chunk_size = 64 * 1024;

            let region_end = address + size;

            let mut current_addr = address;

            while current_addr < region_end {
                let read_size =
                    std::cmp::min(chunk_size, (region_end - current_addr) as usize) as usize;

                if current_addr > 0xFFFFFFFF {
                    break;
                }

                match copy_address(current_addr as usize, read_size, handle) {
                    Ok(bytes) => {
                        for i in (0..bytes.len()).step_by(4) {
                            if i + 4 <= bytes.len() {
                                let val = u32::from_le_bytes(bytes[i..i + 4].try_into()?);

                                if val == magic_value {
                                    let val2 = u32::from_le_bytes(bytes[i + 4..i + 8].try_into()?);
                                    if val2 == magic_check_value {
                                        let status_address = (current_addr as usize) + i + 8; // Offset to status

                                        eprintln!("IPC thread status magic found at 0x{:X}, reading status from 0x{:X}", (current_addr as usize) + i, status_address);

                                        io::stderr().flush().unwrap();

                                        match copy_address(
                                            status_address,
                                            mem::size_of::<i32>(),
                                            handle,
                                        ) {
                                            Ok(status_bytes) => {
                                                let status = i32::from_le_bytes(
                                                    status_bytes.try_into().unwrap(),
                                                );

                                                eprintln!("IPC thread status: {}", status);

                                                io::stderr().flush().unwrap();

                                                return Ok(status == 1);
                                            }

                                            Err(e) => {
                                                eprintln!("Error reading IPC thread status: {}", e);

                                                io::stderr().flush().unwrap();

                                                return Err(anyhow!(
                                                    "Error reading IPC thread status: {}",
                                                    e
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    Err(_) => {}
                }

                current_addr += chunk_size as u64;
            }
        }

        address += size;
    }

    Err(anyhow!(
        "IPC thread status magic not found in memory regions"
    ))
}

#[cfg(not(target_os = "macos"))]
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
