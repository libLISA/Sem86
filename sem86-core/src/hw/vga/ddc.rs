use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::info;
use serde::{Deserialize, Serialize};

// TODO: Implement actual generation of EDID data. For now we're borrowing from Bochs' `iodev/display/ddc.cc`
const BOCHS_EDID_DATA: &[u8] = &[
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, /* 0x0000 8-byte header */
    0x04, 0x21, /* 0x0008 Vendor ID ("AAA") */
    0xAB, 0xCD, /* 0x000A Product ID */
    0x00, 0x00, 0x00, 0x00, /* 0x000C Serial number (none) */
    12, 11, /* 0x0010 Week of manufacture (12) and year of manufacture (2001) */
    0x01, 0x03, /* 0x0012 EDID version number (1.3) */
    0x0F, /* 0x0014 Video signal interface (analogue, 0.700 : 0.300 : 1.000 V p-p,
          Video Setup: Blank Level = Black Level, Separate Sync H & V Signals are
          supported, Composite Sync Signal on Horizontal is supported, Composite
          Sync Signal on Green Video is supported, Serration on the Vertical Sync
          is supported) */
    0x21, 0x19, /* 0x0015 Scren size (330 mm * 250 mm) */
    0x78, /* 0x0017 Display gamma (2.2) */
    0x0F, /* 0x0018 Feature flags (no DMPS states, RGB, preferred timing mode, display is continuous frequency) */
    0x78, 0xF5, /* 0x0019 Least significant bits for chromaticity and default white point */
    0xA6, 0x55, 0x48, 0x9B, 0x26, 0x12, 0x50, 0x54,
    /* 0x001B Most significant bits for chromaticity and default white point */
    0xFF, /* 0x0023 Established timings 1 (720 x 400 @ 70Hz, 720 x 400 @ 88Hz,
          640 x 480 @ 60Hz, 640 x 480 @ 67Hz, 640 x 480 @ 72Hz, 640 x 480 @ 75Hz,
          800 x 600 @ 56Hz, 800 x 600 @ 60Hz) - historical resolutions */
    0xEF, /* 0x0024 Established timings 2 (800 x 600 @ 72Hz, 800 x 600 @ 75Hz, 832 x 624 @ 75Hz
          not 1024 x 768 @ 87Hz(I), 1024 x 768 @ 60Hz, 1024 x 768 @ 70Hz,
          1024 x 768 @ 75Hz, 1280 x 1024 @ 75Hz) - historical resolutions */
    0x80, /* 0x0025 Established timings 2 (1152 x 870 @ 75Hz and no manufacturer timings) */
    /* Standard timing */
    /* First byte: X resolution, divided by 8, less 31 (256–2288 pixels) */
    /* bit 7-6, X:Y pixel ratio: 00=16:10; 01=4:3; 10=5:4; 11=16:9 */
    /* bit 5-0, Vertical frequency, less 60 (60–123 Hz), nop 01 01 */
    0x31, 0x59, /* 0x0026 Standard timing #1 (640 x 480 @ 85 Hz) */
    0x45, 0x59, /* 0x0028 Standard timing #2 (800 x 600 @ 85 Hz) */
    0x61, 0x59, /* 0x002A Standard timing #3 (1024 x 768 @ 85 Hz) */
    0x81, 0xCA, /* 0x002C Standard timing #4 (1280 x 720 @ 70 Hz) */
    0x81, 0x0A, /* 0x002E Standard timing #5 (1280 x 800 @ 70 Hz) */
    0xA9, 0xC0, /* 0x0030 Standard timing #6 (1600 x 900 @ 60 Hz) */
    0xA9, 0x40, /* 0x0034 Standard timing #7 (1600 x 1200 @ 60 Hz) */
    0xD1, 0x00, /* 0x0032 Standard timing #8 (1920 x 1080 @ 60 Hz) */
    /* 0x0036 First 18-byte descriptor (1920 x 1200) */
    0x3C, 0x28, /*        Pixel clock = 154000000 Hz */
    0x80, /* 0x0038 Horizontal addressable pixels low byte (0x0780 & 0xFF) */
    0xA0, /* 0x0039 Horizontal blanking low byte (0x00A0 & 0xFF) */
    0x70, /* 0x003A Horizontal addressable pixels high 4 bits ((0x0780 & 0x0F00) >> 4), and */
    /*        Horizontal blanking high 4 bits ((0x00A0 & 0x0F00 ) >> 8) as low bits */
    0xB0, /* 0x003B Vertical addressable pixels low byte (0x04B0 & 0xFF) */
    0x23, /* 0x003C Vertical blanking low byte (0x0023 & 0xFF) */
    0x40, /* 0x003D Vertical addressable pixels high 4 bits ((0x04B0 & 0x0F00) >> 4), and */
    /*        Vertical blanking high 4 bits ((0x0024 & x0F00) >> 8) */
    0x30, /* 0x003E Horizontal front porch in pixels low byte (0x0030 & 0xFF) */
    0x20, /* 0x003F Horizontal sync pulse width in pixels low byte (0x0020 & 0xFF) */
    0x36, /* 0x0040 Vertical front porch in lines low 4 bits ((0x0003 & 0x0F) << 4), and */
    /*        Vertical sync pulse width in lines low 4 bits (0x0006 & 0x0F) */
    0x00, /* 0x0041 Horizontal front porch pixels high 2 bits (0x0030 >> 8), and */
    /*        Horizontal sync pulse width in pixels high 2 bits (0x0020 >> 8), and */
    /*        Vertical front porch in lines high 2 bits (0x0003 >> 4), and */
    /*        Vertical sync pulse width in lines high 2 bits (0x0006 >> 4) */
    0x06, /* 0x0042 Horizontal addressable video image size in mm low 8 bits (0x0206 & 0xFF) */
    0x44, /* 0x0043 Vertical addressable video image size in mm low 8 bits (0x0144 & 0xFF) */
    0x21, /* 0x0044 Horizontal addressable video image size in mm high 8 bits (0x0206 >> 8), and */
    /*        Vertical addressable video image size in mm high 8 bits (0x0144 >> 8) */
    0x00, /* 0x0045 Left and right border size in pixels (0x00) */
    0x00, /* 0x0046 Top and bottom border size in lines (0x00) */
    0x1E, /* 0x0047 Flags (non-interlaced, no stereo, analog composite sync, sync on */
    /*        all three (RGB) video signals) */


    /* 0x0048 Second 18-byte descriptor (1280 x 1024) */
    0x30, 0x2a, /*        Pixel clock = 108000000 Hz */
    0x00, /* 0x004A Horizontal addressable pixels low byte (0x0500 & 0xFF) */
    0x98, /* 0x004B Horizontal blanking low byte (0x0198 & 0xFF) */
    0x51, /* 0x004C Horizontal addressable pixels high 4 bits (0x0500 >> 8), and */
    /*        Horizontal blanking high 4 bits (0x0198 >> 8) */
    0x00, /* 0x004D Vertical addressable pixels low byte (0x0400 & 0xFF) */
    0x2A, /* 0x004E Vertical blanking low byte (0x002A & 0xFF) */
    0x40, /* 0x004F Vertical addressable pixels high 4 bits (0x0400 >> 8), and */
    /*        Vertical blanking high 4 bits (0x002A >> 8) */
    0x30, /* 0x0050 Horizontal front porch in pixels low byte (0x0030 & 0xFF) */
    0x70, /* 0x0051 Horizontal sync pulse width in pixels low byte (0x0070 & 0xFF) */
    0x13, /* 0x0052 Vertical front porch in lines low 4 bits (0x0001 & 0x0F), and */
    /*        Vertical sync pulse width in lines low 4 bits (0x0003 & 0x0F) */
    0x00, /* 0x0053 Horizontal front porch pixels high 2 bits (0x0030 >> 8), and */
    /*        Horizontal sync pulse width in pixels high 2 bits (0x0070 >> 8), and */
    /*        Vertical front porch in lines high 2 bits (0x0001 >> 4), and */
    /*        Vertical sync pulse width in lines high 2 bits (0x0003 >> 4) */
    0x2C, /* 0x0054 Horizontal addressable video image size in mm low 8 bits (0x012C & 0xFF) */
    0xE1, /* 0x0055 Vertical addressable video image size in mm low 8 bits (0x00E1 & 0xFF) */
    0x10, /* 0x0056 Horizontal addressable video image size in mm high 8 bits (0x012C >> 8), and */
    /*        Vertical addressable video image size in mm high 8 bits (0x00E1 >> 8) */
    0x00, /* 0x0057 Left and right border size in pixels (0x00) */
    0x00, /* 0x0058 Top and bottom border size in lines (0x00) */
    0x1E, /* 0x0059 Flags (non-interlaced, no stereo, analog composite sync, sync on */
    /*        all three (RGB) video signals) */
    0x00, 0x00, 0x00, 0xFF, 0x00, /* 0x005A Third 18-byte descriptor - display product serial number */
    b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', 0x0A, 0x20, 0x20, 0x00, 0x00, 0x00, 0xFC,
    0x00, /* 0x006C Fourth 18-byte descriptor - display product name  */
    b'B', b'o', b'c', b'h', b's', b' ', b'S', b'c', b'r', b'e', b'e', b'n', 0x0A,
    0x00, /* 0x007E Extension block count (none)  */
    0x00, /* 0x007F Checksum (set by constructor) */
];

#[bitsize(8)]
pub struct DdcValue {
    clock: bool,
    data: bool,
    monitor_clock: bool,
    monitor_data: bool,
    reserved: u4,
}

impl DdcValue {
    pub fn as_u8(&self) -> u8 {
        self.value
    }
}

enum Edge {
    Clock(bool),
    Data(bool),
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct I2C {
    state: State,
    cur_data: bool,
    cur_clock: bool,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum State {
    Idle,
    Address { addr: u8, index: usize },
    PendingAckAddress { is_read: bool },
    AckAddress { is_read: bool },
    Write { val: u8, index: usize },
    PendingAckData,
    AckData,
    Read { val: u8, index: usize },
    WaitForAck,
    PendingNextRead,
}

#[derive(Copy, Clone, Debug)]
enum I2CEvent {
    StartCondition,
    SetAddress {
        addr: u8,

        #[allow(unused)]
        is_read: bool,
    },
    Data {
        byte: u8,
    },
    ReadByte,
    StopCondition,
}

impl I2C {
    fn new() -> Self {
        Self {
            state: State::Idle,
            cur_data: true,
            cur_clock: true,
        }
    }

    fn incoming_edge(&mut self, edge: Edge) -> Option<I2CEvent> {
        match edge {
            Edge::Clock(clock) => self.cur_clock = clock,
            Edge::Data(data) => self.cur_data = data,
        }

        match (self.state, self.cur_data, edge) {
            (State::Idle, false, Edge::Clock(false)) => {
                self.state = State::Address {
                    addr: 0,
                    index: 7,
                };

                return Some(I2CEvent::StartCondition)
            },
            (
                State::Address {
                    addr,
                    index,
                },
                data,
                Edge::Clock(true),
            ) => {
                let addr = addr | ((data as u8) << index);

                if let Some(index) = index.checked_sub(1) {
                    self.state = State::Address {
                        addr,
                        index,
                    }
                } else {
                    let is_read = addr & 1 != 0;
                    self.state = State::PendingAckAddress {
                        is_read,
                    };
                    return Some(I2CEvent::SetAddress {
                        addr: addr >> 1,
                        is_read,
                    })
                }
            },
            (
                State::PendingAckAddress {
                    is_read,
                },
                _,
                Edge::Clock(false),
            ) => {
                self.state = State::AckAddress {
                    is_read,
                }
            },
            (
                State::AckAddress {
                    is_read: false,
                },
                _,
                Edge::Clock(false),
            ) => {
                self.state = State::Write {
                    val: 0,
                    index: 7,
                }
            },
            (
                State::AckAddress {
                    is_read: true,
                },
                _,
                Edge::Clock(false),
            ) => {
                self.state = State::Read {
                    val: 0,
                    index: 7,
                };

                return Some(I2CEvent::ReadByte)
            },
            (
                State::Write {
                    ..
                },
                _,
                Edge::Data(true),
            ) => {
                if self.cur_clock {
                    self.state = State::Idle;
                    return Some(I2CEvent::StopCondition)
                }
            },
            (
                State::Write {
                    val,
                    index,
                    ..
                },
                data,
                Edge::Clock(true),
            ) => {
                let val = val | ((data as u8) << index);

                if let Some(index) = index.checked_sub(1) {
                    self.state = State::Write {
                        val,
                        index,
                    }
                } else {
                    self.state = State::PendingAckData;
                    return Some(I2CEvent::Data {
                        byte: val,
                    })
                }
            },
            (State::PendingAckData, _, Edge::Clock(false)) => self.state = State::AckData,
            (State::AckData, _, Edge::Clock(false)) => {
                self.state = State::Write {
                    val: 0,
                    index: 7,
                }
            },

            (
                State::Read {
                    ..
                }
                | State::PendingNextRead,
                _,
                Edge::Data(true),
            ) => {
                if self.cur_clock {
                    self.state = State::Idle;
                    return Some(I2CEvent::StopCondition)
                }
            },
            (
                State::Read {
                    val,
                    index,
                    ..
                },
                _,
                Edge::Clock(false),
            ) => {
                if let Some(index) = index.checked_sub(1) {
                    self.state = State::Read {
                        val,
                        index,
                    }
                } else {
                    self.state = State::WaitForAck;
                }
            },
            (State::WaitForAck, data, Edge::Clock(true)) => {
                if data {
                    // NACK
                    self.state = State::Idle;
                    // TODO: return abort event?
                } else {
                    // ACK
                    self.state = State::PendingNextRead;
                }
            },
            (State::PendingNextRead, _, Edge::Clock(false)) => {
                self.state = State::Read {
                    val: 0,
                    index: 7,
                };
                return Some(I2CEvent::ReadByte)
            },
            _ => (),
        }

        None
    }

    fn set_byte(&mut self, byte: u8) {
        if let State::Read {
            val, ..
        } = &mut self.state
        {
            *val = byte;
        } else {
            panic!("Can only call set_byte when protocol is in reading state")
        }
    }

    fn data_out(&self) -> bool {
        match self.state {
            State::AckAddress {
                ..
            }
            | State::AckData => false,
            State::Read {
                val,
                index,
                ..
            } => (val >> index) & 1 != 0,
            _ => true,
        }
    }

    fn abort(&mut self) {
        self.state = State::Idle;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Ddc {
    clock: bool,
    data: bool,
    i2c: I2C,
    addr: u8,
    edid_index: u8,
}

impl Ddc {
    pub fn new() -> Self {
        Self {
            clock: true,
            data: true,
            i2c: I2C::new(),
            addr: 0,
            edid_index: 0,
        }
    }

    pub fn write(&mut self, clock: bool, data: bool) {
        let clock_edge = self.clock ^ clock;
        let data_edge = self.data ^ data;
        self.clock = clock;
        self.data = data;

        let ev = match (clock_edge, data_edge) {
            (false, false) | (true, true) => None,
            (false, true) => self.i2c.incoming_edge(Edge::Data(self.data)),
            (true, false) => self.i2c.incoming_edge(Edge::Clock(self.clock)),
        };

        info!("Event (addr=0x{:X}): {ev:02X?} (I2C: {:02X?})", self.addr, self.i2c);
        match (self.addr, ev) {
            (_, Some(I2CEvent::StartCondition)) => (),
            (
                _,
                Some(I2CEvent::SetAddress {
                    addr, ..
                }),
            ) => {
                self.addr = addr;

                // TODO: Monitor Control Command Set (MCCS) messages at address 0x37
                if addr != 0x50 {
                    self.i2c.abort();
                }
            },
            (
                0x50,
                Some(I2CEvent::Data {
                    byte,
                }),
            ) => {
                info!("Set edid_index = 0x{byte:X}");
                self.edid_index = byte;
            },
            (0x50, Some(I2CEvent::ReadByte)) => {
                let index = self.edid_index as usize & 0x7f;
                let byte = if index == 0x7f {
                    let checksum = BOCHS_EDID_DATA.iter().copied().fold(0u8, |a, b| a.wrapping_add(b));

                    if checksum == 0 { 0 } else { checksum.wrapping_neg() }
                } else {
                    BOCHS_EDID_DATA[index]
                };
                info!("Outputting EDID byte 0x{index:X} = 0x{byte:02X}");
                self.i2c.set_byte(byte);
                self.edid_index = self.edid_index.wrapping_add(1);
            },
            _ => (),
        }
    }

    pub fn read(&mut self) -> DdcValue {
        DdcValue::new(self.clock, self.data, self.clock, self.data && self.i2c.data_out())
    }
}
