use bitcode::{Decode, Encode};
use log::error;
use serde::{Deserialize, Serialize};

use crate::hw::pci::{CommonPciHeader, PciCommandRegister, PciDevice};

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct PciHostBridge {}

impl Default for PciHostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl PciHostBridge {
    pub fn new() -> Self {
        Self {}
    }

    pub fn snapshot(&self) -> PciHostBridge {
        self.clone()
    }

    pub fn restore(&mut self, pci_host_bridge: PciHostBridge) {
        *self = pci_host_bridge;
    }
}

impl PciDevice for PciHostBridge {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        match index {
            0x16 | 0x17 => {
                // These registers determine for eight 0x8000-byte areas between 0xc0000 and 0x100000 whether they should be mapped to physical memory or BIOS ROM.
                // Each byte describes one area.
                // Operations for reading and writing can be mapped separately.
                error!("TODO: Potentially unmap BIOS from memory: 0x{val:X}");
            },
            _ => error!("TODO: Write PCI bus configuration space @ 0x{index:X} = 0x{val:X}"),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        let hdr = CommonPciHeader {
            vendor_id: 0x8086,
            device_id: 0x1237,
            command: PciCommandRegister::from(6),
            status: 0x280,
            revision_id: 0,
            prog_if: 0,
            subclass: 0,
            class_code: 0x06,
            cache_line_size: 0,
            latency_timer: 0,
            header_type: 0,
            bist: 0,
        };

        let slice: &[u32; 4] = bytemuck::cast_ref(&hdr);
        let result = *slice.get(index).unwrap_or(&0);
        error!("TODO: Read PCI bus configuration space @ 0x{index:X} = 0x{result:X}");
        result
    }
}
