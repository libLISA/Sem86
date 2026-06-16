use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::ide::Ide;
use crate::hw::HwMmio;

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct XtIde {
    hi: u8,
}

// Reference: http://www.freedoors.org/idework/specs/8038-r01.pdf
impl Default for XtIde {
    fn default() -> Self {
        Self::new()
    }
}

impl XtIde {
    pub fn new() -> Self {
        Self {
            hi: 0,
        }
    }

    pub fn write(&mut self, ide: &mut Ide, addr: u8, val: u8, mmio: &mut HwMmio<'_, '_>) {
        match addr {
            0x0 => ide.channels[0]
                .write::<u16>(addr, val as u16 | ((self.hi as u16) << 8), mmio)
                .unwrap(),
            0x1..=0x7 => ide.channels[0].write::<u8>(addr, val, mmio).unwrap(),
            0x8 => self.hi = val,
            0xe => ide.channels[0].write_control::<u8>(val).unwrap(),
            _ => unreachable!(),
        }
    }

    pub fn read(&mut self, ide: &mut Ide, addr: u8) -> u8 {
        match addr {
            0x0 => {
                let w = ide.channels[0].read::<u16>(addr).unwrap();
                self.hi = (w >> 8) as u8;
                w as u8
            },
            0x1..=0x7 => ide.channels[0].read::<u8>(addr).unwrap(),
            0x8 => self.hi,
            0xe => ide.channels[0].read_control::<u8>().unwrap(),
            _ => unreachable!(),
        }
    }

    pub fn snapshot(&self) -> XtIde {
        self.clone()
    }

    pub fn restore(&mut self, xtide: XtIde) {
        *self = xtide;
    }
}
