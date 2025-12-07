#[macro_use]
extern crate log;

mod discord;
mod generated;
mod types;

use crate::discord::PresenceProvider;
use crate::types::{LuigiAi, MapType};
use anyhow::{anyhow, Error};
use discord_rich_presence::DiscordIpc;
use env_logger::Env;
use read_process_memory::copy_address;
use read_process_memory::{Pid, ProcessHandle};
#[cfg(target_os = "macos")]
use security_framework::authorization::{Authorization, AuthorizationItemSetBuilder, Flags};
use std::{mem, thread, time};
use sysinfo::{ProcessesToUpdate, System};

fn main() -> anyhow::Result<()> {
    // Init logger
    let env = Env::default()
        .filter_or("MY_LOG_LEVEL", "info")
        .write_style_or("MY_LOG_STYLE", "always");
    env_logger::init_from_env(env);

    // Get debug introspection (taskport) right on macOS
    #[cfg(target_os = "macos")]
    acquire_taskport_right()?;

    // Create a new System object and refresh process list
    let mut sys = System::new_all();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    // Find the process
    let native_process = sys
        .processes()
        .iter()
        .find(|(_, proc)| {
            proc.name()
                .to_string_lossy()
                .to_lowercase()
                .contains("cogmind.exe")
                && proc
                    .cmd()
                    .iter()
                    .any(|arg| arg.to_string_lossy() == "-luigiAi")
        })
        .map(|(_, proc)| proc);
    let wine_process = sys
        .processes()
        .iter()
        .find(|(_, proc)| {
            proc.name()
                .to_string_lossy()
                .to_lowercase()
                .contains("wine")
                && proc
                    .cmd()
                    .iter()
                    .any(|arg| arg.to_string_lossy() == "-luigiAi")
        })
        .map(|(_, proc)| proc);
    let process = native_process.or(wine_process);

    if let Some(process) = process {
        debug!("Opening handle to process...");
        // Get a handle to the process
        let pid = process.pid().as_u32() as Pid;
        let handle: ProcessHandle = (pid).try_into()?;

        let mut presence = PresenceProvider::try_init()?;

        loop {
            debug!("Reading Cogmind process memory...");
            let map_string = get_luigi_map(&handle)?;
            let result = presence
                .client
                .set_activity(presence.activity.clone().state(&map_string));
            match result {
                Ok(_) => {
                    info!("State updated! {}", map_string);
                    thread::sleep(time::Duration::from_secs(60));
                }
                Err(e) => {
                    error!("Error updating state:\n{}", e);
                    thread::sleep(time::Duration::from_secs(5));
                }
            }
        }
    } else {
        error!("No process found...");
    }
    Ok(())
}

fn get_presence(depth: i32, map_type: MapType) -> String {
    let map = match map_type {
        MapType::MapNone => "None",
        MapType::MapYrd => "Scrapyard",
        MapType::MapMat => "Materials",
        MapType::MapFac => "Factory",
        MapType::MapRes => "Research",
        MapType::MapAcc => "Access",
        MapType::MapSur => "Surface",
        MapType::MapMin => "Mines",
        MapType::MapExi => "Exiles",
        MapType::MapSto => "Storage",
        MapType::MapRec => "Recycling",
        MapType::MapWas => "Waste",
        MapType::MapGar => "Garrison",
        MapType::MapLow => "Lower Caves",
        MapType::MapUpp => "Upper Caves",
        MapType::MapPro => "Proximity Caves",
        MapType::MapDee => "Deep Caves",
        MapType::MapZio => "Zion",
        MapType::MapDat => "Data Miner",
        MapType::MapZhi => "Zhirov",
        MapType::MapWar => "Warlord",
        MapType::MapExt => "Extension",
        MapType::MapCet => "Cetus",
        MapType::MapArc => "Archives",
        MapType::MapHub => "Hub_04(d)",
        MapType::MapArm => "Armory",
        MapType::MapLab => "Lab",
        MapType::MapQua => "Quarantine",
        MapType::MapTes => "Testing",
        MapType::MapSec => "Section 7",
        MapType::MapCom => "Command",
        MapType::MapAc0 => "Access_0",
        MapType::MapLai => "Lair",
        MapType::MapTow => "Wartown",
        MapType::MapDsf => "DSF",
        MapType::MapSub => "Subcaves",
        MapType::MapScr => "Scraptown",
        MapType::MapFrg => "Protoforge",
        MapType::MapW00 => "To Epsilon Eridani",
        MapType::MapW01 => "0b11 Command",
        MapType::MapW02 => "To Fleet Rendezvous",
        MapType::MapW03 => "Tau Ceti IV Orbit",
        MapType::MapW04 => "0b10 Command",
        MapType::MapW05 => "Subspace",
        MapType::MapW06 => "Near Former Tau Ceti IV",
        MapType::MapW07 => "Free Derelict Territories",
        MapType::MapW08 => "By MAIN.C's Side",
        MapType::MapW09 => "0bPrime",
    };

    format!("Current map: {}/{}", depth, map)
}

fn get_base_address(handle: &ProcessHandle) -> anyhow::Result<usize, Error> {
    let check_value = 0x64AD_FA4C;
    let start_address = 0xC0_0000;
    let end_address = 0xFFFF_FFFF; // Assuming a 32-bit address space for simplicity

    // Iterate over each possible address starting from 0x40000
    for address in (start_address..=end_address).step_by(mem::size_of::<u32>()) {
        // Attempt to copy 4 bytes (size of u32) from the current address
        match copy_address(address, mem::size_of::<u32>(), handle) {
            Ok(bytes) if bytes.len() == mem::size_of::<u32>() => {
                // Convert the bytes to an i32 using little-endian byte order
                let val = i32::from_le_bytes(bytes.try_into().unwrap());

                // Check if the value matches the check_value
                if val == check_value {
                    // If it matches, return the current address
                    return Ok(address);
                }
            }
            Ok(_) => {
                // If we did not get exactly 4 bytes, continue to the next address
                continue;
            }
            Err(e) => {
                eprintln!("Error reading from address 0x{:X}: {}", address, e);
                continue;
            }
        }
    }

    // If we reach this point, it means we did not find the check_value at any address
    Err(anyhow!("Could not find base address"))
}

fn get_luigi_map(handle: &ProcessHandle) -> Result<String, Error> {
    let bytes = copy_address(get_base_address(handle)?, mem::size_of::<LuigiAi>(), handle)?;
    let val: LuigiAi = LuigiAi::from(&bytes);
    let map_type =
        MapType::try_from(val.location_map).map_err(|_e| anyhow!("Failed to convert map type!"))?;
    Ok(get_presence(val.location_depth, map_type))
}

#[cfg(target_os = "macos")]
fn acquire_taskport_right() -> security_framework::base::Result<Authorization> {
    let rights = AuthorizationItemSetBuilder::new()
        .add_right("system.privilege.taskport")?
        .build();
    Authorization::new(
        Some(rights),
        None,
        Flags::EXTEND_RIGHTS | Flags::INTERACTION_ALLOWED | Flags::PREAUTHORIZE,
    )
}
