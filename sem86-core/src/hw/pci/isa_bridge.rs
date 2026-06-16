use bitcode::{Decode, Encode};
use log::{error, info};
use serde::{Deserialize, Serialize};

use crate::hw::pci::{CLASS_BRIDGE, CommonPciHeader, PciCommandRegister, PciDevice, SUBCLASS_ISA_BRIDGE};

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct PciToIsaBridge {
    pirq_route_control: u32,
    serr_enable: bool,
}

impl Default for PciToIsaBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl PciToIsaBridge {
    pub fn new() -> Self {
        Self {
            pirq_route_control: 0x80808080,
            serr_enable: true,
        }
    }

    pub fn snapshot(&self) -> PciToIsaBridge {
        self.clone()
    }

    pub fn restore(&mut self, isa_bridge: PciToIsaBridge) {
        *self = isa_bridge;
    }
}

const REG_PIRQ_ROUTE_CONTROL: usize = 0x18;
const REG_SERIRQ_CONTROL: usize = 0x19;

impl PciDevice for PciToIsaBridge {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        match index {
            REG_PIRQ_ROUTE_CONTROL => {
                info!("PIRQ Route Control = 0x{val:08X}");
                self.pirq_route_control = val
            },
            _ => error!("TODO: Write PCI-to-ISA bridge configuration space @ 0x{index:X} = 0x{val:X}"),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        CommonPciHeader {
            vendor_id: 0x8086,
            device_id: 0x7000,
            command: {
                let mut c = PciCommandRegister::from(0);
                c.set_serr_enable(self.serr_enable);
                c
            },
            status: 0x200,
            revision_id: 0,
            prog_if: 0,
            subclass: SUBCLASS_ISA_BRIDGE,
            class_code: CLASS_BRIDGE,
            cache_line_size: 0,
            latency_timer: 0,
            header_type: 0x80, // multifunction
            bist: 0,
        }
        .read(index)
        .unwrap_or_else(|| match index {
            // Reserved on PIIX4
            0x04..0x13 => 0,
            0x13 => {
                error!("TODO: Read ISA I/O Recovery Timer + X-Bus Chip Select");
                0x0003004D
            },
            REG_PIRQ_ROUTE_CONTROL => {
                // PIRQ[A:D] route control
                self.pirq_route_control
            },
            REG_SERIRQ_CONTROL => {
                error!("TODO: Read ISA Serial IRQ control");
                0x10
            },
            0x1A => {
                error!("TODO: Read Top-Of-Memory register");
                0x02
            },
            _ => {
                error!("TODO: Read PCI-to-ISA bridge register #0x{index:X}");
                0
            },
        })
    }
}
