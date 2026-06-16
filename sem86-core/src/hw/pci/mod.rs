use std::fmt::Display;
use std::sync::Arc;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{info, warn};
use sem86_arch::mem::{Mem32, Shm};
use serde::{Deserialize, Serialize};

use super::{PortError, PortIoData};
use crate::hw::acpi::Acpi;
use crate::hw::ide::Ide;
use crate::hw::net::ne2k::Ne2k;
use crate::hw::pci::host_bridge::PciHostBridge;
use crate::hw::pci::isa_bridge::PciToIsaBridge;
use crate::hw::sound::es1370::Es1370;
use crate::hw::vga::Vga;

pub mod header;
pub mod host_bridge;
pub mod isa_bridge;

#[bitsize(16)]
#[derive(
    Copy, Clone, DebugBits, FromBits, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, Serialize, Deserialize, Encode, Decode,
)]
#[repr(transparent)]
pub struct PciCommandRegister {
    pub enable_io_space: bool,
    pub enable_memory_space: bool,
    pub enable_bus_master: bool,
    pub special_cycles: bool,
    pub enable_memory_write_and_invalidate: bool,
    pub enable_vga_palette_snoop: bool,
    pub enable_parity_error_response: bool,
    reserved: bool,
    pub serr_enable: bool,
    pub fast_back_to_back_enable: bool,
    pub interrupt_disable: bool,
    reserved: [bool; 5],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, Serialize, Deserialize, Encode, Decode)]
#[repr(C)]
pub struct CommonPciHeader {
    pub vendor_id: u16,
    pub device_id: u16,
    pub command: PciCommandRegister,
    pub status: u16,
    pub revision_id: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class_code: u8,
    pub cache_line_size: u8,
    pub latency_timer: u8,
    pub header_type: u8,
    pub bist: u8,
}

#[derive(Debug)]
pub enum CommonWriteEvent {
    CommandStatus,
    Bist,
    ProgIf,
}

impl CommonPciHeader {
    const NUM_REGISTERS: usize = size_of::<Self>() / 4;

    pub fn read(&self, index: usize) -> Option<u32> {
        let slice: &[u32; 4] = bytemuck::cast_ref(self);
        slice.get(index).cloned()
    }

    pub fn write(&mut self, index: usize, value: u32) -> Option<CommonWriteEvent> {
        if index < Self::NUM_REGISTERS {
            match index {
                0x1 => {
                    self.command = PciCommandRegister::from(value as u16);
                    self.status = (value >> 16) as u16;
                    Some(CommonWriteEvent::CommandStatus)
                },
                0x2 => {
                    self.prog_if = (value >> 8) as u8;
                    Some(CommonWriteEvent::ProgIf)
                },
                0x3 => {
                    self.bist = (value >> 24) as u8;
                    Some(CommonWriteEvent::Bist)
                },
                _ => None,
            }
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, bytemuck::NoUninit, Serialize, Deserialize, Encode, Decode)]
#[repr(C)]
pub struct GeneralDeviceHeader {
    pub common: CommonPciHeader,
    pub bar: [u32; 6],
    pub cardbus_cis_pointer: u32,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub expansion_rom_base_address: u32,
    pub capabilities_pointer: u8,
    pub reserved1: [u8; 3],
    pub reserved2: u32,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub min_grant: u8,
    pub max_latency: u8,
}

#[derive(Debug)]
pub enum DeviceWriteEvent {
    Common(CommonWriteEvent),
    Bar(usize),
    CardbusCisPointer,
    ExpansionRom,
    CapabilitiesPointer,
    Reserved2,
    InterruptConfig,
}

impl GeneralDeviceHeader {
    pub fn read(&self, index: usize) -> Option<u32> {
        let slice: &[u32; 16] = bytemuck::cast_ref(self);
        slice.get(index).cloned()
    }

    pub fn write(&mut self, index: usize, value: u32) -> Option<DeviceWriteEvent> {
        if index < CommonPciHeader::NUM_REGISTERS {
            self.common.write(index, value).map(DeviceWriteEvent::Common)
        } else {
            match index {
                0x4..=0x9 => {
                    let bar_num = index - 4;
                    self.bar[bar_num] = value;
                    Some(DeviceWriteEvent::Bar(bar_num))
                },
                0xA => {
                    self.cardbus_cis_pointer = value;
                    Some(DeviceWriteEvent::CardbusCisPointer)
                },
                0xC => {
                    self.expansion_rom_base_address = value;
                    Some(DeviceWriteEvent::ExpansionRom)
                },
                0xD => {
                    self.capabilities_pointer = value as u8;
                    Some(DeviceWriteEvent::CapabilitiesPointer)
                },
                0xE => {
                    self.reserved2 = value;
                    None
                },
                0xF => {
                    self.interrupt_line = value as u8;
                    self.min_grant = (value >> 16) as u8;
                    self.max_latency = (value >> 24) as u8;
                    Some(DeviceWriteEvent::InterruptConfig)
                },
                _ => None,
            }
        }
    }
}

pub struct Space<'a> {
    pub ide: &'a mut Ide,
    pub isa_bridge: &'a mut PciToIsaBridge,
    pub host_bridge: &'a mut PciHostBridge,
    pub acpi: &'a mut Acpi,
    pub vga: &'a mut Vga,
    pub es1370: Option<&'a mut Es1370>,
    pub ne2k: &'a mut Ne2k,
}

impl PciSpace for Space<'_> {
    fn get_device(&mut self, bus: u8, device: u8, function: u8) -> Option<impl PciDevice> {
        match (bus, device, function) {
            (0, 0, 0) => Some(Box::new(PciDeviceMut(self.host_bridge)) as Box<dyn PciDevice>),
            (0, 1, 0) => Some(Box::new(PciDeviceMut(self.isa_bridge)) as Box<dyn PciDevice>),
            (0, 1, 1) => Some(Box::new(PciDeviceMut(self.ide)) as Box<dyn PciDevice>),
            (0, 1, 3) => Some(Box::new(PciDeviceMut(self.acpi)) as Box<dyn PciDevice>),
            (0, 2, 0) => Some(Box::new(PciDeviceMut(self.vga)) as Box<dyn PciDevice>),
            (0, 3, 0) => self
                .es1370
                .as_mut()
                .map(|es1370| Box::new(PciDeviceMut(*es1370)) as Box<dyn PciDevice>),
            (0, 4, 0) => Some(Box::new(PciDeviceMut(self.ne2k)) as Box<dyn PciDevice>),
            _ => None,
        }
    }
}

pub struct PciDeviceMut<'a, T>(&'a mut T);

const CLASS_BRIDGE: u8 = 0x6;
const SUBCLASS_ISA_BRIDGE: u8 = 0x1;

pub trait PciSpace {
    fn get_device(&mut self, bus: u8, device: u8, function: u8) -> Option<impl PciDevice>;
}

impl PciSpace for () {
    fn get_device(&mut self, bus: u8, device: u8, function: u8) -> Option<impl PciDevice> {
        warn!("Missing PCI device: {bus:X}:{device:X}:{function:X}");

        Option::<()>::None
    }
}

pub trait PciDevice {
    fn write_configuration_space(&mut self, index: usize, val: u32);
    fn read_configuration_space(&mut self, index: usize) -> u32;
}

impl PciDevice for () {
    fn write_configuration_space(&mut self, _index: usize, _val: u32) {
        todo!()
    }

    fn read_configuration_space(&mut self, _index: usize) -> u32 {
        todo!()
    }
}

impl<D: PciDevice + ?Sized> PciDevice for Box<D> {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        self.as_mut().write_configuration_space(index, val);
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        self.as_mut().read_configuration_space(index)
    }
}

impl<D: PciDevice> PciDevice for PciDeviceMut<'_, D> {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        self.0.write_configuration_space(index, val);
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        self.0.read_configuration_space(index)
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits, Default, Serialize, Deserialize, Encode, Decode)]
struct Address {
    register_offset: u8,
    function_number: u3,
    device_number: u5,
    bus_number: u8,
    reserved: u7,
    enable: bool,
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02X}:{:02X}:{:02X}@{:02X}",
            self.bus_number(),
            self.device_number(),
            self.function_number(),
            self.register_offset()
        )
    }
}

#[derive(Clone, Debug)]
pub struct PciBus {
    address: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct PciBusSnapshot {
    address: Address,
}

impl Default for PciBus {
    fn default() -> Self {
        Self::new()
    }
}

impl PciBus {
    pub fn new() -> Self {
        Self {
            address: Address::default(),
        }
    }

    pub fn write_address(&mut self, addr: u32) {
        self.address = Address::from(addr);
    }

    pub fn read_address(&self) -> u32 {
        self.address.into()
    }

    pub fn write_data<S: PortIoData>(&mut self, pci: &mut impl PciSpace, addr: u16, data: S) {
        info!("PCI write to {} (+{addr} offset): 0x{data:X}", self.address);
        if let Some(mut device) = pci.get_device(
            self.address.bus_number(),
            self.address.device_number().as_u8(),
            self.address.function_number().as_u8(),
        ) {
            let index = self.address.register_offset() as usize / 4;
            let new_val = data.blend_into_u32(addr, || device.read_configuration_space(index));
            device.write_configuration_space(index, new_val);
        }
    }

    pub fn read_data<S: PortIoData>(&mut self, pci: &mut impl PciSpace, addr: u16) -> Result<S, PortError> {
        S::from_u32(addr & 3, || {
            let device = pci.get_device(
                self.address.bus_number(),
                self.address.device_number().as_u8(),
                self.address.function_number().as_u8(),
            );

            let val = if let Some(mut device) = device {
                device.read_configuration_space(self.address.register_offset() as usize / 4)
            } else {
                0xffff_ffff
            };

            info!("Read PCI configuration space at {} = 0x{val:X}", self.address);

            val
        })
    }

    pub fn snapshot(&self) -> PciBusSnapshot {
        PciBusSnapshot {
            address: self.address,
        }
    }

    pub fn restore(&mut self, pci: PciBusSnapshot) {
        self.address = pci.address;
    }
}

#[derive(Clone, Debug)]
pub struct Bar {
    current_addr: Option<u32>,
    shm: Arc<Shm>,
    writable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct BarSnapshot {
    current_addr: Option<u32>,
}

impl Bar {
    pub fn new(shm: Arc<Shm>, writable: bool) -> Self {
        Self {
            current_addr: None,
            shm,
            writable,
        }
    }

    pub fn len_rounded_up_to_page_bound(&self) -> u64 {
        if self.shm.len().is_multiple_of(4096) {
            self.shm.len()
        } else {
            (self.shm.len() + 0xfff) & !0xfff
        }
    }

    pub fn enable_and_set_addr(&mut self, addr: u32, mem: &Mem32) {
        if self.current_addr.map(|old_addr| old_addr != addr).unwrap_or(true) {
            self.disable(mem);
            mem.map_physical_memory_to_shm(
                addr as u64..addr as u64 + self.len_rounded_up_to_page_bound(),
                self.shm.clone(),
                None,
                0,
                self.writable,
            );
            self.current_addr = Some(addr);
        }
    }

    pub fn disable(&mut self, mem: &Mem32) {
        if let Some(old_addr) = self.current_addr {
            mem.map_physical_memory_to_default(old_addr as u64..old_addr as u64 + self.len_rounded_up_to_page_bound());
        }
    }

    pub fn snapshot(&self) -> BarSnapshot {
        BarSnapshot {
            current_addr: self.current_addr,
        }
    }

    pub fn restore(&mut self, snapshot: BarSnapshot, mem: &Mem32) {
        match snapshot.current_addr {
            Some(addr) => self.enable_and_set_addr(addr, mem),
            None => self.disable(mem),
        }
    }
}
