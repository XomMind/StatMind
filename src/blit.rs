//! Reader for the SDL blit sniffer.
//!
//! Cogmind re-composites the entire screen every frame instead of tracking dirty
//! rectangles. Every tile it draws is a blit, so the set of blit destinations in
//! a frame *is* the player's field of view -- including partial occlusion around
//! machinery, sensor blips, and every other case that a reconstruction would get
//! wrong, because this is the render itself rather than a model of it.
//!
//! The source rectangle within the font atlas identifies which glyph was drawn,
//! so a frame gives glyph-and-position for the whole screen: map pane, message
//! log, parts list and HUD alike.
//!
//! Layout verified by compiling an `offsetof` dump of the C definition rather
//! than assumed -- `StatmindBlit` is 20 bytes, not the 16 its fields suggest.

use crate::scan;
use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};
use serde::Serialize;

pub const BLIT_MAGIC: u32 = 0xBADB_EEF3;
pub const BLIT_CHECK: u32 = 0x7953_3EDB;

// Offsets verified by compiling an offsetof dump of the C struct.
pub const OFF_ENABLED: usize = 8;
pub const OFF_CLEAR_REQ: usize = 12;
pub const OFF_CLEAR_SEEN: usize = 16;
pub const OFF_FRAME: usize = 20;
pub const OFF_COUNT: usize = 24;
pub const OFF_DROPPED: usize = 28;
pub const OFF_TOTAL: usize = 32;
pub const OFF_MAX: usize = 36;
pub const OFF_DRAWS: usize = 40;

pub const DRAW_SIZE: usize = 20;
pub const DRAW_MAX: usize = 16384;

pub const KIND_BLIT: u16 = 0;
pub const KIND_FILL: u16 = 1;

/// Screen geometry, measured rather than assumed.
///
/// A character occupies 24x24 pixels but is drawn as **two** 12x24 blits, left
/// half then right: `dx mod 24` splits 50/50 between 0 and 12, and 164 of 166
/// blit cells have a horizontal 12px neighbour. So a screen cell is
/// `(dx / 24, dy / 24)`; treating the blit width as the cell width doubles the
/// horizontal resolution and was an early mistake.
pub const CELL_W: i32 = 24;
pub const CELL_H: i32 = 24;

/// The map view's top-left cell in game coordinates, as a pair of i32s.
///
/// `screen_col = game_x - origin.x`, `screen_row = game_y - origin.y`.
///
/// Calibrated by stepping the game's own keyboard cursor one cell and correlating
/// `LuigiAi.mapCursorIndex` (an exact game coordinate, still live in b17.1)
/// against the screen cell that got redrawn -- a cursor step redraws almost
/// nothing, so the busiest cell *is* the cursor. Confirmed on four axes.
///
/// The offset is **not** a constant: it moved from (32,8) to (27,8) when the
/// player walked around, so it has to be read rather than baked in. This address
/// was then found by scanning for the pair (27,8) and is the only static
/// candidate; it reads back exactly the value the cursor-step derivation implied.
///
/// One matching sample, so treat it as probable until it has been watched across
/// a scroll. `view_origin_from_cursor` is the ground-truth fallback.
pub const VIEW_ORIGIN: usize = 0x00CD_8FA4;

#[derive(Serialize, Debug)]
pub struct ViewOrigin {
    pub addr: String,
    pub x: i32,
    pub y: i32,
    pub plausible: bool,
}

pub fn read_view_origin(handle: &ProcessHandle) -> Result<ViewOrigin> {
    let x = rd_u32(handle, VIEW_ORIGIN)? as i32;
    let y = rd_u32(handle, VIEW_ORIGIN + 4)? as i32;
    Ok(ViewOrigin {
        addr: format!("0x{:08X}", VIEW_ORIGIN),
        x,
        y,
        // A map is at most a few hundred cells on a side.
        plausible: (0..1024).contains(&x) && (0..1024).contains(&y),
    })
}

/// Screen cell -> game coordinate, given the view origin.
pub fn screen_to_game(dx: i32, dy: i32, ox: i32, oy: i32) -> (i32, i32) {
    (dx / CELL_W + ox, dy / CELL_H + oy)
}

/// Game coordinate -> top-left pixel of that cell.
pub fn game_to_pixel(x: i32, y: i32, ox: i32, oy: i32) -> (i32, i32) {
    ((x - ox) * CELL_W, (y - oy) * CELL_H)
}

#[derive(Serialize, Debug, Clone, Copy)]
pub struct Draw {
    pub dx: i16,
    pub dy: i16,
    pub w: i16,
    pub h: i16,
    /// Blit: source position in the atlas, which identifies the glyph.
    pub sx: i16,
    pub sy: i16,
    /// 0 = blit (a glyph), 1 = fill (a cell background).
    pub kind: u16,
    /// Blit: source surface id. Fill: the colour.
    pub arg: u32,
}

#[derive(Serialize, Debug)]
pub struct BlitStatus {
    pub addr: String,
    pub enabled: i32,
    pub clear_req: u32,
    pub clear_seen: u32,
    /// Frames since the last clear. Read once this stops moving.
    pub frame: u32,
    pub count: u32,
    pub dropped: u32,
    pub total: u32,
    pub max: u32,
}

fn rd_u32(handle: &ProcessHandle, addr: usize) -> Result<u32> {
    let b = copy_address(addr, 4, handle)?;
    Ok(u32::from_le_bytes(
        b.try_into().map_err(|_| anyhow!("short read at 0x{:X}", addr))?,
    ))
}

/// Locate the log by its magic pair. Cached by the caller.
pub fn find(handle: &ProcessHandle) -> Result<usize> {
    let cands = scan::scan_new(handle, BLIT_MAGIC as i32)?;
    for a in cands {
        if rd_u32(handle, a + 4).map(|v| v == BLIT_CHECK).unwrap_or(false) {
            return Ok(a);
        }
    }
    Err(anyhow!(
        "blit log not found -- is the rebuilt SDL.dll loaded? (the game must be \
         restarted after replacing it)"
    ))
}

pub fn status(handle: &ProcessHandle, base: usize) -> Result<BlitStatus> {
    Ok(BlitStatus {
        addr: format!("0x{:08X}", base),
        enabled: rd_u32(handle, base + OFF_ENABLED)? as i32,
        clear_req: rd_u32(handle, base + OFF_CLEAR_REQ)?,
        clear_seen: rd_u32(handle, base + OFF_CLEAR_SEEN)?,
        frame: rd_u32(handle, base + OFF_FRAME)?,
        count: rd_u32(handle, base + OFF_COUNT)?,
        dropped: rd_u32(handle, base + OFF_DROPPED)?,
        total: rd_u32(handle, base + OFF_TOTAL)?,
        max: rd_u32(handle, base + OFF_MAX)?,
    })
}

/// Read the accumulated draw list.
pub fn read_draws(handle: &ProcessHandle, base: usize, limit: usize) -> Result<(u32, u32, Vec<Draw>)> {
    let frame = rd_u32(handle, base + OFF_FRAME)?;
    let count = rd_u32(handle, base + OFF_COUNT)? as usize;
    if count > DRAW_MAX {
        return Err(anyhow!("draw log count {} exceeds the {} cap", count, DRAW_MAX));
    }
    let want = count.min(limit);
    let mut out = Vec::with_capacity(want);
    if want > 0 {
        let bytes = copy_address(base + OFF_DRAWS, want * DRAW_SIZE, handle)?;
        for i in 0..want {
            let o = i * DRAW_SIZE;
            let s = |k: usize| i16::from_le_bytes([bytes[o + k], bytes[o + k + 1]]);
            let u = |k: usize| u16::from_le_bytes([bytes[o + k], bytes[o + k + 1]]);
            out.push(Draw {
                dx: s(0), dy: s(2), w: s(4), h: s(6),
                sx: s(8), sy: s(10), kind: u(12),
                arg: u32::from_le_bytes([bytes[o+16], bytes[o+17], bytes[o+18], bytes[o+19]]),
            });
        }
    }
    Ok((frame, count as u32, out))
}
