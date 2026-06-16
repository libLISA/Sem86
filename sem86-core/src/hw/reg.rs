use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum State {
    Lsb,
    Msb,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Reg16 {
    val: u16,
    state: State,
}

impl Reg16 {
    pub fn new(initial_val: u16) -> Self {
        Self {
            val: initial_val,
            state: State::Lsb,
        }
    }

    pub fn write(&mut self, val: u8) {
        *self = match self.state {
            State::Lsb => Self {
                val: (self.val & !0x00ff) | val as u16,
                state: State::Msb,
            },
            State::Msb => Self {
                val: (self.val & !0xff00) | ((val as u16) << 8),
                state: State::Lsb,
            },
        }
    }

    pub fn read(&mut self) -> u8 {
        let result;
        (result, self.state) = match self.state {
            State::Lsb => (self.val as u8, State::Msb),
            State::Msb => ((self.val >> 8) as u8, State::Lsb),
        };

        result
    }

    pub fn value(&self) -> u16 {
        self.val
    }

    pub fn make_next_byte_low(&mut self) {
        self.state = State::Lsb;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Reg8 {
    val: u8,
}

impl Reg8 {
    pub fn new(initial_val: u8) -> Self {
        Self {
            val: initial_val,
        }
    }

    pub fn write(&mut self, val: u8) {
        self.val = val;
    }

    pub fn read(&mut self) -> u8 {
        self.val
    }

    pub fn value(&self) -> u8 {
        self.val
    }
}
