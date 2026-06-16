use bilge::prelude::*;
use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::impl_bank;

impl_bank! {
    #[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
    pub struct SequencerRegisters -> u8 {
        pub reset (0): Reset,
        pub clocking_mode (1): ClockingMode,
        pub map_mask (2): MapMask,
        pub character_map_select (3): CharacterMapSelect,
        pub memory_mode (4): MemoryMode,
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Reset {
    /// true = Normal operation.
    /// false = Reset.
    asynchronous_reset: bool,

    /// true = Normal operation.
    /// false = Reset.
    synchronous_reset: bool,
    reserved: u6,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, Serialize, Deserialize, Encode, Decode)]
pub struct ClockingMode {
    /// When false, 9-dot wide mode.
    /// when true, 8-dot wide mode.
    pub dot_89: bool,

    /// Always one
    one: u1,
    pub shift_load: bool,
    pub dot_clock: bool,
    pub shift_4: bool,
    pub screen_off: bool,
    reserved: u2,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, Serialize, Deserialize, Encode, Decode)]
pub struct MapMask {
    pub map_enabled: [bool; 4],
    reserved: u4,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, Serialize, Deserialize, Encode, Decode)]
pub struct CharacterMapSelect {
    map_b_low2: u2,
    map_a_low2: u2,
    map_b_high1: u1,
    map_a_high1: u1,
    reserved: u2,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Default, Serialize, Deserialize, Encode, Decode)]
pub struct MemoryMode {
    reserved: u1,
    pub extended_memory: bool,

    /// false = even addresses map to maps 0 and 2, odd to 1 and 3.
    /// true = addresses map sequentially.
    pub disable_odd_even: bool,

    /// false = enables system addresses to sequentially access data within a bit map by using the map mask register.
    /// true = the 2 low-order bits of the address select the map accessed.
    pub chain_4: bool,
    reserved: u4,
}
