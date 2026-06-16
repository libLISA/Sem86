use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::net::ne2k::Ne2kSnapshot;
use crate::hw::acpi::Acpi;
use crate::hw::cmos::CmosSnapshot;
use crate::hw::dma::DmaSnapshot;
use crate::hw::fdc::FdcSnapshot;
use crate::hw::ide::IdeSnapshot;
use crate::hw::logger::Logger;
use crate::hw::pci::PciBusSnapshot;
use crate::hw::pci::host_bridge::PciHostBridge;
use crate::hw::pci::isa_bridge::PciToIsaBridge;
use crate::hw::pic::io::IoApicSnapshot;
use crate::hw::pic::legacy::{PicSnapshot, SharedPicCoreSnapshot};
use crate::hw::pic::local::LocalApicSnapshot;
use crate::hw::pit::PitSnapshot;
use crate::hw::ppi::PpiSnapshot;
use crate::hw::sound::es1370::Es1370Snapshot;
use crate::hw::uart::UartSnapshot;
use crate::hw::vga::VgaSnapshot;
use crate::hw::xtide::XtIde;
use crate::time::EmulatorClockSnapshot;

#[derive(Clone, Serialize, Deserialize)]
pub struct HwSnapshot {
    pub(super) ppi: PpiSnapshot,
    pub(super) ioapic: IoApicSnapshot,
    pub(super) lapic: LocalApicSnapshot,
    pub(super) vga: VgaSnapshot,
    pub(super) fdc: FdcSnapshot,
    pub(super) ide: IdeSnapshot,
    pub(super) xtide: XtIde,
    pub(super) com1: UartSnapshot,
    pub(super) core: CoreHwSnapshot,
    pub(super) es1370: Option<Es1370Snapshot>,
    pub(super) vga_logger: Logger,
    pub(super) bios_info_logger: Logger,
    pub(super) bios_debug_logger: Logger,
    pub(super) pci: PciBusSnapshot,
    pub(super) acpi: Acpi,
    pub(super) isa_bridge: PciToIsaBridge,
    pub(super) pci_host_bridge: PciHostBridge,
    pub(super) clock: EmulatorClockSnapshot,
    pub(super) ne2k: Ne2kSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct CoreHwSnapshot {
    pub(super) pit: PitSnapshot,
    pub(super) cmos: CmosSnapshot,
    pub(super) dma: DmaSnapshot,
    pub(super) primary_pic: PicSnapshot,
    pub(super) secondary_pic: PicSnapshot,
    pub(super) shared_pic_core: SharedPicCoreSnapshot,
}
