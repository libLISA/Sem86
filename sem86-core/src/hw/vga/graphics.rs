use bilge::prelude::*;
use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::impl_bank;

impl_bank! {
    #[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
    pub struct GraphicsRegisters -> u8 {
        pub set_reset (0): MapValues,
        pub enable_set_reset (1): MapValues,
        pub color_compare (2): MapValues,
        pub data_rotate (3): DataRotate,
        pub read_map_select (4): ReadMapSelect,
        pub graphics_mode (5): GraphicsMode,
        pub miscellaneous (6): Miscellaneous,
        pub color_dont_care (7): MapValues,
        pub bit_mask (8): u8,
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct MapValues {
    pub values: [bool; 4],
    reserved: u4,
}

#[bitsize(2)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, FromBits, Serialize, Deserialize)]
#[repr(u8)]
pub enum DataRotateFunction {
    #[default]
    Unmodified,
    And,
    Or,
    Xor,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DataRotate {
    pub rotate_count: u3,
    pub function_select: DataRotateFunction,
    reserved: u3,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct ReadMapSelect {
    pub map_select: u2,
    reserved: u6,
}

#[bitsize(2)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, FromBits, Serialize, Deserialize)]
#[repr(u8)]
pub enum WriteMode {
    /// Mode 0.
    ///
    /// Write 0x00 or 0xff according to the bit in the set/reset register if enabled.
    /// Otherwise, write the data rotated by the count in the data rotate register.
    ///
    /// Only bits set to 1 in the bit mask register will be written.
    #[default]
    WriteSrOrData,

    /// Mode 1.
    ///
    /// Write the data from the latches.
    WriteLatches,

    /// Mode 2.
    ///
    /// Bits 0-3 determine whether 0x00 or 0xff is written to the corresponding map.
    ///
    /// Only bits set to 1 in the bit mask register will be written.
    OneBitPerMap,

    /// Mode 3.
    ///
    /// Bit in the set/reset register determines whether plane is written with 0x00 or 0xff.
    /// Set/reset enable has no effect.
    /// Rotate data by the count in the data rotate register, then AND it with the bit mask register.
    ///
    /// Only bits set to 1 in the computed mask will be written.
    WriteSr,
}

#[bitsize(1)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, FromBits, Serialize, Deserialize)]
#[repr(u8)]
pub enum ReadMode {
    /// Reads memory directly from video memory.
    ///
    /// Depending on chain-4 this is either from the map selected in the read map select register,
    /// or from the map determined from the lower 2 bits of the address.
    #[default]
    Normal,
    ColorCompare,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct GraphicsMode {
    pub write_mode: WriteMode,
    reserved: u1,
    pub read_mode: ReadMode,

    /// true = READ addresses map to maps 0 and 2, odd to 1 and 3.
    /// false = READ addresses use planar memory mode.
    pub odd_even: bool,
    pub shift_register_mode: bool,
    pub n256_color_mode: bool,
    reserved: u1,
}

#[bitsize(2)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, FromBits, Serialize, Deserialize)]
#[repr(u8)]
pub enum MemoryMap {
    A0000For128Kb,
    A0000For64Kb,
    B0000For32Kb,
    #[default]
    B8000For32Kb,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Miscellaneous {
    pub graphics_mode: bool,

    /// Replace bit 0 of the computed vram address with bit 16 from the address.
    pub odd_even: bool,
    pub memory_map: MemoryMap,
    reserved: u4,
}
