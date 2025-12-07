# Developer Notes for Statmind

## Overview
Statmind is a Rust-based harness for the game Cogmind (Win32 PE executable). It uses a memory contract (shared structs) to read game data and expose it (currently via Discord Rich Presence).

## Beta 16 Update (Latest Changes)
The codebase has been updated to support Cogmind Beta 16. This involved:
1.  Restoring `build.rs` to auto-generate Rust enums from `src/*.txt` definitions.
2.  Updating `src/types.rs` to match the new memory layout and enum values.
3.  Fixing build errors caused by library updates (`sysinfo` v4, `discord-rich-presence`).

## Source of Truth
*   **Struct Layouts**: `~/sources/kyz/luigiai.hpp` (header file defining C structs).
*   **Map/Item IDs**: `src/*.txt` files (`cellID.txt`, `entityID.txt`, `itemID.txt`, `propID.txt`).
*   **Map Types (Proto)**: `cogmind-scoresheet-prerelease/scoresheet.proto` (submodule).

## MapType Logic (Critical)
The `MapType` enum values in `src/types.rs` are **derived from the sequential order** of the `MapType` enum variants in `scoresheet.proto`.

**IMPORTANT**: The integer values assigned in the `.proto` file (e.g., `MAP_SUB = 36`) are for backwards compatibility/stability in protobuf serialization. They are **NOT** the values used in the game's internal memory (`LuigiAi` struct). The game uses the 0-indexed list order.

Example Mapping (Beta 16):
*   `MAP_NONE` (Order 0) -> `MapType::MapNone` = 0
*   `MAP_SAN` (Order 1) -> `MapType::MapSan` = 1
*   ...
*   `MAP_REC` (Order 11) -> `MapType::MapRec` = 11
*   `MAP_SCR` (Order 12) -> `MapType::MapScr` = 12
*   `MAP_WAS` (Order 13) -> `MapType::MapWas` = 13
*   `MAP_GAR` (Order 14) -> `MapType::MapGar` = 14
*   `MAP_DSF` (Order 15) -> `MapType::MapDsf` = 15
*   `MAP_SUB` (Order 16) -> `MapType::MapSub` = 16

If you are updating `MapType` in the future, follow the **order** in `scoresheet.proto`, ignoring the explicit values (except for `W00`+ wins which seem to start at 1000).

## Code Generation (`build.rs`)
`build.rs` parses the text files in `src/` and generates `src/generated.rs`.
*   It supports `ID Name` format (e.g., `itemID.txt`).
*   It supports `ID Tag Name` format with headers (e.g., `propID.txt`, using `Tag` as identifier).
*   It adds `#[repr(i32)]` to enums to ensure binary compatibility with C `int` fields in `src/types.rs`.

## Struct Alignment
`src/types.rs` structs use `#[repr(C)]`.
*   `LuigiItem` was modified to remove the `equipped: bool` field, which was present in Rust but absent in the authoritative `luigiai.hpp`, causing layout misalignment.
*   Pointers in C++ (`LuigiProp*`, etc.) are represented as `u32` (assuming 32-bit build/process).

## Library Notes
*   `sysinfo`: Updated to v4+. Trait-based extensions (`PidExt`, `ProcessExt`, `SystemExt`) are removed. Use inherent methods (e.g., `pid.as_u32()`, `proc.name()`). `refresh_processes` now requires arguments.
*   `discord-rich-presence`: `DiscordIpcClient::new` no longer returns a `Result`, so remove `?` or `map_err`.
