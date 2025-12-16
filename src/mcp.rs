use crate::common::{get_luigi_ai, get_mailbox_address, set_memory_writable, write_memory};
use crate::generated::CellId;
use crate::types::{LuigiEntity, LuigiItem, LuigiMachineHacking, LuigiProp, LuigiTile, MapType};
use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::mem;
use std::thread;
use std::time::Duration;
use crate::generated::CellId::NO_CELL;
use crate::get_presence;

pub struct McpServer {
    handle: ProcessHandle,
    mailbox_address: Option<usize>,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Deserialize)]
struct CallToolParams {
    name: String,
    arguments: Option<Value>,
}

#[repr(C)]
struct StatmindMailbox {
    magic: u32,
    magic2: u32,
    command: u8,
    data: u8,
    padding: [u8; 2],
}

#[derive(Serialize)]
struct SerializableGameState {
    action_ready: i32,
    map_width: i32,
    map_height: i32,
    location: String,
    map_cursor_index: i32,
    player: LuigiEntity,
    machine_hacking: Option<LuigiMachineHacking>,
    map: Vec<Vec<SerializableTile>>,
}

#[derive(Serialize)]
struct SerializableTile {
    last_action: i32,
    last_fov: i32,
    cell: CellId,
    door_open: bool,
    prop: Option<LuigiProp>,
    entity: Option<LuigiEntity>,
    item: Option<LuigiItem>,
    x: i32,
    y: i32,
}

impl McpServer {
    pub fn new(handle: ProcessHandle) -> Self {
        Self {
            handle,
            mailbox_address: None,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            eprintln!("Received: {}", line);

            let req: JsonRpcRequest = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(e) => {
                    eprintln!("Failed to parse JSON: {}", e);
                    continue;
                }
            };

            let response = self.process_request(req);

            if let Some(res) = response {
                let json_str = serde_json::to_string(&res)?;
                stdout.write_all(json_str.as_bytes())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }

    fn process_request(&mut self, req: JsonRpcRequest) -> Option<Value> {
        let id = req.id.clone();

        if id.is_none() {
            return None;
        }

        match req.method.as_str() {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "statmind-mcp", "version": "0.1.0" }
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": self.get_tools() }
            })),
            "tools/call" => {
                let params: CallToolParams =
                    match serde_json::from_value(req.params.unwrap_or(Value::Null)) {
                        Ok(p) => p,
                        Err(_) => return Some(self.error_response(id, -32602, "Invalid params")),
                    };

                let result = if params.name == "get_game_state" {
                    self.get_game_state().map(|gs| json!(gs))
                } else {
                    self.execute_tool(&params.name).map(|s| json!(s))
                };

                match result {
                    Ok(content) => Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "content": [{ "type": "text", "text": content.to_string() }] }
                    })),
                    Err(e) => Some(self.error_response(id, -32000, &e.to_string())),
                }
            }
            _ => Some(self.error_response(id, -32601, "Method not found")),
        }
    }

    fn error_response(&self, id: Option<Value>, code: i32, message: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        })
    }

    fn get_tools(&self) -> Value {
        json!([
            { "name": "move_north", "description": "Move Cogmind North", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_northeast", "description": "Move Cogmind Northeast", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_east", "description": "Move Cogmind East", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_southeast", "description": "Move Cogmind Southeast", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_south", "description": "Move Cogmind South", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_southwest", "description": "Move Cogmind Southwest", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_west", "description": "Move Cogmind West", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "move_northwest", "description": "Move Cogmind Northwest", "inputSchema": { "type": "object", "properties": {} } },
            //{ "name": "pickup", "description": "Pickup item (g)", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "attach", "description": "Attach/equip item from ground (a)", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "fire", "description": "Fire weapon (f)", "inputSchema": { "type": "object", "properties": {} } },
            { "name": "get_game_state", "description": "Get the current game state as a JSON object", "inputSchema": { "type": "object", "properties": {} } }
        ])
    }

    fn get_game_state(&mut self) -> Result<SerializableGameState> {
        let luigi_ai = get_luigi_ai(&self.handle)?;

        let player: LuigiEntity = self.read_memory_struct(luigi_ai.player as usize)?;
        let machine_hacking: Option<LuigiMachineHacking> = if luigi_ai.machine_hacking == 0 {
            None
        } else {
            Some(self.read_memory_struct(luigi_ai.machine_hacking as usize)?)
        };

        let map_size = (luigi_ai.map_width * luigi_ai.map_height) as usize;
        let map_bytes = copy_address(
            luigi_ai.map_data as usize,
            map_size * mem::size_of::<LuigiTile>(),
            &self.handle,
        )?;

        let tiles: Vec<LuigiTile> = map_bytes
            .chunks_exact(mem::size_of::<LuigiTile>())
            .map(|chunk| {
                let mut arr = [0; mem::size_of::<LuigiTile>()];
                arr.copy_from_slice(chunk);
                unsafe { mem::transmute(arr) }
            })
            .collect();

        let mut serializable_map = Vec::new();
        for y in 0..luigi_ai.map_height {
            let mut row = Vec::new();
            for x in 0..luigi_ai.map_width {
                let index = (y * luigi_ai.map_width + x) as usize;
                let tile = &tiles[index];

                let prop = if tile.prop != 0 {
                    Some(self.read_memory_struct::<LuigiProp>(tile.prop as usize)?)
                } else {
                    None
                };
                let entity = if tile.entity != 0 {
                    Some(self.read_memory_struct::<LuigiEntity>(tile.entity as usize)?)
                } else {
                    None
                };
                let item = if tile.item != 0 {
                    Some(self.read_memory_struct::<LuigiItem>(tile.item as usize)?)
                } else {
                    None
                };

                if tile.cell != NO_CELL {
                    row.push(SerializableTile {
                        last_action: tile.last_action,
                        last_fov: tile.last_fov,
                        cell: tile.cell,
                        door_open: tile.door_open,
                        prop,
                        entity,
                        item,
                        x: y,
                        y: x,
                    });
                }
            }
            if !row.is_empty() {
                serializable_map.push(row);
            }
        }

        Ok(SerializableGameState {
            action_ready: luigi_ai.action_ready,
            map_width: luigi_ai.map_width,
            map_height: luigi_ai.map_height,
            location: get_presence(luigi_ai.location_depth, MapType::try_from(luigi_ai.location_map).unwrap_or(MapType::MapNone)),
            map_cursor_index: luigi_ai.map_cursor_index,
            player,
            machine_hacking,
            map: serializable_map,
        })
    }

    fn read_memory_struct<T: Sized>(&self, address: usize) -> Result<T> {
        let bytes = copy_address(address, mem::size_of::<T>(), &self.handle)?;
        let s: T = unsafe { std::ptr::read(bytes.as_ptr() as *const _) };
        Ok(s)
    }

    fn execute_tool(&mut self, name: &str) -> Result<String> {
        let (cmd, data) = match name {
            "move_north" => ('M', 8),
            "move_northeast" => ('M', 9),
            "move_east" => ('M', 6),
            "move_southeast" => ('M', 3),
            "move_south" => ('M', 2),
            "move_southwest" => ('M', 1),
            "move_west" => ('M', 4),
            "move_northwest" => ('M', 7),
            //"pickup" => ('K', 'g' as u8),
            "attach" => ('K', 'a' as u8),
            "fire" => ('K', 'f' as u8),
            _ => return Err(anyhow!("Unknown tool: {}", name)),
        };

        let initial_state = get_luigi_ai(&self.handle)?.action_ready;
        eprintln!("Initial action_ready: {}", initial_state);

        self.send_to_sdl(cmd, data)?;

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(5);
        while start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(10));
            if let Ok(ai) = get_luigi_ai(&self.handle) {
                if ai.action_ready != initial_state {
                    eprintln!("Action confirmed! New state: {}", ai.action_ready);
                    return Ok(format!(
                        "Action {} completed. State: {} -> {}",
                        name, initial_state, ai.action_ready
                    ));
                }
            }
        }

        Err(anyhow!(
            "Timeout waiting for action update. State stuck at {}",
            initial_state
        ))
    }

    fn send_to_sdl(&mut self, cmd: char, data: u8) -> Result<()> {
        if self.mailbox_address.is_none() {
            eprintln!("Scanning for mailbox...");
            self.mailbox_address = Some(get_mailbox_address(&self.handle)?);
            eprintln!("Mailbox found at 0x{:X}", self.mailbox_address.unwrap());
        }

        let addr = self.mailbox_address.unwrap();
        set_memory_writable(&self.handle, addr, mem::size_of::<StatmindMailbox>())?;

        write_memory(&self.handle, addr + 9, &[data])?;
        write_memory(&self.handle, addr + 8, &[cmd as u8])?;
        eprintln!("Command {:?} with data {:?} written to mailbox.", cmd, data);

        Ok(())
    }

    pub fn initialize_mailbox_address(&mut self) -> Result<()> {
        eprintln!("Scanning for mailbox...");
        self.mailbox_address = Some(get_mailbox_address(&self.handle)?);
        eprintln!("Mailbox found at 0x{:X}", self.mailbox_address.unwrap());
        Ok(())
    }
}
