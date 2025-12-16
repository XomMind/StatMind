#[macro_use]
extern crate log;

mod common;
mod discord;
mod generated;
mod mcp;
mod types;

use crate::discord::PresenceProvider;
use crate::mcp::McpServer;
use crate::types::MapType;
use anyhow::{anyhow, Error};
use clap::Parser;
use discord_rich_presence::DiscordIpc;
use env_logger::Env;
use process_memory::{Architecture, ProcessHandle};
use std::{io::{self, Write}, thread, time};
use sysinfo::{ProcessesToUpdate, System};

#[cfg(target_os = "macos")]
use mach::traps::{mach_task_self, task_for_pid};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long)]
    mcp: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Init logger
    let env = Env::default()
        .filter_or("MY_LOG_LEVEL", "info")
        .write_style_or("MY_LOG_STYLE", "always");
    env_logger::init_from_env(env);

    // Get debug introspection (taskport) right on macOS
    #[cfg(target_os = "macos")]
    common::acquire_taskport_right()?;

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
        let pid = process.pid().as_u32() as i32;
        
        #[cfg(target_os = "macos")]
        let task_port = {
            use mach::kern_return::KERN_SUCCESS;
            let mut port: mach::port::mach_port_name_t = 0;
            unsafe {
                let res = task_for_pid(mach_task_self(), pid, &mut port);
                if res != KERN_SUCCESS {
                    eprintln!("Failed to get task port for PID {}: {}", pid, res);
                    io::stderr().flush().unwrap();
                    return Err(anyhow::anyhow!("Failed to get task port for PID {}: {}", pid, res));
                }
            }
            port
        };
        #[cfg(not(target_os = "macos"))]
        let task_port = pid as u32;

        // Get a handle to the process
        let handle: ProcessHandle = (task_port, Architecture::Arch32Bit);

        #[cfg(target_os = "macos")]
        if !common::check_ipc_thread_status(&handle)? {
            eprintln!("Error: SDL IPC thread did not start correctly.");
            std::process::exit(4);
        }

        if args.mcp {
            info!("Starting MCP Server...");
            let mut server = McpServer::new(handle);
            if let Err(e) = server.initialize_mailbox_address() {
                eprintln!("Error initializing mailbox: {}", e);
                io::stderr().flush().unwrap();
                std::process::exit(3);
            }
            info!("Started MCP Server...");
            server.run()?;
        } else {
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

fn get_luigi_map(handle: &ProcessHandle) -> Result<String, Error> {
    let val = common::get_luigi_ai(handle)?;
    let map_type =
        MapType::try_from(val.location_map).map_err(|_e| anyhow!("Failed to convert map type!"))?;
    Ok(get_presence(val.location_depth, map_type))
}
