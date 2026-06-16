#![allow(unused)]

use std::sync::Arc;

use bilge::prelude::*;
use log::{error, info};
use sem86_arch::mem::Mem32;

use crate::hw::pci::{CommonPciHeader, CommonWriteEvent, DeviceWriteEvent, GeneralDeviceHeader, PciCommandRegister, PciDevice};
use crate::hw::ports::{PortError, PortIoData, WithIoSpace};

// TODO: This seems to be generic for all pci devices
#[bitsize(8)]
struct Command {
    io_space_enable: bool,
    memory_space_enable: bool, // Unused
    bus_master_enable: bool,
    other: [bool; 5],
}

#[derive(Clone, Debug)]
pub struct Ac97Core {}

impl Ac97Core {
    pub fn read(&self, index: u16) -> u16 {
        error!("TODO: Read AC97 I/O space register 0x{index:X}");
        0
    }

    pub fn write(&mut self, index: u16, data: u16) {
        match index {
            0 => {
                error!("TODO: Reset AC97");
            },
            _ => error!("TODO: Write AC97 I/O space register 0x{index:X} = 0x{data:X} in BAR0"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Ac97 {
    pci_header: GeneralDeviceHeader,
    bar0_address: Option<u32>,
    core: Ac97Core,
    mem: Arc<Mem32>,
}

impl Ac97 {
    pub fn new(mem: Arc<Mem32>) -> Self {
        Self {
            pci_header: GeneralDeviceHeader {
                common: CommonPciHeader {
                    vendor_id: 0x8086,
                    device_id: 0x2415,
                    command: PciCommandRegister::from(0),
                    status: 0x0280,
                    revision_id: 1,
                    prog_if: 0,
                    class_code: 0x04,
                    subclass: 0x01,
                    bist: 0,
                    cache_line_size: 0x08,
                    latency_timer: 0x20,
                    header_type: 0,
                },
                bar: [
                    0x1, // I/O space
                    0x1, // I/O space
                    0x0, 0x0, 0x0, 0x0,
                ],
                cardbus_cis_pointer: 0,
                subsystem_vendor_id: 0x8086,
                subsystem_id: 0,
                expansion_rom_base_address: 0,
                capabilities_pointer: 0,
                reserved1: [0; _],
                reserved2: 0,
                interrupt_line: 0xff,
                interrupt_pin: 1,
                min_grant: 0,
                max_latency: 0,
            },
            bar0_address: None,
            mem,
            core: Ac97Core {},
        }
    }

    pub fn core(&mut self) -> &mut Ac97Core {
        &mut self.core
    }
}

impl PciDevice for Ac97 {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        error!("Write AC97 register 0x{index:X} = 0x{val:X}");
        match self.pci_header.write(index, val) {
            Some(DeviceWriteEvent::Common(CommonWriteEvent::CommandStatus)) => {
                info!(
                    "Command/status written: command={:X?}, status={:X?}",
                    self.pci_header.common.command, self.pci_header.common.status
                );
            },
            Some(DeviceWriteEvent::Bar(0)) => {
                self.pci_header.bar[0] = (self.pci_header.bar[0] & !0xff) | 1;
                info!("BAR0 = {:X}", self.pci_header.bar[0]);
            },
            Some(DeviceWriteEvent::Bar(1)) => {
                self.pci_header.bar[1] = (self.pci_header.bar[1] & !0xff) | 1;
                info!("BAR1 = {:X}", self.pci_header.bar[1]);
            },
            // No other BARs
            Some(DeviceWriteEvent::Bar(n)) => self.pci_header.bar[n] = 0,
            Some(DeviceWriteEvent::ExpansionRom) => self.pci_header.expansion_rom_base_address = 0,
            Some(ev) => {
                error!("TODO: Handle write event: {ev:X?}");
            },
            None => (),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        let result = self
            .pci_header
            .read(index)
            .expect("TODO: read outside ac97 generic pci header");

        info!("Read AC97 register 0x{index:X} = 0x{result:X}");

        result
    }
}

impl WithIoSpace for Ac97 {
    fn try_read<S: PortIoData>(&mut self, addr: u16, _mmio: &mut crate::hw::HwMmio) -> Option<Result<S, PortError>> {
        if !self.pci_header.common.command.enable_io_space() {
            return None
        }

        if addr & !0xff == (self.pci_header.bar[0] & !0xff) as u16 {
            Some(S::from_u16(addr & 1, || self.core.read(addr / 2)))
        } else {
            None
        }
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, _mmio: &mut crate::hw::HwMmio) -> Option<Result<(), PortError>> {
        if !self.pci_header.common.command.enable_io_space() {
            return None
        }

        if addr & !0xff == (self.pci_header.bar[0] & !0xff) as u16 {
            self.core.write(addr / 2, val.u16());
            Some(Ok(()))
        } else {
            None
        }
    }
}
