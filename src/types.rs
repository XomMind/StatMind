use crate::generated::{CellId, EntityId, ItemId, PropId};
use std::mem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum MapType {
    MapNone = 0,
    MapYrd = 1,
    MapMat = 2,
    MapFac = 3,
    MapRes = 4,
    MapAcc = 5,
    MapSur = 6,
    MapMin = 7,
    MapExi = 8,
    MapSto = 9,
    MapRec = 10,
    MapScr = 11,
    MapWas = 12,
    MapGar = 13,
    MapDsf = 14,
    MapSub = 16,
    MapLow = 17,
    MapUpp = 18,
    MapPro = 19,
    MapDee = 20,
    MapZio = 21,
    MapDat = 22,
    MapZhi = 23,
    MapWar = 24,
    MapExt = 25,
    MapCet = 26,
    MapArc = 27,
    MapHub = 28,
    MapArm = 29,
    MapLab = 30,
    MapQua = 31,
    MapTes = 32,
    MapSec = 33,
    MapFrg = 34,
    MapCom = 35,
    MapAc0 = 36,
    MapLai = 37,
    MapTow = 38,
    MapW00 = 1000,
    MapW01 = 1001,
    MapW02 = 1002,
    MapW03 = 1003,
    MapW04 = 1004,
    MapW05 = 1005,
    MapW06 = 1006,
    MapW07 = 1007,
    MapW08 = 1008,
    MapW09 = 1009,
}

pub struct InvalidMapType(i32);

impl TryFrom<i32> for MapType {
    type Error = InvalidMapType;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::MapNone),
            1 => Ok(Self::MapYrd),
            2 => Ok(Self::MapMat),
            3 => Ok(Self::MapFac),
            4 => Ok(Self::MapRes),
            5 => Ok(Self::MapAcc),
            6 => Ok(Self::MapSur),
            7 => Ok(Self::MapMin),
            8 => Ok(Self::MapExi),
            9 => Ok(Self::MapSto),
            10 => Ok(Self::MapRec),
            11 => Ok(Self::MapScr),
            12 => Ok(Self::MapWas),
            13 => Ok(Self::MapGar),
            14 => Ok(Self::MapDsf),
            15 => Ok(Self::MapSub),
            16 => Ok(Self::MapLow),
            17 => Ok(Self::MapUpp),
            18 => Ok(Self::MapPro),
            19 => Ok(Self::MapDee),
            20 => Ok(Self::MapZio),
            21 => Ok(Self::MapDat),
            22 => Ok(Self::MapZhi),
            23 => Ok(Self::MapWar),
            24 => Ok(Self::MapExt),
            25 => Ok(Self::MapCet),
            26 => Ok(Self::MapArc),
            27 => Ok(Self::MapHub),
            28 => Ok(Self::MapArm),
            29 => Ok(Self::MapLab),
            30 => Ok(Self::MapQua),
            31 => Ok(Self::MapTes),
            32 => Ok(Self::MapSec),
            33 => Ok(Self::MapFrg),
            34 => Ok(Self::MapCom),
            35 => Ok(Self::MapAc0),
            36 => Ok(Self::MapLai),
            37 => Ok(Self::MapTow),
            1000 => Ok(Self::MapW00),
            1001 => Ok(Self::MapW01),
            1002 => Ok(Self::MapW02),
            1003 => Ok(Self::MapW03),
            1004 => Ok(Self::MapW04),
            1005 => Ok(Self::MapW05),
            1006 => Ok(Self::MapW06),
            1007 => Ok(Self::MapW07),
            1008 => Ok(Self::MapW08),
            1009 => Ok(Self::MapW09),
            _ => Err(InvalidMapType(value)),
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiMachineHacking {
    pub action_ready: i32,
    pub detect_chance: i32,
    pub trace_progress: i32,
    pub last_hack_success: bool,
}
impl From<&Vec<u8>> for LuigiMachineHacking {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiProp {
    pub id: PropId,
    pub interactive_piece: bool,
}
impl From<&Vec<u8>> for LuigiProp {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiItem {
    pub id: ItemId,
    pub integrity: i32,
}
impl From<&Vec<u8>> for LuigiItem {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiEntity {
    pub id: EntityId,
    pub integrity: i32,
    pub relation: i32,
    pub active_state: i32,
    pub exposure: i32,
    pub energy: i32,
    pub matter: i32,
    pub heat: i32,
    pub system_corruption: i32,
    pub speed: i32,
    pub inventory_size: i32,
    pub inventory: u32,
}
impl From<&Vec<u8>> for LuigiEntity {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiTile {
    pub last_action: i32,
    pub last_fov: i32,
    pub cell: CellId,
    pub door_open: bool,
    pub prop: u32,
    pub entity: u32,
    pub item: u32,
}
impl From<&Vec<u8>> for LuigiTile {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct LuigiAi {
    pub magic1: i32,
    pub magic2: i32,
    pub action_ready: i32,
    pub map_width: i32,
    pub map_height: i32,
    pub location_depth: i32,
    pub location_map: i32,
    pub map_data: u32,
    pub map_cursor_index: i32,
    pub player: u32,
    pub machine_hacking: u32,
}
impl From<&Vec<u8>> for LuigiAi {
    fn from(slice: &Vec<u8>) -> Self {
        let p: *const [u8; mem::size_of::<Self>()] =
            slice.as_ptr() as *const [u8; mem::size_of::<Self>()];
        unsafe { mem::transmute(*p) }
    }
}
