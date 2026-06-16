use bilge::prelude::*;
use serde::{Deserialize, Serialize};

use crate::impl_bank;

impl_bank! {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct CrtRegisters -> u8 {
        pub horizontal_total (0): u8,
        pub horizontal_display_enable_end (1): u8,
        pub start_horizontal_blanking (2): u8,
        pub end_horizontal_blanking (3): EndHorizontalBlanking,
        pub start_horizontal_retrace_pulse (4): u8,
        pub end_horizontal_retrace (5): EndHorizontalRetrace,
        pub vertical_total (6): u8,
        pub overflow (7): Overflow,
        pub preset_row_scan (8): u8, // TODO
        pub maximum_scan_line (9): MaximumScanLine,
        pub cursor_start (10): CursorStart,
        pub cursor_end (11): CursorEnd,
        pub start_address_high (12): u8,
        pub start_address_low (13): u8,
        pub cursor_location_high (14): u8,
        pub cursor_location_low (15): u8,
        pub vertical_retrace_start (16): u8,
        pub vertical_retrace_end (17): VerticalRetraceEnd,
        pub vertical_display_enable_end (18): u8,
        pub offset (19): u8,
        pub underline_location (20): UnderlineLocation,
        pub start_vertical_blanking (21): u8,
        pub end_vertical_blanking (22): u8,
        pub crt_mode_control (23): ModeControl,
        pub line_compare (24): u8,
    }
}

impl CrtRegisters {
    pub fn effective_width(&self) -> u16 {
        (self.horizontal_display_enable_end as u16 + 1) * 8
    }

    pub fn effective_height(&self) -> u16 {
        let height = (self.vertical_display_enable_end as u16
            | (self.overflow.vde8().as_u16() << 8)
            | (self.overflow.vde9().as_u16() << 9))
            + 1;

        if self.maximum_scan_line.double_scanline_conversion() {
            height / 2
        } else {
            height
        }
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndHorizontalBlanking {
    end_blanking: u5,
    display_enable_skew_control: u2,
    reserved: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndHorizontalRetrace {
    end_horizontal_retrace_low5: u5,
    horizontal_retrace_delay: u2,
    end_horizontal_retrace_high1: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overflow {
    vt8: u1,
    vde8: u1,
    vrs8: u1,
    vbs8: u1,
    lc8: u1,
    vt9: u1,
    vde9: u1,
    vrs9: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaximumScanLine {
    maximum_scan_line: u5,
    vbs9: u1,
    lc9: u1,
    double_scanline_conversion: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorStart {
    row_scan_cursor_begin: u5,
    cursor_off: bool,
    reserved: u2,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorEnd {
    row_scan_cursor_end: u5,
    cursor_skew_control: u2,
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerticalRetraceEnd {
    pub vertical_retrace_end: u4,
    pub clear_vertical_interrupt: bool,
    pub enable_vertical_interrupt: bool,
    pub select_5_refresh_cycles: bool,
    pub protect_registers_0_to_7: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnderlineLocation {
    start_underline: u5,
    cb4: bool,
    dw: bool,
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeControl {
    cms0: bool,
    select_row_scan_counter: bool,
    horizontal_retrace_select: bool,
    count_by_two: bool,
    reserved: u1,
    address_wrap: bool,
    word_byte_mode: bool,
    hardware_reset: bool,
}
