use arbitrary_int::{u2, u3, u6};
use bilge::prelude::*;

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum Phase {
    Data = 0,
    Command = 1,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum Direction {
    HostToDevice = 0,
    DeviceToHost = 1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct AtapiInterruptReason {
    phase: Phase,
    direction: Direction,
    rel: bool,
    tag: u5,
}

impl AtapiInterruptReason {
    pub fn as_u8(&self) -> u8 {
        self.value
    }
}

#[derive(Copy, Clone, Debug)]
pub enum StartStopCommand {
    Stop,
    Start,
    Eject,
    Load,
}

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
pub enum AtapiPacket {
    /// Does nothing when unit is OK, returns CHECK CONDITION status and sense code.
    TestUnitReady {
        logical_unit_number: u3,
    },
    RequestSense {
        logical_unit_number: u3,
        allocation_length: u8,
    },
    StartStopUnit {
        command: StartStopCommand,
    },
    MechanismStatus,
    ModeSense {
        logical_unit_number: u3,
        pc: u2,
        page_code: u6,
        allocation_length: u16,
        flag: bool,
        link: bool,
    },
    Inquiry {
        logical_unit_number: u3,
        allocation_length: u8,
        flag: bool,
        link: bool,
    },
    ReadCdromCapacity,
    ReadCd,
    ReadToc {
        msf: bool,
        logical_unit_number: u3,
        track_number: u8,
        allocation_length: u16,
        flag: bool,
        link: bool,
        starting_track: u8,
        format: u8,
    },
    Read {
        lba: u32,
        transfer_length: u32,
    },
    Seek,
    EjectLock,
    ReadSubchannel {
        msf: bool,
        sub_q: bool,
        data_format: u8,
        track_number: u8,
        allocation_length: u8,
    },
    ReadDiscInfo,
    GetEventStatusNotification {
        polled: bool,
        request: u8,
        allocation_length: u16,
    },
}

const ATAPI_TEST_UNIT_READY: u8 = 0x00;
const ATAPI_REQUEST_SENSE: u8 = 0x03;
const ATAPI_START_STOP_UNIT: u8 = 0x1b;
const ATAPI_MECHANISM_STATUS: u8 = 0xbd;
const ATAPI_MODE_SENSE6: u8 = 0x1a;
const ATAPI_MODE_SENSE10: u8 = 0x5a;
const ATAPI_INQUIRY: u8 = 0x12;
const ATAPI_READ_CDROM_CAPACITY: u8 = 0x25;
const ATAPI_READ_CD: u8 = 0xbe;
const ATAPI_READ_TOC: u8 = 0x43;
const ATAPI_READ10: u8 = 0x28;
const ATAPI_READ12: u8 = 0xa8;
const ATAPI_SEEK: u8 = 0x2b;
const ATAPI_EJECT_LOCK: u8 = 0x1e;
const ATAPI_READ_SUBCHANNEL: u8 = 0x42;
const ATAPI_READ_DISC_INFO: u8 = 0x51;
const ATAPI_GET_EVENT_STATUS_NOTIFICATION: u8 = 0x4a;

// We can safely return an illegal opcode here, since Bochs also does that.

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
pub enum AtapiError {
    IllegalOpcode,
    LogicalBlockOor,
    InvalidFieldInCmdPacket,
    MediumMayHaveChanged,
    SavingParametersNotSupported,
    MediumNotPresent,
}

impl AtapiPacket {
    pub fn parse(bytes: &[u8; 12]) -> Result<AtapiPacket, AtapiError> {
        Ok(match bytes[0] {
            ATAPI_TEST_UNIT_READY => Self::TestUnitReady {
                logical_unit_number: u3::new(bytes[1] >> 5),
            },
            ATAPI_REQUEST_SENSE => Self::RequestSense {
                logical_unit_number: u3::new(bytes[1] >> 5),
                allocation_length: bytes[4],
            },
            ATAPI_START_STOP_UNIT => Self::StartStopUnit {
                command: match bytes[4] & 3 {
                    0b00 => StartStopCommand::Stop,
                    0b01 => StartStopCommand::Start,
                    0b10 => StartStopCommand::Eject,
                    0b11 => StartStopCommand::Load,
                    _ => unreachable!(),
                },
            },
            ATAPI_MECHANISM_STATUS => Self::MechanismStatus,
            ATAPI_MODE_SENSE6 | ATAPI_MODE_SENSE10 => Self::ModeSense {
                logical_unit_number: u3::new(bytes[1] >> 5),
                pc: u2::new(bytes[2] >> 6),
                page_code: u6::new(bytes[2] & 0x3f),
                allocation_length: if bytes[0] == ATAPI_MODE_SENSE6 {
                    bytes[4] as u16
                } else {
                    u16::from_be_bytes(bytes[7..9].try_into().unwrap())
                },
                flag: bytes[5] & 0b10 != 0,
                link: bytes[5] & 0b01 != 0,
            },
            ATAPI_INQUIRY => Self::Inquiry {
                logical_unit_number: u3::new(bytes[1] >> 5),
                allocation_length: bytes[4],
                flag: bytes[5] & 0b10 != 0,
                link: bytes[5] & 0b01 != 0,
            },
            ATAPI_READ_CDROM_CAPACITY => Self::ReadCdromCapacity,
            ATAPI_READ_CD => Self::ReadCd,
            ATAPI_READ_TOC => Self::ReadToc {
                msf: (bytes[1] & 0b10) != 0,
                starting_track: bytes[6],
                logical_unit_number: u3::new(bytes[1] >> 5),
                allocation_length: u16::from_be_bytes(bytes[7..9].try_into().unwrap()),
                track_number: bytes[5],
                flag: bytes[9] & 0b10 != 0,
                link: bytes[9] & 0b01 != 0,
                format: (bytes[9] >> 6),
            },
            ATAPI_READ10 | ATAPI_READ12 => Self::Read {
                lba: u32::from_be_bytes(bytes[2..6].try_into().unwrap()),
                transfer_length: if bytes[0] == ATAPI_READ10 {
                    u16::from_be_bytes(bytes[7..9].try_into().unwrap()) as u32
                } else {
                    u32::from_be_bytes(bytes[6..10].try_into().unwrap())
                },
            },
            ATAPI_SEEK => Self::Seek,
            ATAPI_EJECT_LOCK => Self::EjectLock,
            ATAPI_READ_SUBCHANNEL => Self::ReadSubchannel {
                msf: (bytes[1] & 0b10) != 0,
                sub_q: (bytes[2] & (1 << 6)) != 0,
                data_format: bytes[3],
                track_number: bytes[6],
                allocation_length: bytes[7],
            },
            ATAPI_READ_DISC_INFO => Self::ReadDiscInfo,
            ATAPI_GET_EVENT_STATUS_NOTIFICATION => Self::GetEventStatusNotification {
                allocation_length: u16::from_be_bytes(bytes[7..9].try_into().unwrap()),
                polled: (bytes[1] & 0b1) != 0,
                request: bytes[4],
            },
            _ => return Err(AtapiError::IllegalOpcode),
        })
    }
}

pub fn compute_simple_toc(msf: bool, format: u8, start_track: u8, capacity: u32) -> Result<Vec<u8>, &'static str> {
    let mut buf = vec![0u8; 4];
    match format {
        0 => {
            // From atapi specs : start track can be 0-63, AA
            if (start_track > 1) && (start_track != 0xaa) {
                return Err("invalid start track")
            }

            buf[2] = 1;
            buf[3] = 1;

            if start_track <= 1 {
                buf.push(0); // Reserved
                buf.push(0x14); // ADR, control
                buf.push(1); // Track number
                buf.push(0); // Reserved

                // Start address
                if msf {
                    buf.push(0); // reserved
                    buf.push(0); // minute
                    buf.push(2); // second
                    buf.push(0); // frame
                } else {
                    buf.push(0);
                    buf.push(0);
                    buf.push(0);
                    buf.push(0); // logical sector 0
                }
            }

            // Lead out track
            buf.push(0); // Reserved
            buf.push(0x16); // ADR, control
            buf.push(0xaa); // Track number
            buf.push(0); // Reserved

            let blocks = capacity;

            // Start address
            if msf {
                buf.push(0); // reserved
                buf.push((((blocks + 150) / 75) / 60) as u8); // minute
                buf.push((((blocks + 150) / 75) % 60) as u8); // second
                buf.push(((blocks + 150) % 75) as u8); // frame
            } else {
                buf.push((blocks >> 24) as u8);
                buf.push((blocks >> 16) as u8);
                buf.push((blocks >> 8) as u8);
                buf.push(blocks as u8);
            }
            buf[0] = ((buf.len() - 2) >> 8) as u8;
            buf[1] = (buf.len() - 2) as u8;
        },

        1 => {
            // multi session stuff - emulate a single session only
            buf[0] = 0;
            buf[1] = 0x0a;
            buf[2] = 1;
            buf[3] = 1;

            buf.resize(12, 0);
        },

        2 => {
            // raw toc - emulate a single session only (ported from qemu)
            buf[2] = 1;
            buf[3] = 1;

            for i in 0..4 {
                buf.push(1);
                buf.push(0x14);
                buf.push(0);
                if i < 3 {
                    buf.push(0xa0 + i);
                } else {
                    buf.push(1);
                }
                buf.push(0);
                buf.push(0);
                buf.push(0);
                if i < 2 {
                    buf.push(0);
                    buf.push(1);
                    buf.push(0);
                    buf.push(0);
                } else if i == 2 {
                    let blocks = capacity;
                    if msf {
                        buf.push(0); // reserved
                        buf.push((((blocks + 150) / 75) / 60) as u8); // minute
                        buf.push((((blocks + 150) / 75) % 60) as u8); // second
                        buf.push(((blocks + 150) % 75) as u8); // frame
                    } else {
                        buf.push((blocks >> 24) as u8);
                        buf.push((blocks >> 16) as u8);
                        buf.push((blocks >> 8) as u8);
                        buf.push(blocks as u8);
                    }
                } else {
                    buf.push(0);
                    buf.push(0);
                    buf.push(0);
                    buf.push(0);
                }
            }

            buf[0] = ((buf.len() - 2) >> 8) as u8;
            buf[1] = (buf.len() - 2) as u8;
        },
        _ => {
            panic!("unknown toc format {format}");
        },
    }

    Ok(buf)
}
