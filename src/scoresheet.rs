//! Reader for the live scoresheet-dump channel.
//!
//! Cogmind can serialise its whole run state on demand -- the manual calls it a
//! stat dump, bound to Alt-Shift-S -- and writes it to `dumps/` as text plus
//! JSON (with `jsonStatDump=1`). The shim calls that writer directly instead of
//! sending the keystroke, because the keybind only fires in one UI domain while
//! a direct call works with a menu open. See
//! `SDL-1.2/src/statmind_scoresheet.h` for the ABI and the fingerprints that
//! gate it.
//!
//! What the dump is worth: part loadouts by slot and name, resource *maxima*
//! (derived from parts, so they exist nowhere in memory to scan for), per-map
//! stats, discovered exits with destinations, and the known map as text. The
//! schema is recovered from the same binary by `harness/extract_proto.py`.
//!
//! Offsets verified by compiling an `offsetof` dump of the C struct.

use crate::scan;
use anyhow::{anyhow, Result};
use process_memory::{copy_address, ProcessHandle};
use serde::Serialize;

pub const SS_MAGIC: u32 = 0xBADB_EEF5;
pub const SS_CHECK: u32 = 0x7953_3EDD;

pub const OFF_STATUS: usize = 0x08;
pub const OFF_MODULE_BASE: usize = 0x0C;
pub const OFF_FN: usize = 0x10;
pub const OFF_SELF: usize = 0x14;
pub const OFF_REQ: usize = 0x18;
pub const OFF_DONE: usize = 0x1C;
pub const OFF_LAST_RESULT: usize = 0x20;
pub const OFF_CALLS: usize = 0x24;
pub const OFF_TEXT_LEN: usize = 0x28;
pub const OFF_TEXT: usize = 0x2C;
pub const TEXT_MAX: usize = 192;
pub const OFF_RET_RAW: usize = 0xEC;
pub const RET_RAW: usize = 32;

#[derive(Debug, Serialize)]
pub struct DumpStatus {
    pub addr: String,
    /// Raw `status` from the shim; see `status_text`.
    pub status: i32,
    pub status_text: &'static str,
    pub module_base: String,
    /// `Scorekeeper::outputScoresheet`, resolved and fingerprinted.
    pub writer: String,
    /// The `Scorekeeper` singleton, read out of the game's own call site.
    pub scorekeeper: String,
    pub req: u32,
    pub done: u32,
    pub last_result: i32,
    pub last_result_text: &'static str,
    pub calls: u32,
    /// The `std::string` the writer returned on the last call.
    pub returned: String,
    /// The head of the hidden return buffer, verbatim. `std::string` field
    /// offsets are an MSVC implementation detail, so this is here to diagnose a
    /// build that moves them without needing a shim rebuild.
    pub ret_raw: String,
}

pub fn status_text(v: i32) -> &'static str {
    match v {
        0 => "untried",
        1 => "ready",
        -1 => "disabled by STATMIND_SCORESHEET=0",
        -2 => "GetModuleHandle(NULL) failed",
        -3 => "no PE header at the module base",
        -4 => "writer fingerprint failed -- not Beta 17.1",
        -5 => "call site did not match -- no Scorekeeper pointer",
        -6 => "not a Win32 build",
        _ => "unknown",
    }
}

pub fn result_text(v: i32) -> &'static str {
    match v {
        0 => "no dump yet",
        1 => "ok",
        -1 => "writer not ready",
        -2 => "refused: re-entered the frame hook",
        _ => "unknown",
    }
}

fn rd_u32(handle: &ProcessHandle, addr: usize) -> Result<u32> {
    let b = copy_address(addr, 4, handle)?;
    Ok(u32::from_le_bytes(
        b.try_into().map_err(|_| anyhow!("short read at 0x{:X}", addr))?,
    ))
}

/// Locate the status block by its magic pair.
pub fn find(handle: &ProcessHandle) -> Result<usize> {
    for a in scan::scan_new(handle, SS_MAGIC as i32)? {
        if rd_u32(handle, a + 4).map(|v| v == SS_CHECK).unwrap_or(false) {
            return Ok(a);
        }
    }
    Err(anyhow!(
        "scoresheet block not found -- the loaded SDL.dll predates the dump \
         support (rebuild with harness/build-sdl.sh and restart the game)"
    ))
}

pub fn status(handle: &ProcessHandle, base: usize) -> Result<DumpStatus> {
    let st = rd_u32(handle, base + OFF_STATUS)? as i32;
    let lr = rd_u32(handle, base + OFF_LAST_RESULT)? as i32;
    let n = (rd_u32(handle, base + OFF_TEXT_LEN)? as usize).min(TEXT_MAX);
    let returned = if n > 0 {
        String::from_utf8_lossy(&copy_address(base + OFF_TEXT, n, handle)?).into_owned()
    } else {
        String::new()
    };
    Ok(DumpStatus {
        addr: format!("0x{:08X}", base),
        status: st,
        status_text: status_text(st),
        module_base: format!("0x{:08X}", rd_u32(handle, base + OFF_MODULE_BASE)?),
        writer: format!("0x{:08X}", rd_u32(handle, base + OFF_FN)?),
        scorekeeper: format!("0x{:08X}", rd_u32(handle, base + OFF_SELF)?),
        req: rd_u32(handle, base + OFF_REQ)?,
        done: rd_u32(handle, base + OFF_DONE)?,
        last_result: lr,
        last_result_text: result_text(lr),
        calls: rd_u32(handle, base + OFF_CALLS)?,
        returned,
        ret_raw: copy_address(base + OFF_RET_RAW, RET_RAW, handle)?
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" "),
    })
}
