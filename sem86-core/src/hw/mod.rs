use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32};
use std::sync::mpsc::{Receiver, Sender};

use cmos::Cmos;
use dma::Dma;
use fdc::Fdc;
use ide::Ide;
use liblisa::arch::CpuState;
use log::{error, info, warn};
use logger::Logger;
use mouse::UartMouse;
use pci::{PciBus, Space};
use pit::Pit;
use ppi::Ppi;
use sem86_arch::addr::PhysFrameIndex;
use sem86_arch::mem::{Mem32, MemoryData, Mmio, MmioId, Shm};
use uart::Uart;
use vga::{ModeSet, Vga};
use xtide::XtIde;

use crate::arch::intel386::{GpReg, State};
use crate::hw::acpi::Acpi;
use crate::hw::intr::IntrHandle;
use crate::hw::net::ne2k::Ne2k;
use crate::hw::pci::host_bridge::PciHostBridge;
use crate::hw::pci::isa_bridge::PciToIsaBridge;
use crate::hw::pic::io::IoApic;
use crate::hw::pic::legacy::{DynamicIrqLine, Pic, SharedPicCore};
use crate::hw::pic::local::LocalApic;
use crate::hw::pic::{ApicIrqLine, DualDynamicIrqLine, DualIrqLine, DynamicApicIrqLine};
use crate::hw::ports::{PortError, PortIoData};
use crate::hw::snapshot::{CoreHwSnapshot, HwSnapshot};
use crate::hw::sound::es1370::Es1370;
use crate::hw::storage::{DiskData, MemoryDiskData};
use crate::hw::vga::VideoMemory;
use crate::icache::InstructionCache;
use crate::time::{EmulatorClock, PeriodicIntr};
use crate::{port_read_chain, port_write_chain};

pub mod acpi;
pub mod bank;
pub mod bcd;
pub mod cmos;
pub mod dma;
pub mod fdc;
pub mod ide;
pub mod intr;
pub mod logger;
pub mod logibm;
pub mod mouse;
pub mod net;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod ports;
pub mod ppi;
pub mod reg;
pub mod snapshot;
pub mod sound;
pub mod storage;
pub mod uart;
pub mod vga;
pub mod xtide;

pub const MMIO_ID_VGA: MmioId = MmioId::new(0);
pub const MMIO_ID_LAPIC: MmioId = MmioId::new(1);
pub const MMIO_ID_IOAPIC: MmioId = MmioId::new(2);

#[derive(Debug)]
pub enum Ev {
    ScanCode(u8),
    MouseMove(MouseMove),
    InsertFdd(PathBuf),
    ClearCaches,
    Stop,
}

#[derive(Copy, Clone, Debug)]
pub struct MouseMove {
    pub left_pressed: bool,
    pub right_pressed: bool,
    pub x: f64,
    pub y: f64,
    pub z: f32,
}

#[derive(Debug)]
pub struct Hw {
    mem: Arc<Mem32>,
    ppi: Ppi,
    ioapic: IoApic,
    lapic: LocalApic,
    vga: Vga,
    fdc: Fdc,
    ide: Ide,
    xtide: XtIde,
    recv: Receiver<Ev>,
    com1: Uart<UartMouse>,
    core: CoreHw,
    es1370: Option<Es1370>,
    vga_logger: Logger,
    bios_info_logger: Logger,
    bios_debug_logger: Logger,
    pci: PciBus,
    acpi: Acpi,
    isa_bridge: PciToIsaBridge,
    pci_host_bridge: PciHostBridge,
    clock: EmulatorClock,
    ne2k: Ne2k,
    periodic_intr: PeriodicIntr,
}

#[derive(Debug)]
pub struct CoreHw {
    pit: Pit,
    cmos: Cmos,
    dma: Dma,
    primary_pic: Pic,
    secondary_pic: Pic,
    shared_pic_core: Arc<SharedPicCore>,
}

macro_rules! mmio {
    ($self:expr, $phys_cache:expr, $tag:lifetime) => {
        HwMmio::<'_, $tag> {
            vga: &mut $self.vga,
            ioapic: &mut $self.ioapic,
            lapic: &mut $self.lapic,
            phys_cache: $phys_cache,
            clock: &$self.clock,
        }
    };
}

const PPI_IRQ: u8 = 1;
const PPI_AUX_IRQ: u8 = 4;

impl Hw {
    pub fn new(
        memory: Arc<Mem32>, fdc_disks: Vec<DiskData>, cga_mode_sender: Sender<ModeSet>, recv: Receiver<Ev>, vga_bios: Arc<Shm>,
        intr: IntrHandle, clock: EmulatorClock,
    ) -> Self {
        let shared_pic_core = Arc::new(SharedPicCore::new(intr.clone()));
        let primary_pic = Pic::new(0, 0x08, shared_pic_core.clone());
        let secondary_pic = Pic::new(1, 0x70, shared_pic_core.clone());
        let ioapic = IoApic::new(&memory);
        let lapic = LocalApic::new(&memory, intr.clone());

        let dma2_addr = Arc::new(AtomicU32::new(0));
        let dma2_len = Arc::new(AtomicU16::new(0));
        let dma2_mode = Arc::new(AtomicU8::new(0));

        let timer_irq = DualIrqLine {
            pic: primary_pic.get_line(0),
            apic: ApicIrqLine::new(&ioapic, &lapic, 0),
        };

        let ppi_irq = DualIrqLine {
            pic: primary_pic.get_line(PPI_IRQ),
            apic: ApicIrqLine::new(&ioapic, &lapic, 1),
        };

        let ps2_aux_irq = DualIrqLine {
            pic: secondary_pic.get_line(PPI_AUX_IRQ),
            apic: ApicIrqLine::new(&ioapic, &lapic, 0xC),
        };

        let com1_irq = DualIrqLine {
            pic: primary_pic.get_line(4),
            apic: ApicIrqLine::new(&ioapic, &lapic, 4),
        };

        // let busmouse_irq = DualIrqLine {
        //     pic: primary_pic.get_line(5),
        //     apic: ApicIrqLine::new(&ioapic, &lapic, 5),
        // };

        let floppy_irq = DualIrqLine {
            pic: primary_pic.get_line(6),
            apic: ApicIrqLine::new(&ioapic, &lapic, 6),
        };

        let ide_primary_irq = DualIrqLine {
            pic: secondary_pic.get_line(6),
            apic: ApicIrqLine::new(&ioapic, &lapic, 0xE),
        };

        let ide_secondary_irq = DualIrqLine {
            pic: secondary_pic.get_line(7),
            apic: ApicIrqLine::new(&ioapic, &lapic, 0xF),
        };

        let rtc_irq = DualIrqLine {
            pic: secondary_pic.get_line(0),
            apic: ApicIrqLine::new(&ioapic, &lapic, 0x8),
        };

        let dynamic_irq = DynamicApicIrqLine::new(&ioapic, &lapic);
        let dual_dynamic_irq = DualDynamicIrqLine {
            pic: DynamicIrqLine::from_shared_core(shared_pic_core.clone()),
            apic: dynamic_irq.clone(),
        };

        let periodic_intr = PeriodicIntr::new(intr.clone(), &clock);
        Hw {
            periodic_intr,
            mem: memory.clone(),
            fdc: Fdc::new(
                if !fdc_disks.is_empty() {
                    fdc_disks.into_iter().next().unwrap()
                } else {
                    DiskData::new(Box::new(MemoryDiskData::default()))
                },
                floppy_irq,
                dma2_addr.clone(),
                dma2_len.clone(),
                dma2_mode.clone(),
                memory.clone(),
            ),
            ppi: Ppi::new(memory.clone(), ppi_irq, ps2_aux_irq),
            vga: Vga::new(cga_mode_sender, memory.clone(), vga_bios),
            ide: Ide::new(ide_primary_irq.clone(), ide_secondary_irq.clone(), memory.clone()),
            xtide: XtIde::new(),
            com1: Uart::new(UartMouse::new(), com1_irq),
            // logibm: LogiBM::new(busmouse_irq),
            // ac97: Ac97::new(memory.clone()),
            es1370: Some(Es1370::new(memory.clone(), dual_dynamic_irq.clone())),
            recv,
            vga_logger: Logger::new(),
            bios_info_logger: Logger::new(),
            bios_debug_logger: Logger::new(),
            core: CoreHw {
                pit: Pit::new(timer_irq, &clock),
                cmos: Cmos::new(rtc_irq, memory.physical_memory().len()),
                dma: Dma::new(dma2_addr, dma2_len, dma2_mode),
                shared_pic_core,
                primary_pic,
                secondary_pic,
            },
            pci: PciBus::new(),
            ioapic,
            lapic,
            acpi: Acpi::new(),
            isa_bridge: PciToIsaBridge::new(),
            pci_host_bridge: PciHostBridge::new(),
            clock,
            ne2k: Ne2k::new(dual_dynamic_irq.clone()),
        }
    }

    pub fn set_disk(&mut self, channel: usize, disk: usize, data: Option<DiskData>) {
        self.ide.set_disk(channel, disk, data)
    }

    pub fn clear_periodic_intr(&self) {
        self.periodic_intr.clear();
    }

    /// Returns true when emulation should be stopped.
    pub fn update(&mut self, icache: &mut InstructionCache) -> bool {
        while let Ok(ev) = self.recv.try_recv() {
            match ev {
                Ev::ScanCode(n) => self.ppi.enqueue_scancode(n),
                Ev::InsertFdd(path) => {
                    info!("Inserting new FDD: {path:?}");
                    let new_data = std::fs::read(path).expect("should be able to read new FDD image");
                    self.fdc.replace_disk(0, new_data);
                },
                Ev::MouseMove(m) => {
                    self.com1.device_mut().update_position(&m);
                    // self.logibm.update(&m);
                    self.ppi.handle_mouse_input(m);
                },
                Ev::ClearCaches => {
                    icache.clear();
                    self.mem.clean_all_phys_frames();
                    self.mem.invalidate_all_pages();
                },
                Ev::Stop => return true,
            }
        }

        false
    }

    fn read_port<'tag, S: PortIoData>(&mut self, port: u16, phys_cache: &mut InstructionCache<'tag>) -> Result<S, PortError> {
        let val = match port {
            // ======== DMA ========
            // Addresses
            0x00 | 0x02 | 0x04 | 0x06 => S::from_u8(|| self.core.dma.read_address(port as u8 / 2)),
            0xC0 | 0xC2 | 0xC4 | 0xC6 => S::from_u8(|| self.core.dma.read_address((port as u8 & 0xf) / 2 + 4)),

            // Counts
            0x01 | 0x03 | 0x05 | 0x07 => S::from_u8(|| self.core.dma.read_count(port as u8 / 2)),
            0xC1 | 0xC3 | 0xC5 | 0xC7 => S::from_u8(|| self.core.dma.read_count((port as u8 & 0xf) / 2 + 4)),

            // Status
            0x08 => S::from_u8(|| self.core.dma.read_status(0)),
            0xD0 => S::from_u8(|| self.core.dma.read_status(1)),

            // Temporary registers
            0x0D | 0xDA => S::from_u8(|| 0),

            // Page addresses
            0x81 => S::from_u8(|| self.core.dma.read_page_addr_reg(2)),
            0x82 => S::from_u8(|| self.core.dma.read_page_addr_reg(3)),
            0x83 => S::from_u8(|| self.core.dma.read_page_addr_reg(1)),
            0x87 => S::from_u8(|| self.core.dma.read_page_addr_reg(0)),
            0x89 => S::from_u8(|| self.core.dma.read_page_addr_reg(6)),
            0x8A => S::from_u8(|| self.core.dma.read_page_addr_reg(7)),
            0x8B => S::from_u8(|| self.core.dma.read_page_addr_reg(5)),
            0x8F => S::from_u8(|| self.core.dma.read_page_addr_reg(4)),

            // ======== OTHER ========
            0x20 | 0x21 => S::from_u8(|| self.core.primary_pic.read(port as u8 & 1)),
            0x40..=0x42 => S::from_u8(|| self.core.pit.read_timer(port as u8 & 3, &self.clock)),
            0x43 => S::from_u8(|| self.core.pit.read_control()),
            0x60 => S::from_u8(|| self.ppi.read_a()),
            0x61 => S::from_u8(|| self.ppi.read_b()),
            0x62 => S::from_u8(|| self.ppi.read_c()),
            0x64 => S::from_u8(|| self.ppi.read_status()),

            0x71 => S::from_u8(|| self.core.cmos.read(&self.fdc)),
            0x92 => S::from_u8(|| self.ppi.read_system_control_a()),
            0xA0 | 0xA1 => S::from_u8(|| self.core.secondary_pic.read(port as u8 & 1)),
            0x1ce | 0x1cf => S::from_u16(0u8, || self.vga.read_dispi(port as u8 & 1)),

            0x300..=0x30f => S::from_u8(|| self.xtide.read(&mut self.ide, port as u8 & 0xf)),
            0x3c0..=0x3cf => S::from_u8(|| self.vga.read_vga(port as u8 & 0xf)),
            0x3b0..=0x3bf | 0x3d0..=0x3df => S::from_u8(|| self.vga.read_cga(port as u8 & 0xf)),
            0x3f0..=0x3f5 | 0x3f7 => S::from_u8(|| self.fdc.read(port as u8 & 0xf)),

            // COM1
            0x3f8..=0x3ff => S::from_u8(|| self.com1.read_u8(port as u8 & 0x7)),

            // COM2
            0x2f8..=0x2ff => Ok(S::NO_DEVICE),

            // 0x23c..=0x23f => S::from_u8(|| self.logibm.read(port as u8 & 3)),
            0xCF8..=0xCFB => S::from_u32(port & 3, || self.pci.read_address()),
            0xCFC..=0xCFF => self.pci.read_data::<S>(
                &mut Space {
                    ide: &mut self.ide,
                    isa_bridge: &mut self.isa_bridge,
                    vga: &mut self.vga,
                    acpi: &mut self.acpi,
                    es1370: self.es1370.as_mut(),
                    host_bridge: &mut self.pci_host_bridge,
                    ne2k: &mut self.ne2k,
                },
                port & 3,
            ),

            // Polled a lot by FreeDOS
            0x1ef | 0x16F => Ok(S::NO_DEVICE),

            port => {
                let result = port_read_chain! {
                    port, S, &mut mmio!(self, phys_cache, 'tag) => {
                        &mut self.ide,
                        &mut self.acpi,
                        &mut self.es1370.as_mut(),
                        &mut self.ne2k,
                    }
                };

                if let Some(result) = result {
                    if let Ok(val) = result {
                        info!(target: extend_path_with!("port"), "IN: read 0x{val:02X} from port 0x{port:X}");
                    }

                    return result
                } else {
                    error!(target: extend_path_with!("port"), "TODO: Read from port 0x{port:X}");
                    // Set to 0xff to indicate no device present
                    Ok(S::NO_DEVICE)
                }
            },
        }?;

        info!(target: extend_path_with!("port"), "IN: read 0x{val:02X} from port 0x{port:X}");

        Ok(val)
    }

    fn write_port<'tag, S: PortIoData>(
        &mut self, port: u16, val: S, phys_cache: &mut InstructionCache<'tag>,
    ) -> Result<(), PortError> {
        info!(target: extend_path_with!("port"), "OUT: write 0x{val:02X} to port 0x{port:X}");

        assert!(!(0xc100..=0xc300).contains(&port));

        match port {
            // ======== DMA ========
            // Addresses
            0x00 | 0x02 | 0x04 | 0x06 => self.core.dma.write_address(port as u8 / 2, val.require_u8()?),
            0xC0 | 0xC2 | 0xC4 | 0xC6 => self.core.dma.write_address((port as u8 & 0xf) / 2 + 4, val.require_u8()?),

            // Counts
            0x01 | 0x03 | 0x05 | 0x07 => self.core.dma.write_count(port as u8 / 2, val.require_u8()?),
            0xC1 | 0xC3 | 0xC5 | 0xC7 => self.core.dma.write_count((port as u8 & 0xf) / 2 + 4, val.require_u8()?),

            // Commands
            0x08 => self.core.dma.write_command(0, val.require_u8()?),
            0xD0 => self.core.dma.write_command(1, val.require_u8()?),

            // Requests
            0x09 => self.core.dma.write_request(0, val.require_u8()?),
            0xD2 => self.core.dma.write_request(1, val.require_u8()?),

            // Masks
            0x0A => self.core.dma.write_mask(0, val.require_u8()?),
            0xD4 => self.core.dma.write_mask(1, val.require_u8()?),

            // Modes
            0x0B => self.core.dma.write_mode(0, val.require_u8()?),
            0xD6 => self.core.dma.write_mode(1, val.require_u8()?),

            // Clear byte flip/flop
            0x0C => self.core.dma.clear_byte_flip_flop(0),
            0xD8 => self.core.dma.clear_byte_flip_flop(1),

            // Master clear
            0x0D => self.core.dma.master_clear(0),
            0xDA => self.core.dma.master_clear(1),

            // Clear mask register
            0x0E => self.core.dma.clear_mask(0, val.require_u8()?),
            0xDC => self.core.dma.clear_mask(1, val.require_u8()?),

            // Write all mask bits
            0x0F => self.core.dma.write_all_mask_bits(0, val.require_u8()?),
            0xDE => self.core.dma.write_all_mask_bits(1, val.require_u8()?),

            // Page addresses
            0x81 => self.core.dma.write_page_addr_reg(2, val.require_u8()?),
            0x82 => self.core.dma.write_page_addr_reg(3, val.require_u8()?),
            0x83 => self.core.dma.write_page_addr_reg(1, val.require_u8()?),
            0x87 => self.core.dma.write_page_addr_reg(0, val.require_u8()?),
            0x89 => self.core.dma.write_page_addr_reg(6, val.require_u8()?),
            0x8A => self.core.dma.write_page_addr_reg(7, val.require_u8()?),
            0x8B => self.core.dma.write_page_addr_reg(5, val.require_u8()?),
            0x8F => self.core.dma.write_page_addr_reg(4, val.require_u8()?),

            // ======== OTHER ========
            0x20 | 0x21 => self.core.primary_pic.write(port as u8 & 1, val.require_u8()?),
            0x40..=0x42 => self.core.pit.write_timer(port as u8 & 3, val.require_u8()?, &self.clock),
            0x43 => self.core.pit.write_control(val.require_u8()?, &self.clock),
            0x60 => self.ppi.write_a(val.require_u8()?, phys_cache),
            0x61 => self.ppi.write_b(val.require_u8()?),
            0x62 => self.ppi.write_c(val.require_u8()?),
            0x64 => self.ppi.write_command(val.require_u8()?),

            0x70 => self.core.cmos.write_port(val.require_u8()?),
            0x71 => self.core.cmos.write_data(val.require_u8()?, &self.clock),
            0x92 => self.ppi.write_system_control_a(val.require_u8()?, phys_cache),
            0x80 => {
                info!(target: extend_path_with!("port"), "Post progress: 0x{:X}", val.require_u8()?);
            },
            0xA0 | 0xA1 => self.core.secondary_pic.write(port as u8 & 1, val.require_u8()?),
            0x1ce | 0x1cf => self.vga.write_dispi(port as u8 & 1, val.u16(), &self.mem),

            0x300..=0x30f => self.xtide.write(&mut self.ide, port as u8 & 0xf, val.require_u8()?, &mut mmio!(self, phys_cache, 'tag)),
            0x3c0..=0x3cf => self.vga.write_vga(port as u8 & 0xf, val.require_u8()?, &self.mem),
            0x3b0..=0x3bf | 0x3d0..=0x3df => self.vga.write_cga(port as u8 & 0xf, val.require_u8()?),
            0x3f0..=0x3f5 | 0x3f7 => self.fdc.write(port as u8 & 0xf, val.require_u8()?, &mut mmio!(self, phys_cache, 'tag)),
            0x63 => info!(target: extend_path_with!("port"), "TODO: Write to port 0x{port:X}"),

            // COM1
            0x3f8..=0x3ff => self.com1.write_u8(port as u8 & 0x7, val.require_u8()?),
            // COM2
            0x2f8..=0x2ff => (),

            // 0x23c..=0x23f => self.logibm.write(port as u8 & 3, val.require_u8()?),

            0x94 => warn!(target: extend_path_with!("port"), "TODO: system board enable/setup register"),
            0x96 => warn!(target: extend_path_with!("port"), "TODO: adapter enable/setup register"),

            // ???
            0x3EA | 0x2ea
            // LPT
            | 0x378 | 0x278
            // ???
            | 0x37a | 0x27a  => info!(target: extend_path_with!("port"), "TODO: Write to port 0x{port:X}"),

            0x402 => self.bios_info_logger.write(val.require_u8()?),
            0x403 => self.bios_debug_logger.write(val.require_u8()?),
            0x500 => self.vga_logger.write(val.require_u8()?),

            0xCF8 => self.pci.write_address(val.u32()),
            0xCFC..=0xCFF => self.pci.write_data(&mut Space {
                ide: &mut self.ide,
                isa_bridge: &mut self.isa_bridge,
                host_bridge: &mut self.pci_host_bridge,
                acpi: &mut self.acpi,
                es1370: self.es1370.as_mut(),
                vga: &mut self.vga,
                ne2k: &mut self.ne2k,
            }, port & 3, val),

            port => {
                let result = port_write_chain! {
                    port, val, &mut mmio!(self, phys_cache, 'tag) => {
                        &mut self.ide,
                        &mut self.acpi,
                        &mut self.es1370.as_mut(),
                        &mut self.ne2k,
                    }
                };

                if let Some(result) = result {
                    return result
                } else {
                    error!(target: extend_path_with!("port"), "TODO: Write 0x{val:X} to port 0x{port:X}")
                }
            },
        }

        Ok(())
    }

    pub fn read_port_u8(&mut self, port: u16, ctx: &mut InstructionCache<'_>) -> u8 {
        match self.read_port::<u8>(port, ctx) {
            Ok(val) => val,
            Err(e) => {
                panic!("Error: {e:?} reading u8 from port 0x{port:04X}");
            },
        }
    }

    pub fn read_port_u16(&mut self, port: u16, ctx: &mut InstructionCache<'_>) -> u16 {
        if let Ok(result) = self.read_port::<u16>(port, ctx) {
            result
        } else {
            self.read_port_u8(port, ctx) as u16 | ((self.read_port_u8(port + 1, ctx) as u16) << 8)
        }
    }

    pub fn read_port_u32(&mut self, port: u16, phys_cache: &mut InstructionCache<'_>) -> u32 {
        if let Ok(result) = self.read_port::<u32>(port, phys_cache) {
            result
        } else if let Ok(result) = self.read_port::<u16>(port, phys_cache) {
            result as u32 | ((self.read_port_u16(port + 1, phys_cache) as u32) << 16)
        } else {
            self.read_port_u8(port, phys_cache) as u32
                | ((self.read_port_u8(port + 1, phys_cache) as u32) << 8)
                | ((self.read_port_u8(port + 2, phys_cache) as u32) << 16)
                | ((self.read_port_u8(port + 3, phys_cache) as u32) << 24)
        }
    }

    pub fn write_port_u8(&mut self, port: u16, val: u8, phys_cache: &mut InstructionCache<'_>) {
        self.write_port(port, val, phys_cache).unwrap()
    }

    pub fn write_port_u16(&mut self, port: u16, val: u16, ctx: &mut InstructionCache<'_>) {
        if self.write_port(port, val, ctx).is_err() {
            self.write_port_u8(port, val as u8, ctx);
            self.write_port_u8(port + 1, (val >> 8) as u8, ctx);
        }
    }

    pub fn write_port_u32(&mut self, port: u16, val: u32, ctx: &mut InstructionCache<'_>) {
        if self.write_port(port, val, ctx).is_err() {
            if self.write_port(port, val as u16, ctx).is_ok() {
                self.write_port_u16(port + 1, (val >> 16) as u16, ctx);
            } else {
                self.write_port_u8(port, val as u8, ctx);
                self.write_port_u8(port + 1, (val >> 8) as u8, ctx);
                self.write_port_u8(port + 2, (val >> 16) as u8, ctx);
                self.write_port_u8(port + 3, (val >> 24) as u8, ctx);
            }
        }
    }

    pub fn check_interrupt(&self) -> Option<u8> {
        self.lapic
            .next_pending_interrupt()
            .or_else(|| self.core.shared_pic_core.get_next_interrupt())
    }

    pub fn video_memory(&self) -> VideoMemory {
        self.vga.video_memory()
    }

    #[inline(always)]
    pub fn mmio<'r, 'tag>(&'r mut self, phys_cache: &'r mut InstructionCache<'tag>) -> HwMmio<'r, 'tag> {
        mmio!(self, phys_cache, 'tag)
    }

    pub fn vector_offsets(&self) -> (u8, u8) {
        (self.core.primary_pic.vector_offset(), self.core.secondary_pic.vector_offset())
    }

    pub fn trace_disengaged(&self) {
        // It seems we generate a few too many FDC interrupts.
        // For now we just clear these when the trace disengages to avoid triggering old pending interrupts.
        self.core.primary_pic.clear_pending();
        self.core.secondary_pic.clear_pending();
        self.lapic.clear_pending();
        self.ioapic.clear_pending();
        if let Some(es1370) = &self.es1370 {
            es1370.clear_pending_interrupts();
        }

        warn!("Cleared pending interrupts");
        warn!("CMOS interrupt enabled: {}", self.core.cmos.irq_enabled());
    }

    pub fn disable_es1370(&mut self) {
        self.es1370 = None;
    }

    pub fn apic_is_enabled(&self) -> bool {
        self.lapic.is_enabled()
    }

    pub fn ioapic(&self) -> &IoApic {
        &self.ioapic
    }

    pub fn read_msr(&self, cpu: &State, msr: u32, k: u64) -> u64 {
        match msr {
            // TSC
            0x10 => k >> 3,
            0x1b => self.lapic.read_apic_base(),
            0xfe => 0x508,
            0x174 => cpu.gpreg(GpReg::SysEnterCs),
            0x175 => cpu.gpreg(GpReg::SysEnterSp),
            0x176 => cpu.gpreg(GpReg::SysEnterIp),
            // IA32_MTRRdefType
            0x2ff => 0xC06,
            n => {
                error!("TODO: read MSR 0x{n:X}");
                0
            },
        }
    }

    pub fn write_msr(&mut self, cpu: &mut State, msr: u32, val: u64) {
        match msr {
            0x1b => self.lapic.write_apic_base(val, &self.mem),
            0x174 => cpu.set_gpreg(GpReg::SysEnterCs, val),
            0x175 => cpu.set_gpreg(GpReg::SysEnterSp, val),
            0x176 => cpu.set_gpreg(GpReg::SysEnterIp, val),
            _ => error!("TODO: Write MSR 0x{msr:X} = 0x{val:X}"),
        }
    }

    pub fn redirection_entry_vector(&self, index: usize) -> u8 {
        self.ioapic.redirection_entry_vector(index)
    }

    pub fn snapshot(&mut self) -> HwSnapshot {
        self.clock.pause();

        HwSnapshot {
            core: CoreHwSnapshot {
                pit: self.core.pit.snapshot(),
                cmos: self.core.cmos.snapshot(),
                dma: self.core.dma.snapshot(),
                primary_pic: self.core.primary_pic.snapshot(),
                secondary_pic: self.core.secondary_pic.snapshot(),
                shared_pic_core: self.core.shared_pic_core.snapshot(),
            },
            ppi: self.ppi.snapshot(),
            ioapic: self.ioapic.snapshot(),
            lapic: self.lapic.snapshot(),
            vga: self.vga.snapshot(),
            fdc: self.fdc.snapshot(),
            ide: self.ide.snapshot(),
            xtide: self.xtide.snapshot(),
            com1: self.com1.snapshot(),
            es1370: self.es1370.as_mut().map(|e| e.snapshot()),
            vga_logger: self.vga_logger.snapshot(),
            bios_info_logger: self.bios_info_logger.snapshot(),
            bios_debug_logger: self.bios_debug_logger.snapshot(),
            pci: self.pci.snapshot(),
            acpi: self.acpi.snapshot(),
            isa_bridge: self.isa_bridge.snapshot(),
            pci_host_bridge: self.pci_host_bridge.snapshot(),
            clock: self.clock.snapshot(),
            ne2k: self.ne2k.snapshot(),
        }
    }

    pub fn restore(&mut self, hw: HwSnapshot) {
        self.core.pit.restore(hw.core.pit, &self.clock);
        self.core.cmos.restore(hw.core.cmos, &self.clock);
        self.core.dma.restore(hw.core.dma);
        self.core.primary_pic.restore(hw.core.primary_pic);
        self.core.secondary_pic.restore(hw.core.secondary_pic);
        self.core.shared_pic_core.restore(hw.core.shared_pic_core);
        self.ppi.restore(hw.ppi);
        self.ioapic.restore(hw.ioapic);
        self.lapic.restore(hw.lapic);
        self.vga.restore(hw.vga, &self.mem);
        self.fdc.restore(hw.fdc);
        self.ide.restore(hw.ide);
        self.xtide.restore(hw.xtide);
        self.com1.restore(hw.com1);
        match (&mut self.es1370, hw.es1370) {
            (Some(es1370), Some(snapshot)) => es1370.restore(snapshot, &self.clock),
            (None, None) => (),
            _ => panic!("hardware change"),
        }
        self.vga_logger.restore(hw.vga_logger);
        self.bios_info_logger.restore(hw.bios_info_logger);
        self.bios_debug_logger.restore(hw.bios_debug_logger);
        self.pci.restore(hw.pci);
        self.acpi.restore(hw.acpi);
        self.isa_bridge.restore(hw.isa_bridge);
        self.pci_host_bridge.restore(hw.pci_host_bridge);
        self.clock.restore(hw.clock);
        self.ne2k.restore(hw.ne2k);
    }

    pub fn pause(&mut self) {
        self.clock.pause();
    }

    pub fn start_clock(&mut self) {
        self.clock.start()
    }
}

pub struct HwMmio<'a, 'tag> {
    pub vga: &'a mut Vga,
    pub ioapic: &'a mut IoApic,
    pub lapic: &'a mut LocalApic,
    pub phys_cache: &'a mut InstructionCache<'tag>,
    pub clock: &'a EmulatorClock,
}

impl Mmio for HwMmio<'_, '_> {
    fn read_mem<D: MemoryData>(&mut self, id: MmioId, address: u32) -> D {
        match id {
            MMIO_ID_VGA => {
                let mut buf = [0; 8];
                for (n, b) in buf.iter_mut().enumerate().take(D::NUM_BYTES) {
                    *b = self.vga.read_video_memory(address + n as u32)
                }

                D::from_bytes(&buf[..D::NUM_BYTES])
            },
            MMIO_ID_IOAPIC => D::from_u32_with_offset(address & 3, self.ioapic.read((address & 0xfff) >> 4)),
            MMIO_ID_LAPIC => D::from_u32_with_offset(address & 3, self.lapic.read((address & 0xfff) >> 4, self.clock)),
            other => {
                error!("invoked MMIO read with unknown id: {other:?}, offset: 0x{address:X}");
                D::MAX
            },
        }
    }

    fn write_mem<D: MemoryData>(&mut self, id: MmioId, address: u32, val: D) {
        match id {
            MMIO_ID_VGA => {
                let bytes = val.to_bytes();
                let bytes = bytes.as_ref();
                for (n, &b) in bytes.iter().enumerate() {
                    self.vga.write_video_memory(address + n as u32, b)
                }
            },
            MMIO_ID_IOAPIC => {
                if let Some(val) = val.into_u32_exact() {
                    self.ioapic.write((address & 0xfff) >> 4, val);
                } else {
                    warn!(
                        "Silently ignoring {}-byte write to IOAPIC address 0x{address:X}",
                        D::NUM_BYTES
                    )
                }
            },
            MMIO_ID_LAPIC => {
                if let Some(val) = val.into_u32_exact() {
                    self.lapic.write((address & 0xfff) >> 4, val);
                } else {
                    warn!("Silently ignoring {}-byte write to LAPIC address 0x{address:X}", D::NUM_BYTES)
                }
            },
            other => {
                error!("invoked MMIO write with unknown id: {other:?}, offset: 0x{address:X}, value: 0x{val:X}");
            },
        }
    }

    fn notify_memory_dirty(&mut self, phys_frame_index: PhysFrameIndex, memory: &Mem32) {
        self.phys_cache.notify_memory_dirty(phys_frame_index, memory);
    }

    fn advise_memory_dirty(&mut self, addr: sem86_arch::addr::PhysAddr, len: u8) -> sem86_arch::mem::MarkDirtyAdvice {
        self.phys_cache.advise_mark_dirty(addr, len)
    }
}
