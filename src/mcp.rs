use crate::blit;
use crate::cells;
use crate::common::{get_luigi_ai, get_mailbox_address, set_memory_writable, write_memory};
use crate::generated::{CellId, EntityId, ItemId, PropId};
use crate::get_presence;
use crate::scan;
use crate::scoresheet;
use crate::types::{LuigiEntity, LuigiItem, LuigiMachineHacking, LuigiProp, LuigiTile, MapType};
use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::mem;
use std::thread;
use std::time::Duration;

pub struct McpServer {
    handle: ProcessHandle,
    mailbox_address: Option<usize>,
    /// Cached address of the blit log; the magic scan is slow.
    blit_addr: Option<usize>,
    scoresheet_addr: Option<usize>,
    /// Working set for differential scans. Kept server-side because a first pass
    /// on a small integer can legitimately match millions of addresses.
    scan_candidates: Vec<usize>,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Deserialize)]
struct CallToolParams {
    name: String,
    arguments: Option<Value>,
}

// Mailbox v2 -- field offsets must match SDL-1.2/src/statmind_ipc.h.
// Verified against a compiled `offsetof` dump of that header.
const MB_VERSION: usize = 8;
const MB_SEQ: usize = 12;
const MB_ACK: usize = 16;
const MB_STATUS: usize = 20;
const MB_COMMAND: usize = 24;
const MB_BUTTON: usize = 25;
const MB_KEYSYM: usize = 26;
const MB_MODIFIERS: usize = 28;
const MB_UNICODE: usize = 30;
const MB_REPEAT: usize = 32;
const MB_TEXT_LEN: usize = 34;
const MB_MOUSE_X: usize = 36;
const MB_MOUSE_Y: usize = 40;
const MB_TEXT: usize = 44;
const MB_SIZE: usize = 108;
const MB_TEXT_MAX: usize = 64;
const MB_EXPECT_VERSION: u32 = 2;

// SDL 1.2 modifier bits. We send left-hand variants; the shim also mirrors these
// into SDL_SetModState because Cogmind reads modifiers both ways.
const KMOD_LSHIFT: u16 = 0x0001;
const KMOD_LCTRL: u16 = 0x0040;
const KMOD_LALT: u16 = 0x0100;

// SDL 1.2 keypad, for the eight-way movement shortcuts.
const SDLK_KP0: u16 = 256;

enum MailboxCmd {
    Key {
        keysym: u16,
        mods: u16,
        unicode: u16,
        repeat: u16,
    },
    Text(String),
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseClick {
        x: i32,
        y: i32,
        button: u8,
    },
    /// Ask the shim to call Cogmind's own scoresheet writer on the game's
    /// main thread. `budget_ms` bounds how long the shim waits for a frame
    /// boundary before giving up.
    Dump {
        budget_ms: u16,
    },
}

#[derive(Serialize)]
struct SerializableGameState {
    action_ready: i32,
    map_width: i32,
    map_height: i32,
    location: String,
    map_cursor_index: i32,
    /// Cogmind's own map coordinates, or -1 if the player tile was not found.
    /// Located by matching `tile.entity` against `LuigiAi.player`; entity id
    /// alone is unreliable because allies can share it.
    player_x: i32,
    player_y: i32,
    player: SerializableEntity,
    /// Attached parts and cargo, walked from `LuigiEntity.inventory`.
    /// `equipped` separates the two.
    inventory: Vec<SerializableItem>,
    /// Stride used for the inventory walk; see STATMIND_ITEM_STRIDE.
    item_stride: usize,
    machine_hacking: Option<LuigiMachineHacking>,
    map: Vec<Vec<SerializableTile>>,
}

/// luigiai.h: `cell` is NO_CELL (-1) when unknown, so the raw value is not
/// always a valid enum discriminant. Carry the raw id and a resolved name.
#[derive(Serialize)]
struct SerializableItem {
    name: Option<&'static str>,
    raw_id: i32,
    integrity: i32,
    equipped: Option<bool>,
}

#[derive(Serialize)]
struct SerializableProp {
    name: Option<&'static str>,
    raw_id: i32,
    interactive_piece: bool,
}

#[derive(Serialize)]
struct SerializableEntity {
    name: Option<&'static str>,
    raw_id: i32,
    integrity: i32,
    relation: i32,
    active_state: i32,
    exposure: i32,
    energy: i32,
    matter: i32,
    heat: i32,
    system_corruption: i32,
    speed: i32,
    inventory_size: i32,
}

impl From<&LuigiEntity> for SerializableEntity {
    fn from(e: &LuigiEntity) -> Self {
        Self {
            name: EntityId::from_id(e.id).map(|v| v.name()),
            raw_id: e.id,
            integrity: e.integrity,
            relation: e.relation,
            active_state: e.active_state,
            exposure: e.exposure,
            energy: e.energy,
            matter: e.matter,
            heat: e.heat,
            system_corruption: e.system_corruption,
            speed: e.speed,
            inventory_size: e.inventory_size,
        }
    }
}

/// loadMap() allocates `count * 0x1C + 4` bytes, so the game's LuigiTile is
/// exactly 28 bytes. If this fires, every tile after the first is garbage.
const _: () = assert!(std::mem::size_of::<LuigiTile>() == 28);

#[derive(Serialize)]
struct SerializableTile {
    last_action: i32,
    last_fov: i32,
    cell: Option<&'static str>,
    raw_cell: i32,
    door_open: bool,
    prop: Option<SerializableProp>,
    entity: Option<SerializableEntity>,
    item: Option<SerializableItem>,
    x: i32,
    y: i32,
}

impl McpServer {
    pub fn new(handle: ProcessHandle) -> Self {
        Self {
            handle,
            mailbox_address: None,
            blit_addr: None,
            scoresheet_addr: None,
            scan_candidates: Vec::new(),
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

                let args = params.arguments.clone().unwrap_or(Value::Null);
                let argi = |k: &str, d: i64| args.get(k).and_then(|v| v.as_i64()).unwrap_or(d);

                let result = if params.name == "get_game_state" {
                    self.get_game_state().map(|gs| json!(gs))
                } else if params.name == "get_map" {
                    let bbox = if args.get("x0").is_some() {
                        Some((
                            argi("x0", 0) as i32,
                            argi("y0", 0) as i32,
                            argi("x1", 0) as i32,
                            argi("y1", 0) as i32,
                        ))
                    } else {
                        None
                    };
                    cells::read_map(&self.handle, bbox).map(|m| json!(m))
                } else if params.name == "luigi_raw" {
                    get_luigi_ai(&self.handle).map(|v| {
                        json!({
                            "base_addr": format!("0x{:08X}", crate::common::get_addrs(&self.handle).map(|a| a.luigi).unwrap_or(0)),
                            "magic1_ok": v.magic1 == 1689123404u32 as i32,
                            "magic2_ok": v.magic2 == 2035498713u32 as i32,
                            "action_ready": v.action_ready,
                            "map_width": v.map_width,
                            "map_height": v.map_height,
                            "location_depth": v.location_depth,
                            "location_map": v.location_map,
                            "map_data": format!("0x{:08X}", v.map_data),
                            "map_cursor_index": v.map_cursor_index,
                            "player": format!("0x{:08X}", v.player),
                            "machine_hacking": format!("0x{:08X}", v.machine_hacking),
                            "note": "player/mapData are NULL until a map loads. mapData stays \
                                     empty even then on b17.1 (mirror is stubbed); terrain \
                                     comes from get_map."
                        })
                    })
                } else if params.name == "dump_status" || params.name == "stat_dump" {
                    let found = match self.scoresheet_addr {
                        Some(a) => Ok(a),
                        None => scoresheet::find(&self.handle).map(|a| {
                            self.scoresheet_addr = Some(a);
                            eprintln!("Scoresheet block found at 0x{:X}", a);
                            a
                        }),
                    };
                    match (found, params.name.as_str()) {
                        (Err(e), _) => Err(e),
                        (Ok(a), "dump_status") => {
                            scoresheet::status(&self.handle, a).map(|s| json!(s))
                        }
                        (Ok(a), _) => {
                            let budget = args
                                .get("budget_ms")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(5000)
                                .clamp(100, 60_000) as u16;
                            // Report the resolution failure rather than the
                            // shim's generic rejection: "not Beta 17.1" and
                            // "the game never presented a frame" need
                            // different fixes.
                            match scoresheet::status(&self.handle, a) {
                                Err(e) => Err(e),
                                Ok(before) if before.status != 1 => Err(anyhow!(
                                    "stat_dump unavailable: {} (status {})",
                                    before.status_text,
                                    before.status
                                )),
                                Ok(_) => self
                                    .submit(MailboxCmd::Dump { budget_ms: budget })
                                    .and_then(|note| {
                                        scoresheet::status(&self.handle, a).map(|after| {
                                            json!({
                                                "note": note,
                                                "calls": after.calls,
                                                "result": after.last_result_text,
                                                "returned": after.returned,
                                                "where": "written under the profile's dumps/ \
                                                          directory, .txt plus .json when \
                                                          advanced.cfg has jsonStatDump=1. Does \
                                                          not touch scorehistory.txt, so it \
                                                          cannot look like a completed run."
                                            })
                                        })
                                    }),
                            }
                        }
                    }
                } else if params.name.starts_with("blit_") {
                    let found = match self.blit_addr {
                        Some(a) => Ok(a),
                        None => blit::find(&self.handle).map(|a| {
                            self.blit_addr = Some(a);
                            eprintln!("Blit log found at 0x{:X}", a);
                            a
                        }),
                    };
                    match (found, params.name.as_str()) {
                        (Err(e), _) => Err(e),
                        (Ok(a), "blit_status") => blit::status(&self.handle, a).map(|s| json!(s)),
                        (Ok(a), "blit_enable") => {
                            let on = args.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
                            set_memory_writable(&self.handle, a, 64)
                                .and_then(|_| {
                                    write_memory(
                                        &self.handle,
                                        a + blit::OFF_ENABLED,
                                        &(if on { 1i32 } else { 0i32 }).to_le_bytes(),
                                    )
                                })
                                .and_then(|_| blit::status(&self.handle, a))
                                .map(|s| json!(s))
                        }
                        (Ok(a), "blit_clear") => {
                            // Bump clear_req; the shim resets on its next draw.
                            blit::status(&self.handle, a)
                                .and_then(|st| {
                                    set_memory_writable(&self.handle, a, 64)?;
                                    write_memory(
                                        &self.handle,
                                        a + blit::OFF_CLEAR_REQ,
                                        &(st.clear_req + 1).to_le_bytes(),
                                    )
                                })
                                .and_then(|_| blit::status(&self.handle, a))
                                .map(|s| json!(s))
                        }
                        (Ok(_), "blit_view") => {
                            blit::read_view_origin(&self.handle).map(|v| json!(v))
                        }
                        (Ok(a), "blit_screen") => {
                            // Convert the accumulated draws into game coordinates.
                            // Clamp to the real map, not an arbitrary bound: anything
                            // outside it is HUD, and a loose bound silently mislabels
                            // HUD columns as map cells.
                            let dims = cells::read_header(&self.handle).map(|(w, h, _)| (w, h));
                            blit::read_view_origin(&self.handle).and_then(|vo| {
                                let (mw, mh) = dims.unwrap_or((0, 0));
                                blit::read_draws(&self.handle, a, 16384).map(|(f, n, ds)| {
                                    let mut cells: std::collections::BTreeMap<(i32, i32), u32> =
                                        Default::default();
                                    let mut hud = 0u32;
                                    for d in &ds {
                                        let (gx, gy) = blit::screen_to_game(
                                            d.dx as i32,
                                            d.dy as i32,
                                            vo.x,
                                            vo.y,
                                        );
                                        if gx >= 0 && gy >= 0 && gx < mw && gy < mh {
                                            *cells.entry((gx, gy)).or_insert(0) += 1;
                                        } else {
                                            hud += 1;
                                        }
                                    }
                                    json!({
                                        "frame": f, "draws": n,
                                        "map_size": [mw, mh],
                                        "view_origin": [vo.x, vo.y],
                                        "cell_w": blit::CELL_W, "cell_h": blit::CELL_H,
                                        "map_cells": cells.len(),
                                        "hud_draws": hud,
                                        "cells": cells.iter()
                                            .map(|((x, y), c)| json!({"x": x, "y": y, "draws": c}))
                                            .collect::<Vec<_>>()
                                    })
                                })
                            })
                        }
                        (Ok(a), "blit_frame") => {
                            let lim = argi("limit", 16384).clamp(1, 16384) as usize;
                            blit::read_draws(&self.handle, a, lim).map(|(f, n, r)| {
                                json!({"frame": f, "count": n,
                                                        "returned": r.len(), "draws": r})
                            })
                        }
                        (Ok(_), other) => Err(anyhow!("unknown blit tool: {}", other)),
                    }
                } else if params.name == "player" {
                    cells::read_player(&self.handle).map(|p| json!(p))
                } else if params.name == "find_stats" {
                    let integ = argi("integrity", -1) as i32;
                    let matter = argi("matter", -1) as i32;
                    if integ < 0 || matter < 0 {
                        Err(anyhow!(
                            "find_stats: `integrity` and `matter` are required (read them \
                                     off the HUD; both carry across maps)"
                        ))
                    } else {
                        cells::find_stats(&self.handle, integ, matter).map(|v| json!({
                            "matches": v.len(),
                            "stats": v,
                            "note": "if more than one matches, move a little and re-run: energy \
                                     drifts, so a stale copy stops agreeing."
                        }))
                    }
                } else if params.name == "stats" {
                    let base = args
                        .get("base")
                        .and_then(|v| v.as_str())
                        .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .ok_or_else(|| anyhow!("stats: `base` required as \"0x...\""));
                    match base {
                        Err(e) => Err(e),
                        Ok(b) => cells::read_stats(&self.handle, b).map(|s| json!(s)),
                    }
                } else if params.name == "map_header" {
                    cells::read_header(&self.handle).map(|(w, h, c)| {
                        let a = crate::common::get_addrs(&self.handle).ok();
                        json!({
                            "build": format!("0x{:08X}", a.map_or(0, |a| a.build_stamp)),
                            "map_obj": format!("0x{:08X}", a.map_or(0, |a| a.map_object)),
                            "player_rec_known": a.is_some_and(|a| a.player_rec.is_some()),
                            "width": w, "height": h,
                            "cells_base": format!("0x{:08X}", c)
                        })
                    })
                } else if params.name == "scan_new" {
                    let v = argi("value", 0) as i32;
                    scan::scan_new(&self.handle, v).map(|c| {
                        self.scan_candidates = c;
                        json!({
                            "value": v,
                            "candidates": self.scan_candidates.len(),
                            "sample": self.scan_candidates.iter().take(12)
                                .map(|a| format!("0x{:08X}", a)).collect::<Vec<_>>(),
                            "next": "change the value in-game, then call scan_filter with the new \
                                     value (or scan_changed if you do not know it)."
                        })
                    })
                } else if params.name == "scan_filter" {
                    let v = argi("value", 0) as i32;
                    let before = self.scan_candidates.len();
                    self.scan_candidates =
                        scan::scan_filter(&self.handle, &self.scan_candidates, v);
                    Ok(json!({
                        "value": v,
                        "before": before,
                        "candidates": self.scan_candidates.len(),
                        "addresses": self.scan_candidates.iter().take(64)
                            .map(|a| format!("0x{:08X}", a)).collect::<Vec<_>>()
                    }))
                } else if params.name == "scan_changed" {
                    let old = argi("old", 0) as i32;
                    let changed = scan::scan_changed(&self.handle, &self.scan_candidates, old);
                    let keep: Vec<usize> = changed.iter().map(|(a, _)| *a).collect();
                    let before = self.scan_candidates.len();
                    self.scan_candidates = keep;
                    Ok(json!({
                        "old": old,
                        "before": before,
                        "candidates": self.scan_candidates.len(),
                        "changed": changed.iter().take(64)
                            .map(|(a, v)| json!({"addr": format!("0x{:08X}", a), "now": v}))
                            .collect::<Vec<_>>()
                    }))
                } else if params.name == "scan_ptr_to" {
                    let hex = |k: &str| {
                        args.get(k)
                            .and_then(|v| v.as_str())
                            .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                            .or_else(|| args.get(k).and_then(|v| v.as_i64()).map(|v| v as u32))
                    };
                    match (hex("lo"), hex("hi")) {
                        (Some(lo), Some(hi)) if hi >= lo => scan::scan_ptr_to(&self.handle, lo, hi)
                            .map(|hits| {
                                let statics: Vec<_> = hits
                                    .iter()
                                    .filter(|(a, _)| *a >= 0x0040_0000 && *a <= 0x0100_0000)
                                    .take(40)
                                    .map(|(a, v)| {
                                        json!({"at": format!("0x{:08X}", a),
                                                         "points_to": format!("0x{:08X}", v)})
                                    })
                                    .collect();
                                json!({
                                    "range": [format!("0x{:08X}", lo), format!("0x{:08X}", hi)],
                                    "references": hits.len(),
                                    "static_or_low_refs": statics,
                                    "sample": hits.iter().take(24)
                                        .map(|(a, v)| json!({"at": format!("0x{:08X}", a),
                                                             "points_to": format!("0x{:08X}", v)}))
                                        .collect::<Vec<_>>()
                                })
                            }),
                        _ => Err(anyhow!("scan_ptr_to: need `lo` and `hi` with hi >= lo")),
                    }
                } else if params.name == "scan_context" {
                    let vals: Vec<i32> = args
                        .get("values")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_i64())
                                .map(|x| x as i32)
                                .collect()
                        })
                        .unwrap_or_default();
                    if vals.is_empty() {
                        Err(anyhow!(
                            "scan_context: `values` must be a non-empty array of ints"
                        ))
                    } else {
                        let win = argi("window", 128).clamp(8, 4096) as usize;
                        let hits =
                            scan::scan_context(&self.handle, &self.scan_candidates, &vals, win);
                        let before = self.scan_candidates.len();
                        self.scan_candidates = hits.iter().map(|(a, _)| *a).collect();
                        Ok(json!({
                            "values": vals, "window": win,
                            "before": before, "candidates": self.scan_candidates.len(),
                            "hits": hits.iter().take(24).map(|(a, f)| json!({
                                "addr": format!("0x{:08X}", a),
                                "found": f.iter().map(|(v, off)| json!({
                                    "value": v,
                                    "rel": format!("{}{:#x}", if *off < 0 {"-"} else {"+"}, off.abs())
                                })).collect::<Vec<_>>()
                            })).collect::<Vec<_>>()
                        }))
                    }
                } else if params.name == "read_window" {
                    let addr = args
                        .get("addr")
                        .and_then(|v| v.as_str())
                        .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                        .or_else(|| {
                            args.get("addr")
                                .and_then(|v| v.as_i64())
                                .map(|v| v as usize)
                        })
                        .ok_or_else(|| anyhow!("read_window: `addr` required (int or \"0x...\")"));
                    match addr {
                        Err(e) => Err(e),
                        Ok(a) => {
                            let n = argi("words", 16).clamp(1, 256) as usize;
                            scan::read_window(&self.handle, a, n).map(|w| json!({
                                "addr": format!("0x{:08X}", a),
                                "words": w.iter().enumerate()
                                    .map(|(i, v)| json!({"off": format!("+0x{:02X}", i*4), "i32": v}))
                                    .collect::<Vec<_>>()
                            }))
                        }
                    }
                } else if params.name == "fov" {
                    cells::read_fov(&self.handle, argi("limit", 400) as usize).map(|f| json!(f))
                } else if params.name == "probe_cell" {
                    cells::probe(&self.handle, argi("x", 0) as i32, argi("y", 0) as i32)
                        .map(|p| json!(p))
                } else {
                    self.execute_tool(&params.name, params.arguments)
                        .map(|s| json!(s))
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
        let dir = |n: &str, d: &str| {
            json!({ "name": n, "description": d,
                    "inputSchema": { "type": "object", "properties": {} } })
        };
        json!([
            dir("move_north", "Move Cogmind north (numpad 8)"),
            dir("move_northeast", "Move Cogmind northeast (numpad 9)"),
            dir("move_east", "Move Cogmind east (numpad 6)"),
            dir("move_southeast", "Move Cogmind southeast (numpad 3)"),
            dir("move_south", "Move Cogmind south (numpad 2)"),
            dir("move_southwest", "Move Cogmind southwest (numpad 1)"),
            dir("move_west", "Move Cogmind west (numpad 4)"),
            dir("move_northwest", "Move Cogmind northwest (numpad 7)"),
            dir("attach", "Attach/equip from the ground (a)"),
            dir("fire", "Fire (f). Needs targeting to be useful."),
            {
                "name": "key",
                "description": "Send one keystroke: any SDLK_* keysym with optional modifiers,                                 unicode and repeat. Use the keysym values from actions.json.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "keysym":  { "type": "integer", "description": "SDLK_* value, 1..65535" },
                        "ctrl":    { "type": "boolean" },
                        "shift":   { "type": "boolean" },
                        "alt":     { "type": "boolean" },
                        "unicode": { "type": "integer", "description": "SDL_keysym.unicode; needed for text fields" },
                        "repeat":  { "type": "integer", "description": "1..255, default 1" }
                    },
                    "required": ["keysym"]
                }
            },
            {
                "name": "text",
                "description": "Type a string as keystrokes, with correct unicode. For hacking                                 codes and other text fields. Max 64 bytes.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            },
            {
                "name": "mouse_move",
                "description": "Warp the cursor to a pixel position (for cursor-driven targeting).",
                "inputSchema": {
                    "type": "object",
                    "properties": { "x": { "type": "integer" }, "y": { "type": "integer" } },
                    "required": ["x", "y"]
                }
            },
            {
                "name": "mouse_click",
                "description": "Warp the cursor and click. button 1=left 2=middle 3=right.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": { "type": "integer" }, "y": { "type": "integer" },
                        "button": { "type": "integer" }
                    },
                    "required": ["x", "y"]
                }
            },
            {
                "name": "get_game_state",
                "description": "LuigiAI state: player, inventory, cursor, hacking. Note that the                                 tile list is empty on Beta 17.1 -- the per-tile mirror is stubbed                                 out in that build. Use get_map for terrain.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "luigi_raw",
                "description": "Every LuigiAi field, raw. Use this when get_game_state errors -- it                                 shows whether the struct is live but empty (no map loaded) versus                                 genuinely broken.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "blit_status",
                "description": "Header of the SDL blit sniffer: whether recording is on, frame                                 counter, rect count, overflow. Locates the log by magic scan on                                 first use.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "blit_enable",
                "description": "Turn blit recording on or off. Off by default so it costs nothing.",
                "inputSchema": { "type": "object", "properties": { "on": {"type":"boolean"} } }
            },
            {
                "name": "blit_view",
                "description": "The map view's top-left cell in game coordinates. Needed to convert                                 screen cells to game cells, and it changes as the view pans.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "blit_screen",
                "description": "The accumulated draws converted into game coordinates: which map                                 cells the game redrew since the last blit_clear, plus a count of                                 draws that fell outside the map (HUD).",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "blit_clear",
                "description": "Start a fresh capture: resets the accumulated draw list. Beta 17.1                                 dirty-rects rather than recompositing, so the pattern is clear, act,                                 wait for the redraw, then read.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "blit_frame",
                "description": "The blit rects of the last composed frame: destination on screen,                                 source within the atlas (which identifies the glyph) or the fill                                 colour. Accumulated since the last blit_clear, so it is everything                                 drawn in that window -- i.e. everything that changed on screen.",
                "inputSchema": { "type": "object", "properties": { "limit": {"type":"integer"} } }
            },
            {
                "name": "stat_dump",
                "description": "Make Cogmind serialise the run in progress: calls its own \
                                Scorekeeper::outputScoresheet(isDump=1) on the game's main \
                                thread, the same writer Alt-Shift-S drives. Writes ~32KB of \
                                text (plus JSON with jsonStatDump=1) to the profile's dumps/ \
                                directory: part loadouts by slot and name, resource maxima, \
                                per-map stats, discovered exits, and the known map as text. \
                                Does not append to scorehistory.txt and does not advance a \
                                turn, so it is safe to call mid-run and cannot look like a \
                                completed run. Works with a menu open, unlike the keybind.",
                "inputSchema": { "type": "object", "properties": { "budget_ms": {"type":"integer"} } }
            },
            {
                "name": "dump_status",
                "description": "Whether the scoresheet writer was resolved and fingerprinted, \
                                its address and the Scorekeeper singleton, plus the outcome of \
                                the last dump. Check this first if stat_dump errors.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "player",
                "description": "Cogmind's handle, position and entity id, from the fixed-address                                 player record. Use this instead of LuigiAi.player, which stays                                 NULL on Beta 17.1.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "find_stats",
                "description": "Locate Cogmind's stat block (integrity/energy/matter/heat/corruption)                                 by scanning for matter and confirming integrity. The block is on the                                 heap so its address changes per map; give the two HUD values, both of                                 which carry across maps.",
                "inputSchema": { "type": "object",
                                 "properties": { "integrity": {"type":"integer"}, "matter": {"type":"integer"} },
                                 "required": ["integrity","matter"] }
            },
            {
                "name": "stats",
                "description": "Read the stat block at a known base (from find_stats).",
                "inputSchema": { "type": "object", "properties": { "base": {} }, "required": ["base"] }
            },
            {
                "name": "map_header",
                "description": "Map dimensions and the Cell* table base, read from the game's own                                 map object. Cheap; works whenever a map is loaded.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "get_map",
                "description": "Read Cogmind's own cell table directly (terrain, doors, and where                                 props/entities are). Complete regardless of field of view. Pass a                                 bounding box to keep the read small; a full map is tens of                                 thousands of reads.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x0": { "type": "integer" }, "y0": { "type": "integer" },
                        "x1": { "type": "integer" }, "y1": { "type": "integer" }
                    }
                }
            },
            {
                "name": "scan_new",
                "description": "Start a differential memory scan: find every writable 4-byte-aligned                                 i32 equal to `value`. Candidates are kept server-side.",
                "inputSchema": { "type": "object", "properties": { "value": { "type": "integer" } },
                                 "required": ["value"] }
            },
            {
                "name": "scan_filter",
                "description": "Narrow the stored candidates to those now equal to `value`. Run after                                 the value changed in-game.",
                "inputSchema": { "type": "object", "properties": { "value": { "type": "integer" } },
                                 "required": ["value"] }
            },
            {
                "name": "scan_changed",
                "description": "Narrow the stored candidates to those that no longer hold `old`, and                                 report what they hold now. For when the new value is unknown.",
                "inputSchema": { "type": "object", "properties": { "old": { "type": "integer" } },
                                 "required": ["old"] }
            },
            {
                "name": "scan_ptr_to",
                "description": "Find 4-byte-aligned words holding a value inside [lo, hi]. For                                 pointer chaining: an exact pointer scan usually finds nothing                                 because pointers target an object's start, not the field you care                                 about. Highlights references that live at low/static addresses.",
                "inputSchema": { "type": "object", "properties": { "lo": {}, "hi": {} },
                                 "required": ["lo", "hi"] }
            },
            {
                "name": "scan_context",
                "description": "Narrow the stored candidates to those with ALL of `values` present                                 as i32s within `window` bytes. For narrowing on values you cannot                                 change on demand, by requiring related fields to co-occur.",
                "inputSchema": { "type": "object",
                                 "properties": { "values": { "type": "array", "items": {"type":"integer"} },
                                                 "window": { "type": "integer" } },
                                 "required": ["values"] }
            },
            {
                "name": "read_window",
                "description": "Read consecutive i32s at an address, to inspect a candidate's                                 neighbouring fields.",
                "inputSchema": { "type": "object",
                                 "properties": { "addr": {}, "words": { "type": "integer" } },
                                 "required": ["addr"] }
            },
            {
                "name": "fov",
                "description": "Read the field-of-view containers. Visibility is not stored per                                 cell; isVisible() queries a coordinate vector on a global object.                                 Returns the visible coordinate list plus the two adjacent                                 containers. `limit` caps how many coords come back.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "limit": { "type": "integer" } }
                }
            },
            {
                "name": "probe_cell",
                "description": "Dump one cell's raw bytes plus the decoded fields. Calibration                                 tool: the decoded x/y must match the requested x/y, otherwise the                                 Cell offsets are wrong for this build.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "x": { "type": "integer" }, "y": { "type": "integer" } },
                    "required": ["x", "y"]
                }
            }
        ])
    }

    fn get_game_state(&mut self) -> Result<SerializableGameState> {
        let luigi_ai = get_luigi_ai(&self.handle)?;

        // LuigiAi::initialize() leaves player/mapData NULL until a map loads, so
        // these must be checked rather than dereferenced. The previous code read
        // through them unconditionally and failed with a bare errno.
        if luigi_ai.player == 0 {
            return Err(anyhow!(
                "LuigiAi.player is NULL (actionReady={}, map={}x{}, depth={}, mapData=0x{:X}). \
                 The struct exists but no map is loaded yet -- get to an actual map, then retry. \
                 Use luigi_raw to see every field.",
                luigi_ai.action_ready,
                luigi_ai.map_width,
                luigi_ai.map_height,
                luigi_ai.location_depth,
                luigi_ai.map_data
            ));
        }
        let player: LuigiEntity = self.read_memory_struct(luigi_ai.player as usize)?;
        let machine_hacking: Option<LuigiMachineHacking> = if luigi_ai.machine_hacking == 0 {
            None
        } else {
            Some(self.read_memory_struct(luigi_ai.machine_hacking as usize)?)
        };

        if luigi_ai.map_data == 0 || luigi_ai.map_width <= 0 || luigi_ai.map_height <= 0 {
            return Err(anyhow!(
                "LuigiAi.mapData is not allocated ({}x{}, mapData=0x{:X}). Note that on Beta 17.1 \
                 the per-tile mirror is a stub even once this is allocated -- use get_map.",
                luigi_ai.map_width,
                luigi_ai.map_height,
                luigi_ai.map_data
            ));
        }
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

        // luigiai.h: "access = x*mapHeight+y" -- mapData is COLUMN-major.
        // The previous walk indexed y*map_width+x and then swapped the emitted
        // coordinates, which only cancels out on square maps.
        let mut serializable_map = Vec::new();
        let mut player_x: i32 = -1;
        let mut player_y: i32 = -1;

        for x in 0..luigi_ai.map_width {
            let mut column = Vec::new();
            for y in 0..luigi_ai.map_height {
                let index = (x * luigi_ai.map_height + y) as usize;
                let tile = &tiles[index];

                if tile.entity != 0 && tile.entity == luigi_ai.player {
                    player_x = x;
                    player_y = y;
                }

                let prop = if tile.prop != 0 {
                    let p = self.read_memory_struct::<LuigiProp>(tile.prop as usize)?;
                    Some(SerializableProp {
                        name: PropId::from_id(p.id).map(|v| v.name()),
                        raw_id: p.id,
                        interactive_piece: p.interactive_piece,
                    })
                } else {
                    None
                };
                let entity = if tile.entity != 0 {
                    let e = self.read_memory_struct::<LuigiEntity>(tile.entity as usize)?;
                    Some(SerializableEntity::from(&e))
                } else {
                    None
                };
                let item = if tile.item != 0 {
                    let it = self.read_memory_struct::<LuigiItem>(tile.item as usize)?;
                    Some(SerializableItem {
                        name: ItemId::from_id(it.id).map(|v| v.name()),
                        raw_id: it.id,
                        integrity: it.integrity,
                        equipped: None,
                    })
                } else {
                    None
                };

                // NO_CELL is -1: unknown / never seen.
                if tile.cell >= 0 {
                    column.push(SerializableTile {
                        last_action: tile.last_action,
                        last_fov: tile.last_fov,
                        cell: CellId::from_id(tile.cell).map(|v| v.name()),
                        raw_cell: tile.cell,
                        door_open: tile.door_open,
                        prop,
                        entity,
                        item,
                        x,
                        y,
                    });
                }
            }
            if !column.is_empty() {
                serializable_map.push(column);
            }
        }

        let item_stride = Self::item_stride();
        let inventory = self.read_inventory(&player, item_stride)?;

        Ok(SerializableGameState {
            action_ready: luigi_ai.action_ready,
            map_width: luigi_ai.map_width,
            map_height: luigi_ai.map_height,
            location: get_presence(
                luigi_ai.location_depth,
                MapType::try_from(luigi_ai.location_map).unwrap_or(MapType::MapNone),
            ),
            map_cursor_index: luigi_ai.map_cursor_index,
            player_x,
            player_y,
            player: SerializableEntity::from(&player),
            inventory,
            item_stride,
            machine_hacking,
            map: serializable_map,
        })
    }

    /// `luigiai.h` declares `LuigiItem { itemID; int integrity; bool equipped; }`,
    /// which is 12 bytes with padding. StatMind's AGENT.md records `equipped`
    /// being removed to fix a Beta 16 misalignment, so the two disagree and the
    /// stride has not been confirmed against Beta 17.1. Single items read through
    /// a pointer are fine either way; only array walks care. Override with
    /// STATMIND_ITEM_STRIDE=8 to test the other reading without a rebuild.
    fn item_stride() -> usize {
        match std::env::var("STATMIND_ITEM_STRIDE") {
            Ok(v) => v.trim().parse::<usize>().unwrap_or(12).clamp(8, 16),
            Err(_) => 12,
        }
    }

    /// Walk `LuigiEntity.inventory` -- the only route to Cogmind's own build.
    fn read_inventory(&self, player: &LuigiEntity, stride: usize) -> Result<Vec<SerializableItem>> {
        let mut out = Vec::new();
        if player.inventory == 0 || player.inventory_size <= 0 {
            return Ok(out);
        }
        // Guard against a garbage size dragging in the whole address space.
        let count = player.inventory_size.min(512) as usize;
        let bytes = copy_address(player.inventory as usize, count * stride, &self.handle)?;

        for i in 0..count {
            let base = i * stride;
            let raw_id = i32::from_le_bytes(bytes[base..base + 4].try_into()?);
            let integrity = i32::from_le_bytes(bytes[base + 4..base + 8].try_into()?);
            let equipped = if stride >= 12 {
                Some(bytes[base + 8] != 0)
            } else {
                None
            };
            out.push(SerializableItem {
                name: ItemId::from_id(raw_id).map(|v| v.name()),
                raw_id,
                integrity,
                equipped,
            });
        }
        Ok(out)
    }

    fn read_memory_struct<T: Sized>(&self, address: usize) -> Result<T> {
        let bytes = copy_address(address, mem::size_of::<T>(), &self.handle)?;
        let s: T = unsafe { std::ptr::read(bytes.as_ptr() as *const _) };
        Ok(s)
    }

    fn execute_tool(&mut self, name: &str, args: Option<Value>) -> Result<String> {
        let a = args.unwrap_or(Value::Null);
        let b = |k: &str| a.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let i = |k: &str, d: i64| a.get(k).and_then(|v| v.as_i64()).unwrap_or(d);

        let mut mods = 0u16;
        if b("ctrl") {
            mods |= KMOD_LCTRL;
        }
        if b("shift") {
            mods |= KMOD_LSHIFT;
        }
        if b("alt") {
            mods |= KMOD_LALT;
        }

        // Eight-way movement keeps its own verbs; the numpad bindings are
        // unmodified in every stock layout.
        let numpad = match name {
            "move_north" => Some(8),
            "move_northeast" => Some(9),
            "move_east" => Some(6),
            "move_southeast" => Some(3),
            "move_south" => Some(2),
            "move_southwest" => Some(1),
            "move_west" => Some(4),
            "move_northwest" => Some(7),
            _ => None,
        };
        if let Some(n) = numpad {
            return self.submit(MailboxCmd::Key {
                keysym: SDLK_KP0 + n,
                mods: 0,
                unicode: 0,
                repeat: 1,
            });
        }

        match name {
            "key" => {
                let keysym = i("keysym", 0);
                if !(1..=65535).contains(&keysym) {
                    return Err(anyhow!("key: `keysym` must be 1..65535 (an SDLK_* value)"));
                }
                let unicode = i("unicode", 0).clamp(0, 65535) as u16;
                let repeat = i("repeat", 1).clamp(1, 255) as u16;
                self.submit(MailboxCmd::Key {
                    keysym: keysym as u16,
                    mods,
                    unicode,
                    repeat,
                })
            }
            "text" => {
                let t = a
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("text: `text` is required"))?;
                if t.len() > MB_TEXT_MAX {
                    return Err(anyhow!(
                        "text: {} bytes exceeds the {}-byte mailbox buffer",
                        t.len(),
                        MB_TEXT_MAX
                    ));
                }
                self.submit(MailboxCmd::Text(t.to_string()))
            }
            "mouse_move" => self.submit(MailboxCmd::MouseMove {
                x: i("x", 0) as i32,
                y: i("y", 0) as i32,
            }),
            "mouse_click" => self.submit(MailboxCmd::MouseClick {
                x: i("x", 0) as i32,
                y: i("y", 0) as i32,
                button: i("button", 1).clamp(1, 5) as u8,
            }),
            // Retained for compatibility with the v1 tool names.
            "attach" => self.submit(MailboxCmd::Key {
                keysym: b'a' as u16,
                mods: 0,
                unicode: b'a' as u16,
                repeat: 1,
            }),
            "fire" => self.submit(MailboxCmd::Key {
                keysym: b'f' as u16,
                mods: 0,
                unicode: b'f' as u16,
                repeat: 1,
            }),
            _ => Err(anyhow!("Unknown tool: {}", name)),
        }
    }

    /// Write a command into the mailbox and wait for the shim to acknowledge it.
    ///
    /// Two separate signals, which the v1 protocol conflated:
    ///   * `ack == seq`      -- the keystroke was delivered to SDL.
    ///   * `actionReady`     -- a game turn actually advanced.
    /// A key that only opens a UI panel satisfies the first and not the second,
    /// which is why the old code timed out on every menu interaction.
    fn submit(&mut self, cmd: MailboxCmd) -> Result<String> {
        let addr = self.mailbox()?;
        let version = u32::from_le_bytes(
            copy_address(addr + MB_VERSION, 4, &self.handle)?
                .try_into()
                .map_err(|_| anyhow!("short read of mailbox version"))?,
        );
        if version != MB_EXPECT_VERSION {
            return Err(anyhow!(
                "mailbox version {} but this build expects {} -- rebuild SDL.dll from \
                 SDL-1.2/src/statmind_ipc.h",
                version,
                MB_EXPECT_VERSION
            ));
        }

        set_memory_writable(&self.handle, addr, MB_SIZE)?;

        let (is_dump, dump_budget) = match &cmd {
            MailboxCmd::Dump { budget_ms } => (true, *budget_ms),
            _ => (false, 0),
        };

        let seq = u32::from_le_bytes(
            copy_address(addr + MB_SEQ, 4, &self.handle)?
                .try_into()
                .map_err(|_| anyhow!("short read of mailbox seq"))?,
        );
        let action_before = get_luigi_ai(&self.handle)
            .map(|v| v.action_ready)
            .unwrap_or(-1);

        // Zero the variable payload so a previous command cannot leak through.
        write_memory(
            &self.handle,
            addr + MB_COMMAND,
            &[0u8; MB_SIZE - MB_COMMAND],
        )?;

        let label;
        match cmd {
            MailboxCmd::Key {
                keysym,
                mods,
                unicode,
                repeat,
            } => {
                label = format!(
                    "key sym={} mods=0x{:04x} uni={} x{}",
                    keysym, mods, unicode, repeat
                );
                write_memory(&self.handle, addr + MB_KEYSYM, &keysym.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_MODIFIERS, &mods.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_UNICODE, &unicode.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_REPEAT, &repeat.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_COMMAND, &[b'K'])?;
            }
            MailboxCmd::Text(t) => {
                label = format!("text {:?}", t);
                let bytes = t.as_bytes();
                let mut buf = [0u8; MB_TEXT_MAX];
                buf[..bytes.len()].copy_from_slice(bytes);
                write_memory(&self.handle, addr + MB_TEXT, &buf)?;
                write_memory(
                    &self.handle,
                    addr + MB_TEXT_LEN,
                    &(bytes.len() as u16).to_le_bytes(),
                )?;
                write_memory(&self.handle, addr + MB_COMMAND, &[b'T'])?;
            }
            MailboxCmd::MouseMove { x, y } => {
                label = format!("mouse_move ({},{})", x, y);
                write_memory(&self.handle, addr + MB_MOUSE_X, &x.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_MOUSE_Y, &y.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_COMMAND, &[b'M'])?;
            }
            MailboxCmd::MouseClick { x, y, button } => {
                label = format!("mouse_click ({},{}) button={}", x, y, button);
                write_memory(&self.handle, addr + MB_MOUSE_X, &x.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_MOUSE_Y, &y.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_BUTTON, &[button])?;
                write_memory(&self.handle, addr + MB_COMMAND, &[b'B'])?;
            }
            MailboxCmd::Dump { budget_ms } => {
                label = format!("stat_dump (budget {}ms)", budget_ms);
                write_memory(&self.handle, addr + MB_REPEAT, &budget_ms.to_le_bytes())?;
                write_memory(&self.handle, addr + MB_COMMAND, &[b'D'])?;
            }
        }

        // Publish LAST: the shim treats seq != ack as "a command is ready".
        let next = seq.wrapping_add(1);
        write_memory(&self.handle, addr + MB_SEQ, &next.to_le_bytes())?;

        // Phase 1: delivery.
        let t0 = std::time::Instant::now();
        // A dump acks only after the writer has run and the file is on disk, so
        // it needs headroom over the shim's own wait budget rather than the 2s
        // that suffices for a keystroke.
        let deliver_timeout = if is_dump {
            Duration::from_millis(dump_budget as u64 + 3000)
        } else {
            Duration::from_secs(2)
        };
        let mut delivered = false;
        while t0.elapsed() < deliver_timeout {
            let ack = u32::from_le_bytes(
                copy_address(addr + MB_ACK, 4, &self.handle)?
                    .try_into()
                    .map_err(|_| anyhow!("short read of mailbox ack"))?,
            );
            if ack == next {
                delivered = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        if !delivered {
            return Err(anyhow!(
                "{}: shim never acked (seq={}) -- is the patched SDL.dll loaded?",
                label,
                next
            ));
        }
        let deliver_ms = t0.elapsed().as_millis();
        let status = i32::from_le_bytes(
            copy_address(addr + MB_STATUS, 4, &self.handle)?
                .try_into()
                .map_err(|_| anyhow!("short read of mailbox status"))?,
        );
        if status < 0 {
            return Err(anyhow!(
                "{}: shim rejected the command (status {})",
                label,
                status
            ));
        }

        // A dump advances no turn and is complete once acked; waiting on
        // actionReady would just add 750ms of nothing.
        if is_dump {
            return Ok(format!("{} completed in {}ms", label, deliver_ms));
        }

        // Phase 2: did a turn advance? Not an error if it did not -- many keys
        // legitimately only change UI state.
        let t1 = std::time::Instant::now();
        let mut action_after = action_before;
        while t1.elapsed() < Duration::from_millis(750) {
            if let Ok(ai) = get_luigi_ai(&self.handle) {
                if ai.action_ready != action_before {
                    action_after = ai.action_ready;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }

        Ok(if action_after != action_before {
            format!(
                "{} delivered in {}ms; turn advanced {} -> {}",
                label, deliver_ms, action_before, action_after
            )
        } else {
            format!(
                "{} delivered in {}ms; no turn advanced (actionReady still {}) -- \
                 UI-only key, or the game is waiting on something",
                label, deliver_ms, action_before
            )
        })
    }

    fn mailbox(&mut self) -> Result<usize> {
        if self.mailbox_address.is_none() {
            self.mailbox_address = Some(get_mailbox_address(&self.handle)?);
            eprintln!("Mailbox found at 0x{:X}", self.mailbox_address.unwrap());
        }
        Ok(self.mailbox_address.unwrap())
    }

    pub fn initialize_mailbox_address(&mut self) -> Result<()> {
        eprintln!("Scanning for mailbox...");
        let addr = self.mailbox()?;
        let version = u32::from_le_bytes(
            copy_address(addr + MB_VERSION, 4, &self.handle)?
                .try_into()
                .map_err(|_| anyhow!("short read of mailbox version"))?,
        );
        eprintln!("Mailbox at 0x{:X}, protocol v{}", addr, version);
        if version != MB_EXPECT_VERSION {
            eprintln!(
                "WARNING: mailbox is v{} but statmind expects v{}. Rebuild SDL.dll.",
                version, MB_EXPECT_VERSION
            );
        }
        Ok(())
    }
}
