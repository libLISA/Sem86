use bilge::prelude::*;
use serde::{Deserialize, Serialize};

use crate::impl_bank;

impl_bank! {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct DispiRegisters -> u16 {
        pub id (0): DispiVersion,
        pub xres (1): u16,
        pub yres (2): u16,
        pub bpp (3): u16,
        pub enable (4): Enable,
        pub bank (5): u16,
        pub virt_width (6): u16,
        pub virt_height (7): u16,
        pub x_offset (8): u16,
        pub y_offset (9): u16,
        pub video_memory_64k (10): u16,
        pub ddc (11): u16,
    }
}

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
pub enum DispiVersion {
    #[default]
    V0,
    V1,
    V2,
    V3,
    V4,
    V5,
}

impl From<u16> for DispiVersion {
    fn from(val: u16) -> Self {
        match val {
            0xB0C0 => DispiVersion::V0,
            0xB0C1 => DispiVersion::V1,
            0xB0C2 => DispiVersion::V2,
            0xB0C3 => DispiVersion::V3,
            0xB0C4 => DispiVersion::V4,
            0xB0C5 => DispiVersion::V5,
            _ => DispiVersion::V0,
        }
    }
}

impl From<DispiVersion> for u16 {
    fn from(val: DispiVersion) -> Self {
        match val {
            DispiVersion::V0 => 0xB0C0,
            DispiVersion::V1 => 0xB0C1,
            DispiVersion::V2 => 0xB0C2,
            DispiVersion::V3 => 0xB0C3,
            DispiVersion::V4 => 0xB0C4,
            DispiVersion::V5 => 0xB0C5,
        }
    }
}

#[bitsize(16)]
#[derive(Copy, Clone, Default, DebugBits, PartialEq, Eq, FromBits, Serialize, Deserialize)]
pub struct Enable {
    pub vbe_enabled: bool,
    pub caps: bool,
    reserved: u2,
    pub bank_granularity_32k: bool,
    pub dac_8bit: bool,

    /// Does nothing: recent Bochs' versions always map LFB.
    pub lfb_enabled: bool,
    pub no_clear_mem: bool,
    reserved: u8,
}
