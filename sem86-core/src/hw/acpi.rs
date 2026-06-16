use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{error, info, trace};
use serde::{Deserialize, Serialize};

use super::ports::{PortError, PortIoData};
use crate::hw::pci::{CommonPciHeader, PciCommandRegister, PciDevice};
use crate::hw::ports::WithIoSpace;
use crate::time::EmulatorClock;

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum EventType {
    Smi,
    Sci,
}

#[bitsize(3)]
#[derive(Copy, Clone, Debug, FromBits)]
enum SleepType {
    SoftPowerOff,
    S1,
    S2,
    S3,
    S4,
    S5,
    S6,
    S7,
}

#[bitsize(16)]
#[derive(Copy, Clone, DebugBits, FromBits)]
struct Pm1aCntBlk {
    /// SCI_EN
    event_type: EventType,

    /// BM_RLD
    wake_c3_processors_on_busmaster_request: bool,

    /// GBL_RLS
    gbl_rls: bool,
    reserved: u6,
    reserved: u1,

    sleep_type: SleepType,

    /// SLP_EN. Write only. When set, the CPU enters the sleep type specified in `sleep_type`.
    enter_specified_sleep_type: bool,
    reserved: u2,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Acpi {
    pm_base: u16,
    _sm_base: u16,
    intinfo: u32,
}

impl Default for Acpi {
    fn default() -> Self {
        Self::new()
    }
}

impl Acpi {
    pub fn new() -> Self {
        Self {
            // TODO: Proper values
            pm_base: 0xB000,
            _sm_base: 0xFFFF,
            intinfo: 0x01_00,
        }
    }

    fn read<S: PortIoData>(&self, addr: u16, time: &EmulatorClock) -> Result<S, PortError> {
        trace!("TODO: ACPI read at {addr:X}");
        match addr {
            // Power Management status?
            0 => S::from_u16(addr & 1, || 0),
            // PMEN?
            2 => S::from_u16(addr & 1, || 0),
            // 24-bit Power Management Timer
            8 => {
                // Windows XP is very picky in terms of what timings it accepts from this timer in relation to the LAPIC timer (and possibly the PIT/CMOS?)
                let pmtmr = time.get_ticks_in_hz(3_579_545) & 0xff_ffff;
                info!("Reading power management timer = 0x{:X}", pmtmr);
                S::from_u32(addr & 3, || pmtmr as u32)
            },
            15 => S::from_u32(addr & 3, || self.intinfo),
            // TODO: Other PM registers
            _ => S::from_u32(0u8, || u32::MAX),
        }
    }

    fn write<S: PortIoData>(&mut self, addr: u16, raw_val: S) -> Result<(), PortError> {
        match addr {
            0x4 => {
                let val = Pm1aCntBlk::from(raw_val.u16());
                println!("PM11_CNT_BLK = 0x{raw_val:X?} = {val:?}");

                if val.enter_specified_sleep_type() {
                    match val.sleep_type() {
                        SleepType::SoftPowerOff => panic!("TODO: Power off"),
                        other => todo!("unknown sleep type: {other:?}"),
                    }
                }
            },
            0x3C => self.intinfo = raw_val.u32(),
            _ => {
                error!("TODO: ACPI write at 0x{addr:X} = 0x{raw_val:X}");
            },
        }

        Ok(())
    }

    pub fn snapshot(&self) -> Acpi {
        self.clone()
    }

    pub fn restore(&mut self, acpi: Acpi) {
        *self = acpi;
    }
}

impl WithIoSpace for Acpi {
    fn try_read<S: PortIoData>(&mut self, addr: u16, mmio: &mut super::HwMmio) -> Option<Result<S, PortError>> {
        if addr & 0xffc0 == self.pm_base {
            Some(self.read(addr & 0x3f, mmio.clock))
        } else {
            None
        }
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, _mmio: &mut super::HwMmio) -> Option<Result<(), PortError>> {
        if addr & 0xffc0 == self.pm_base {
            Some(self.write(addr & 0x3f, val))
        } else {
            None
        }
    }
}

impl PciDevice for Acpi {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        match index {
            0xF => self.intinfo = val,
            _ => error!("TODO: Write PCI ACPI configuration space @ 0x{index:X} = 0x{val:X}"),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        CommonPciHeader {
            vendor_id: 0x8086,
            device_id: 0x7113,
            command: PciCommandRegister::from(0),
            status: 0,
            revision_id: 3,
            prog_if: 0,
            subclass: 0,
            class_code: 0,
            cache_line_size: 0,
            latency_timer: 0,
            header_type: 0,
            bist: 0,
        }
        .read(index)
        .unwrap_or_else(|| match index {
            0xF => self.intinfo,
            _ => {
                error!("TODO: Read ACPI register #0x{index:X}");
                0
            },
        })
    }
}
