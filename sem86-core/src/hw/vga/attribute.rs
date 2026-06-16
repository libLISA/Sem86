use bilge::prelude::*;
use serde::{Deserialize, Serialize};

use crate::impl_bank;

impl_bank! {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct AttributeRegisters -> u8 {
        internal_palette0 (0): u8,
        internal_palette1 (1): u8,
        internal_palette2 (2): u8,
        internal_palette3 (3): u8,
        internal_palette4 (4): u8,
        internal_palette5 (5): u8,
        internal_palette6 (6): u8,
        internal_palette7 (7): u8,
        internal_palette8 (8): u8,
        internal_palette9 (9): u8,
        internal_palette10 (10): u8,
        internal_palette11 (11): u8,
        internal_palette12 (12): u8,
        internal_palette13 (13): u8,
        internal_palette14 (14): u8,
        internal_palette15 (15): u8,
        mode_control (16): ModeControl,
        overscan_color (17): u8,
        color_plane_enable (18): u8,
        horizontal_pel_scanning (19): u8,
        color_select (20): u8,
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeControl {
    /// 0 = alphanumeric mode, 1 = graphics mode
    graphics_enabled: bool,
    mono_emulation: bool,
    enable_line_graphics: bool,
    enable_blink: bool,
    reserved: u1,
    pel_panning_compatibility_enabled: bool,
    pel_width: bool,
    pel_select: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorSelect {
    sc5_sc4: u2,
    sc7_sc6: u2,
    reserved: u4,
}

impl AttributeRegisters {
    pub fn palette(&self) -> [u8; 16] {
        [
            self.internal_palette0,
            self.internal_palette1,
            self.internal_palette2,
            self.internal_palette3,
            self.internal_palette4,
            self.internal_palette5,
            self.internal_palette6,
            self.internal_palette7,
            self.internal_palette8,
            self.internal_palette9,
            self.internal_palette10,
            self.internal_palette11,
            self.internal_palette12,
            self.internal_palette13,
            self.internal_palette14,
            self.internal_palette15,
        ]
    }

    #[allow(unused)]
    pub fn set_palette(&mut self, new_palette: [u8; 16]) {
        [
            self.internal_palette0,
            self.internal_palette1,
            self.internal_palette2,
            self.internal_palette3,
            self.internal_palette4,
            self.internal_palette5,
            self.internal_palette6,
            self.internal_palette7,
            self.internal_palette8,
            self.internal_palette9,
            self.internal_palette10,
            self.internal_palette11,
            self.internal_palette12,
            self.internal_palette13,
            self.internal_palette14,
            self.internal_palette15,
        ] = new_palette
    }
}
