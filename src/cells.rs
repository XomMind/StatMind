//! Direct reads of Cogmind's own cell table.
//!
//! `updateLuigiAiMapTile` is an empty function in Beta 17.1, so `LuigiAi.mapData`
//! is allocated and never filled. Map content therefore has to come from the
//! game's own structures instead of from the LuigiAI mirror.
//!
//! This is not a workaround so much as the better path. The table below is live
//! and complete regardless of field of view, so the same read serves two views:
//! filtered by visibility for the agent, unfiltered for a privileged critic.
//! Crucially it requires no patching -- and in particular no widening of FOV,
//! which would change enemy activation and so alter the environment's dynamics
//! rather than just its observability.
//!
//! All addresses are Beta 17.1 absolute VAs. The image has no `.reloc` section
//! and no `DYNAMIC_BASE`, so it always loads at `0x00400000` and these are
//! literal at runtime.
//!
//! # Provenance
//!
//! Map object, from `cellAt` @ `0x009CF7D0` (thiscall, `retl $8`):
//! ```text
//! Cell** cellAt(this, int x, int y) {
//!     int idx = x * this->[0x04] + y;    // height
//!     return (Cell**)this->[0x08] + idx; // cells
//! }
//! ```
//! `x * height + y` matches `luigiai.h`'s `access = x*mapHeight+y`, and matches
//! the cursor-index computation at `0x00774D0C` independently.
//!
//! Cell offsets come from byte-matching the unoptimised MSVC accessor bodies in
//! both Beta 14 and Beta 17.1; every offset below is identical across the two.
//! Call counts are b17.1 xref counts, and they are what make the identification
//! credible for the two handles.
//!
//! | Accessor | VA (b17.1) | Field | Callers |
//! |---|---|---|---|
//! | `getCoord`      | `0x0045D1A0` | `(Coord*)(this+0x30)`  | 188 |
//! | `getCellId`     | `0x0045D0D0` | `**(int**)this`        | 45 |
//! | `typeClass`     | `0x0045DC50` | `this->[0]->[0x5C]>=2` | 33 |
//! | `isDoorOpen`    | `0x0045DBF0` | `this->[0x3A] != 0`    | 8 |
//! | `getProp`       | `0x0045D550` | `this->[0x44]`         | 1357 |
//! | `getEntity`     | `0x00463250` | `this->[0x48]`         | 1312 |
//! | `itemContainer` | `0x0045D8F0` | `this+0x4C`            | 547 |

use crate::generated::{CellId, EntityId};
use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};
use serde::Serialize;
use std::collections::HashMap;

/// Field offsets in the map object `{ int width; int height; Cell** cells; }`.
/// Its address is per build and comes from `common::get_addrs`.
pub const MAP_WIDTH: usize = 0x00;
pub const MAP_HEIGHT: usize = 0x04;
pub const MAP_CELLS: usize = 0x08;

// Cell member offsets.
//
// +0x00/+0x30/+0x34 were derived statically and are confirmed: probing a live
// Beta 17.1 returns decoded x/y equal to the requested coordinates for every
// cell, and all 10,000 slots of a 100x100 map resolve to distinct in-range
// coordinates. The rest were read off raw dumps of known cell types and are
// labelled by the evidence in `docs/cell-layout` -- see the notes file.
pub const CELL_TYPE: usize = 0x00; // CellType*
/// ASCII glyph. Observed: FLOOR_YRD 0x2E '.', EARTH 0x23 '#', STAIRS_YRD 0x3C '<'.
/// The game's own render character, so no cellID-to-glyph table is needed.
pub const CELL_GLYPH: usize = 0x04;
/// Small per-type integer, distinct per cell type (483/485/489 observed).
/// Presumed a colour or palette index.
pub const CELL_COLOR: usize = 0x08;
/// Small per-cell integer, NOT per-type: -1 on EARTH, and 1..16 scattered across
/// FLOOR_YRD cells. An earlier reading called this an exit/link id because the
/// first cells sampled happened to be -1 except a stairs cell; a wider read
/// disproved that. Most likely a tile variant or debris index. Unidentified.
pub const CELL_UNK_0C: usize = 0x0C;
pub const CELL_X: usize = 0x30;
pub const CELL_Y: usize = 0x34;
/// 1 on FLOOR/STAIRS, 0 on EARTH/EARTH_EXC. Presumed passability.
pub const CELL_PASSABLE: usize = 0x38; // u8
/// Also 1 on FLOOR/STAIRS and 0 on EARTH. Presumed transparency.
/// NOTE: this is the byte the b14 `isDoorOpen` accessor read. Carrying that
/// name into 17.1 was wrong -- it is set on 100% of walkable cells and 0% of
/// solid ones, which is not door state.
pub const CELL_TRANSPARENT: usize = 0x3A; // u8
/// Prop handle, 0 = absent. NOT a pointer: observed 0x00440000, i.e. a small
/// tag in the upper half-word, unlike the 0x0Cxxxxxx heap pointers elsewhere.
pub const CELL_PROP: usize = 0x44;
/// Entity handle, 0 = absent. Same shape (observed 0x00420000).
pub const CELL_ENTITY: usize = 0x48;
pub const CELL_ITEM_CONTAINER: usize = 0x4C;
/// FOV-state candidates, found by diffing a visible cell against a distant one
/// of the same type. The Cell is much larger than 0x50; these all differ.
pub const CELL_UNK_2C: usize = 0x2C; // visible -1, distant 0
pub const CELL_UNK_40: usize = 0x40; // visible -9999, distant 2 -- turn-stamp shaped
pub const CELL_UNK_98: usize = 0x98; // non-zero on visible, zero on distant
pub const CELL_UNK_A0: usize = 0xA0;
/// Enough to cover every offset above.
pub const CELL_READ_LEN: usize = 0xC0;

// CellType member offsets.
pub const CELLTYPE_ID: usize = 0x00; // int cellID
pub const CELLTYPE_CLASS: usize = 0x5C; // int; >= 2 means appearance is state-dependent

/// Refuse absurd dimensions rather than trying to allocate for them: a wrong
/// address or a mid-load read would otherwise ask for gigabytes.
const MAX_DIM: i32 = 1024;

#[derive(Serialize, Debug, Clone)]
pub struct DirectCell {
    pub x: i32,
    pub y: i32,
    /// The game's own render character.
    pub glyph: char,
    pub color: i32,
    /// Unidentified small per-cell value; see CELL_UNK_0C.
    pub unk_0c: i32,
    pub passable: bool,
    pub transparent: bool,
    pub unk_2c: i32,
    pub unk_40: i32,
    pub unk_98: i32,
    pub unk_a0: i32,
    /// Resolved `cellID`, or `None` if the type pointer was unreadable.
    pub cell_id: Option<i32>,
    /// `cellID` looked up in the generated table. Beware: the tables in this repo
    /// are Beta 16, so a `None` here with a valid `cell_id` most likely means the
    /// tables need regenerating for the build in use, not that the read failed.
    pub cell_name: Option<&'static str>,
    /// `CellType+0x5C`. `>= 2` marks types whose look depends on state (doors).
    pub type_class: Option<i32>,
    /// Non-zero when a prop occupies this cell. A handle, not a pointer.
    pub prop: u32,
    /// Non-zero when an entity occupies this cell. A handle, not a pointer.
    pub entity: u32,
    /// Raw first word of the item container; interpretation still unconfirmed.
    pub item_container: u32,
}

#[derive(Serialize, Debug)]
pub struct DirectMap {
    pub width: i32,
    pub height: i32,
    pub cells_base: u32,
    /// Cells actually read, i.e. those with a non-null `Cell*`.
    pub cell_count: usize,
    /// Distinct `CellType*` values seen; the resolved ids are cached per type,
    /// which is what keeps this to roughly one extra read per type rather than
    /// per cell.
    pub type_count: usize,
    pub cells: Vec<DirectCell>,
}

fn rd_i32(handle: &ProcessHandle, addr: usize) -> Result<i32> {
    let b = copy_address(addr, 4, handle)?;
    Ok(i32::from_le_bytes(
        b.try_into()
            .map_err(|_| anyhow!("short read at 0x{:X}", addr))?,
    ))
}

fn rd_u32(handle: &ProcessHandle, addr: usize) -> Result<u32> {
    Ok(rd_i32(handle, addr)? as u32)
}

/// Read the map dimensions and the base of the `Cell*` table.
pub fn read_header(handle: &ProcessHandle) -> Result<(i32, i32, u32)> {
    let map_obj = crate::common::get_addrs(handle)?.map_object;
    let width = rd_i32(handle, map_obj + MAP_WIDTH)?;
    let height = rd_i32(handle, map_obj + MAP_HEIGHT)?;
    let cells = rd_u32(handle, map_obj + MAP_CELLS)?;

    if width <= 0 || height <= 0 || width > MAX_DIM || height > MAX_DIM {
        return Err(anyhow!(
            "implausible map dimensions {}x{} at 0x{:X} -- not in a map yet, or the \
             address is wrong for this build",
            width,
            height,
            map_obj
        ));
    }
    if cells == 0 {
        return Err(anyhow!(
            "cell table pointer at 0x{:X} is null",
            map_obj + MAP_CELLS
        ));
    }
    Ok((width, height, cells))
}

/// Walk the cell table, optionally restricted to an inclusive bounding box.
///
/// Cells are individually heap-allocated, so this is one read per occupied cell
/// and cannot be batched. A full map is tens of thousands of reads, which is
/// tolerable interactively and far too slow for training -- the fix there is to
/// move the reader in-process, not to micro-optimise the mach round trips.
pub fn read_map(handle: &ProcessHandle, bbox: Option<(i32, i32, i32, i32)>) -> Result<DirectMap> {
    let (width, height, cells_base) = read_header(handle)?;

    let (x0, y0, x1, y1) = match bbox {
        Some((a, b, c, d)) => (
            a.clamp(0, width - 1),
            b.clamp(0, height - 1),
            c.clamp(0, width - 1),
            d.clamp(0, height - 1),
        ),
        None => (0, 0, width - 1, height - 1),
    };
    if x0 > x1 || y0 > y1 {
        return Err(anyhow!(
            "empty bounding box ({},{})..({},{})",
            x0,
            y0,
            x1,
            y1
        ));
    }

    // One bulk read of the pointer table for the columns we care about.
    let col_bytes = (height as usize) * 4;
    let mut type_cache: HashMap<u32, (Option<i32>, Option<i32>)> = HashMap::new();
    let mut out = Vec::new();

    for x in x0..=x1 {
        let col_addr = cells_base as usize + (x as usize) * col_bytes;
        let raw = copy_address(col_addr, col_bytes, handle)?;

        for y in y0..=y1 {
            let i = (y as usize) * 4;
            let cell_ptr = u32::from_le_bytes(
                raw[i..i + 4]
                    .try_into()
                    .map_err(|_| anyhow!("short read in cell column {}", x))?,
            );
            if cell_ptr == 0 {
                continue;
            }

            let cb = match copy_address(cell_ptr as usize, CELL_READ_LEN, handle) {
                Ok(b) => b,
                Err(_) => continue, // torn or unmapped: skip rather than abort the sweep
            };
            let word = |off: usize| -> u32 {
                u32::from_le_bytes([cb[off], cb[off + 1], cb[off + 2], cb[off + 3]])
            };

            let type_ptr = word(CELL_TYPE);
            let (cell_id, type_class) = if type_ptr == 0 {
                (None, None)
            } else {
                *type_cache.entry(type_ptr).or_insert_with(|| {
                    let id = rd_i32(handle, type_ptr as usize + CELLTYPE_ID).ok();
                    let cls = rd_i32(handle, type_ptr as usize + CELLTYPE_CLASS).ok();
                    (id, cls)
                })
            };

            out.push(DirectCell {
                // Prefer the cell's own coordinates over the loop indices: if they
                // disagree, the layout assumption is wrong and we want to see it.
                x: word(CELL_X) as i32,
                y: word(CELL_Y) as i32,
                glyph: char::from_u32(word(CELL_GLYPH) & 0x7F).unwrap_or('?'),
                color: word(CELL_COLOR) as i32,
                unk_0c: word(CELL_UNK_0C) as i32,
                passable: cb[CELL_PASSABLE] != 0,
                transparent: cb[CELL_TRANSPARENT] != 0,
                unk_2c: word(CELL_UNK_2C) as i32,
                unk_40: word(CELL_UNK_40) as i32,
                unk_98: word(CELL_UNK_98) as i32,
                unk_a0: word(CELL_UNK_A0) as i32,
                cell_id,
                cell_name: cell_id.and_then(CellId::from_id).map(|c| c.name()),
                type_class,
                prop: word(CELL_PROP),
                entity: word(CELL_ENTITY),
                item_container: word(CELL_ITEM_CONTAINER),
            });
        }
    }

    Ok(DirectMap {
        width,
        height,
        cells_base,
        cell_count: out.len(),
        type_count: type_cache.len(),
        cells: out,
    })
}

/// The player record: `{ u32 handle; i32 x; i32 y; i32 entity_id }` at a fixed
/// address in `.data`.
///
/// Found by differential scan, not statically: scan for the player's x, move,
/// rescan, three times. 2119 candidates narrowed to 3, of which this is the only
/// static one. Verified to track movement -- (45,52) -> north -> (45,51) ->
/// west -> (44,51) -- and to agree with the entity handle in the cell table.
///
/// This is what `LuigiAi.player` should have pointed at, and is the replacement
/// for it on Beta 17.1, where that field stays NULL forever.
/// Field offsets in the player record. Its address is per build, comes from
/// `common::get_addrs`, and is absent on a build where it has never been read.
pub const PLAYER_HANDLE: usize = 0x00; // observed 0x00420000
pub const PLAYER_X: usize = 0x04;
pub const PLAYER_Y: usize = 0x08;
pub const PLAYER_ENTITY_ID: usize = 0x0C; // 322 == EntityId "Player"

#[derive(Serialize, Debug)]
pub struct PlayerRec {
    pub addr: String,
    pub handle: u32,
    pub x: i32,
    pub y: i32,
    pub entity_id: i32,
    pub entity_name: Option<&'static str>,
    pub plausible: bool,
}

/// Read the player record. `plausible` is a sanity gate: the handle must be
/// non-zero and the coordinates inside the current map.
pub fn read_player(handle: &ProcessHandle) -> Result<PlayerRec> {
    let rec = crate::common::get_addrs(handle)?.player_rec.ok_or_else(|| {
        anyhow!(
            "the player record has never been located on this build; it is \
             zero-fill with no reference to pin it, so it has to be found by \
             differential scan and added to statmind_build.h"
        )
    })?;
    let h = rd_u32(handle, rec + PLAYER_HANDLE)?;
    let x = rd_i32(handle, rec + PLAYER_X)?;
    let y = rd_i32(handle, rec + PLAYER_Y)?;
    let id = rd_i32(handle, rec + PLAYER_ENTITY_ID)?;
    let dims = read_header(handle).ok();
    let plausible = h != 0
        && dims
            .map(|(w, ht, _)| x >= 0 && y >= 0 && x < w && y < ht)
            .unwrap_or(false);
    Ok(PlayerRec {
        addr: format!("0x{:08X}", rec),
        handle: h,
        x,
        y,
        entity_id: id,
        entity_name: EntityId::from_id(id).map(|e| e.name()),
        plausible,
    })
}

/// Cogmind's own stat block: `{ integrity, energy, matter, heat, corruption }`
/// as five consecutive i32s, with `speed` at -0x44.
///
/// Found by differential scan: `scan_new(228)` for matter, then requiring the
/// current core (250) and energy (100) to co-occur within 128 bytes, which cut
/// 462 candidates to 17; of those, exactly one had an entity-shaped
/// neighbourhood. Verified live: energy read 99 -> 100 -> 99 across injected
/// moves, i.e. regenerating and being spent on movement.
///
/// **This address is on the heap**, so unlike the player record it is not a
/// constant. A pointer scan over the enclosing object found only stack
/// (`0x0012xxxx`) and heap referrers, no static path, so chaining to a stable
/// base would take several more rounds and stay fragile. `find_stats` re-locates
/// it instead, which works because integrity and matter carry across maps.
///
/// The durable fix is the in-process reader: from inside the process these
/// become calls to the game's own accessors at fixed VAs, and no offset or
/// pointer chain has to be maintained at all.
pub const STATS_INTEGRITY: usize = 0x00; // relative to the record base
pub const STATS_ENERGY: usize = 0x04;
pub const STATS_MATTER: usize = 0x08;
pub const STATS_HEAT: usize = 0x0C;
pub const STATS_CORRUPTION: usize = 0x10;
/// Speed sits well before the block; observed 50 at base-0x44 with FASTx2.
pub const STATS_SPEED_REL: isize = -0x44;

#[derive(Serialize, Debug)]
pub struct Stats {
    pub base: String,
    pub integrity: i32,
    pub energy: i32,
    pub matter: i32,
    pub heat: i32,
    pub corruption: i32,
    pub speed: i32,
}

pub fn read_stats(handle: &ProcessHandle, base: usize) -> Result<Stats> {
    Ok(Stats {
        base: format!("0x{:08X}", base),
        integrity: rd_i32(handle, base + STATS_INTEGRITY)?,
        energy: rd_i32(handle, base + STATS_ENERGY)?,
        matter: rd_i32(handle, base + STATS_MATTER)?,
        heat: rd_i32(handle, base + STATS_HEAT)?,
        corruption: rd_i32(handle, base + STATS_CORRUPTION)?,
        speed: rd_i32(handle, (base as isize + STATS_SPEED_REL) as usize).unwrap_or(-1),
    })
}

/// Re-locate the stat block from two values that are known to persist across map
/// transitions: core integrity and matter. Returns every base whose decoded
/// record is self-consistent.
pub fn find_stats(handle: &ProcessHandle, integrity: i32, matter: i32) -> Result<Vec<Stats>> {
    let cands = crate::scan::scan_new(handle, matter)?;
    let mut out = Vec::new();
    for a in cands {
        // matter sits at base+0x08, so the base is 8 bytes back.
        if a < STATS_MATTER {
            continue;
        }
        let base = a - STATS_MATTER;
        let Ok(st) = read_stats(handle, base) else {
            continue;
        };
        // Integrity must match, and heat/corruption must be sane rather than
        // arbitrary bytes that happen to sit next to the right number.
        let sane = st.integrity == integrity
            && st.energy >= 0
            && st.energy <= 100_000
            && st.heat >= 0
            && st.heat <= 100_000
            && st.corruption >= 0
            && st.corruption <= 100;
        if sane {
            out.push(st);
        }
    }
    Ok(out)
}

/// The FOV object pointer, and the container of currently-visible coordinates.
///
/// Visibility is not a Cell field. `isVisible(Coord*)` (`0x00464010`) queries a
/// container at `fovObj + 0x7D0`, and Beta 14's equivalent lookup is a linear
/// scan with `size()`/`at()` -- i.e. a vector, so the elements are a contiguous
/// array of 8-byte Coords. MSVC lays a vector out as three pointers.
pub const FOV_OBJ_PTR: usize = 0x00CE_FC4C;
pub const FOV_VISIBLE_OFF: usize = 0x7D0;
pub const FOV_SIBLING_A_OFF: usize = 0x7C4;
pub const FOV_SIBLING_B_OFF: usize = 0x7E0;

#[derive(Serialize, Debug)]
pub struct CoordVec {
    pub member_offset: String,
    pub first: u32,
    pub last: u32,
    pub end: u32,
    pub len: usize,
    pub capacity: usize,
    pub plausible: bool,
    pub coords: Vec<(i32, i32)>,
}

fn read_coord_vec(handle: &ProcessHandle, base: u32, off: usize, cap: usize) -> Result<CoordVec> {
    let a = rd_u32(handle, base as usize + off)?;
    let b = rd_u32(handle, base as usize + off + 4)?;
    let c = rd_u32(handle, base as usize + off + 8)?;
    let span = b.wrapping_sub(a) as usize;
    // A vector of 8-byte Coords: first <= last <= end, and the span divides by 8.
    // An empty MSVC vector has all three pointers null, which is valid -- treating
    // that as implausible was wrong and read as "the offset is bad" when the real
    // answer was "nothing is visible yet".
    let empty_ok = a == 0 && b == 0 && c == 0;
    let plausible =
        empty_ok || (a != 0 && b >= a && c >= b && span % 8 == 0 && span <= 8 * 100_000);
    let len = if plausible && !empty_ok { span / 8 } else { 0 };
    let mut coords = Vec::new();
    if plausible && len > 0 {
        let want = len.min(cap);
        let bytes = copy_address(a as usize, want * 8, handle)?;
        for i in 0..want {
            let x = i32::from_le_bytes(bytes[i * 8..i * 8 + 4].try_into().unwrap());
            let y = i32::from_le_bytes(bytes[i * 8 + 4..i * 8 + 8].try_into().unwrap());
            coords.push((x, y));
        }
    }
    Ok(CoordVec {
        member_offset: format!("+0x{:X}", off),
        first: a,
        last: b,
        end: c,
        len,
        capacity: (c.wrapping_sub(a) as usize) / 8,
        plausible,
        coords,
    })
}

#[derive(Serialize, Debug)]
pub struct FovState {
    pub fov_obj_ptr_addr: String,
    pub fov_obj: u32,
    pub visible: CoordVec,
    pub sibling_a: CoordVec,
    pub sibling_b: CoordVec,
    pub note: &'static str,
}

/// Read the FOV containers. `cap` bounds how many coordinates are returned.
pub fn read_fov(handle: &ProcessHandle, cap: usize) -> Result<FovState> {
    let obj = rd_u32(handle, FOV_OBJ_PTR)?;
    if obj == 0 {
        return Err(anyhow!(
            "FOV object pointer at 0x{:08X} is null -- no map loaded yet",
            FOV_OBJ_PTR
        ));
    }
    Ok(FovState {
        fov_obj_ptr_addr: format!("0x{:08X}", FOV_OBJ_PTR),
        fov_obj: obj,
        visible: read_coord_vec(handle, obj, FOV_VISIBLE_OFF, cap)?,
        sibling_a: read_coord_vec(handle, obj, FOV_SIBLING_A_OFF, cap)?,
        sibling_b: read_coord_vec(handle, obj, FOV_SIBLING_B_OFF, cap)?,
        note: "`visible` is the container isVisible() queries. The siblings are the \
               adjacent containers; which one is `remembered` is not yet confirmed. \
               `plausible` false means the three pointers do not look like a vector \
               of 8-byte Coords, so the offset is wrong or the map is mid-load.",
    })
}

#[derive(Serialize, Debug)]
pub struct CellProbe {
    pub x: i32,
    pub y: i32,
    pub index: usize,
    pub slot_addr: u32,
    pub cell_ptr: u32,
    pub raw_hex: String,
    pub decoded: Option<DirectCell>,
    pub type_ptr: u32,
    pub type_raw_hex: String,
    pub note: &'static str,
}

/// Dump one cell's raw bytes alongside the decoded view.
///
/// The offsets in this module are inferred from static analysis of accessor
/// bodies. This exists so that can be checked against a live game in one step
/// instead of by inference: stand somewhere known, probe the cell, and confirm
/// that `x`/`y` match where you are standing and that `entity` is non-zero.
pub fn probe(handle: &ProcessHandle, x: i32, y: i32) -> Result<CellProbe> {
    let (width, height, cells_base) = read_header(handle)?;
    if x < 0 || y < 0 || x >= width || y >= height {
        return Err(anyhow!(
            "({},{}) is outside the {}x{} map",
            x,
            y,
            width,
            height
        ));
    }

    let index = (x as usize) * (height as usize) + (y as usize);
    let slot_addr = cells_base + (index as u32) * 4;
    let cell_ptr = rd_u32(handle, slot_addr as usize)?;

    let mut raw_hex = String::new();
    let mut decoded = None;
    let mut type_ptr = 0u32;
    let mut type_raw_hex = String::new();

    if cell_ptr != 0 {
        let cb = copy_address(cell_ptr as usize, CELL_READ_LEN, handle)?;
        raw_hex = cb
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if i % 16 == 0 && i != 0 {
                    format!("\n{:02x}", b)
                } else {
                    format!("{:02x}", b)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let word = |off: usize| -> u32 {
            u32::from_le_bytes([cb[off], cb[off + 1], cb[off + 2], cb[off + 3]])
        };
        type_ptr = word(CELL_TYPE);
        if type_ptr != 0 {
            if let Ok(tb) = copy_address(type_ptr as usize, 0x60, handle) {
                type_raw_hex = tb
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }
        let pid = if type_ptr == 0 {
            None
        } else {
            rd_i32(handle, type_ptr as usize + CELLTYPE_ID).ok()
        };
        decoded = Some(DirectCell {
            x: word(CELL_X) as i32,
            y: word(CELL_Y) as i32,
            glyph: char::from_u32(word(CELL_GLYPH) & 0x7F).unwrap_or('?'),
            color: word(CELL_COLOR) as i32,
            unk_0c: word(CELL_UNK_0C) as i32,
            passable: cb[CELL_PASSABLE] != 0,
            transparent: cb[CELL_TRANSPARENT] != 0,
            unk_2c: word(CELL_UNK_2C) as i32,
            unk_40: word(CELL_UNK_40) as i32,
            unk_98: word(CELL_UNK_98) as i32,
            unk_a0: word(CELL_UNK_A0) as i32,
            cell_id: pid,
            cell_name: pid.and_then(CellId::from_id).map(|c| c.name()),
            type_class: if type_ptr == 0 {
                None
            } else {
                rd_i32(handle, type_ptr as usize + CELLTYPE_CLASS).ok()
            },
            prop: word(CELL_PROP),
            entity: word(CELL_ENTITY),
            item_container: word(CELL_ITEM_CONTAINER),
        });
    }

    Ok(CellProbe {
        x,
        y,
        index,
        slot_addr,
        cell_ptr,
        raw_hex,
        decoded,
        type_ptr,
        type_raw_hex,
        note: "decoded.x/decoded.y must equal the requested x/y. If they do not, \
               the Cell offsets or the column-major index are wrong for this build.",
    })
}
