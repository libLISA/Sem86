use std::iter::repeat_with;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use arbitrary_int::Number;
use attribute::AttributeRegisters;
use bilge::prelude::*;
use crt::CrtRegisters;
use dispi::DispiRegisters;
use graphics::{DataRotateFunction, GraphicsRegisters, MemoryMap, WriteMode};
use itertools::Itertools;
use liblisa::utils::bitmap::{BitmapSlice, FixedBitmapU64};
use log::{debug, error, info, trace, warn};
use sem86_arch::mem::{Mem32, MemoryWatcher, MmioId, Shm};
use sequencer::SequencerRegisters;
use serde::{Deserialize, Serialize};

use super::bank::RegisterBank;
use super::reg::Reg8;
use crate::hw::MMIO_ID_VGA;
use crate::hw::pci::{
    Bar, BarSnapshot, CommonPciHeader, CommonWriteEvent, DeviceWriteEvent, GeneralDeviceHeader, PciCommandRegister, PciDevice,
};
use crate::hw::vga::ddc::Ddc;

mod attribute;
mod crt;
mod ddc;
mod dispi;
mod graphics;
mod sequencer;

const DISPI_XRES: u8 = 1;
const DISPI_YRES: u8 = 2;
const DISPI_BPP: u8 = 3;
const DISPI_ENABLE: u8 = 4;
const DISPI_BANK: u8 = 5;
#[allow(unused)]
const DISPI_VIRT_WIDTH: u8 = 6;
const DISPI_VIRT_HEIGHT: u8 = 7;
#[allow(unused)]
const DISPI_X_OFFSET: u8 = 8;
#[allow(unused)]
const DISPI_Y_OFFSET: u8 = 9;
const DISPI_VIDEO_MEMORY_64K: u8 = 10;
const DISPI_DDC: u8 = 11;

pub const DEFAULT_DAC_COLORS: [u32; 256] = [
    0x000000, 0x2A0000, 0x002A00, 0x2A2A00, 0x00002A, 0x2A002A, 0x00152A, 0x2A2A2A, 0x151515, 0x3F1515, 0x153F15, 0x3F3F15,
    0x15153F, 0x3F153F, 0x153F3F, 0x3F3F3F, 0x000000, 0x050505, 0x080808, 0x0B0B0B, 0x0E0E0E, 0x111111, 0x141414, 0x181818,
    0x1C1C1C, 0x202020, 0x242424, 0x282828, 0x2D2D2D, 0x323232, 0x383838, 0x3F3F3F, 0x3F0000, 0x3F0010, 0x3F001F, 0x3F002F,
    0x3F003F, 0x2F003F, 0x1F003F, 0x10003F, 0x00003F, 0x00103F, 0x001F3F, 0x002F3F, 0x003F3F, 0x003F2F, 0x003F1F, 0x003F10,
    0x003F00, 0x103F00, 0x1F3F00, 0x2F3F00, 0x3F3F00, 0x3F2F00, 0x3F1F00, 0x3F1000, 0x3F1F1F, 0x3F1F27, 0x3F1F2F, 0x3F1F37,
    0x3F1F3F, 0x371F3F, 0x2F1F3F, 0x271F3F, 0x1F1F3F, 0x1F273F, 0x1F2F3F, 0x1F373F, 0x1F3F3F, 0x1F3F37, 0x1F3F2F, 0x1F3F27,
    0x1F3F1F, 0x273F1F, 0x2F3F1F, 0x373F1F, 0x3F3F1F, 0x3F371F, 0x3F2F1F, 0x3F271F, 0x3F2D2D, 0x3F2D31, 0x3F2D36, 0x3F2D3A,
    0x3F2D3F, 0x3A2D3F, 0x362D3F, 0x312D3F, 0x2D2D3F, 0x2D313F, 0x2D363F, 0x2D3A3F, 0x2D3F3F, 0x2D3F3A, 0x2D3F36, 0x2D3F31,
    0x2D3F2D, 0x313F2D, 0x363F2D, 0x3A3F2D, 0x3F3F2D, 0x3F3A2D, 0x3F362D, 0x3F312D, 0x1C0000, 0x1C0007, 0x1C000E, 0x1C0015,
    0x1C001C, 0x15001C, 0x0E001C, 0x07001C, 0x00001C, 0x00071C, 0x000E1C, 0x00151C, 0x001C1C, 0x001C15, 0x001C0E, 0x001C07,
    0x001C00, 0x071C00, 0x0E1C00, 0x151C00, 0x1C1C00, 0x1C1500, 0x1C0E00, 0x1C0700, 0x1C0E0E, 0x1C0E11, 0x1C0E15, 0x1C0E18,
    0x1C0E1C, 0x180E1C, 0x150E1C, 0x110E1C, 0x0E0E1C, 0x0E111C, 0x0E151C, 0x0E181C, 0x0E1C1C, 0x0E1C18, 0x0E1C15, 0x0E1C11,
    0x0E1C0E, 0x111C0E, 0x151C0E, 0x181C0E, 0x1C1C0E, 0x1C180E, 0x1C150E, 0x1C110E, 0x1C1414, 0x1C1416, 0x1C1418, 0x1C141A,
    0x1C141C, 0x1A141C, 0x18141C, 0x16141C, 0x14141C, 0x14161C, 0x14181C, 0x141A1C, 0x141C1C, 0x141C1A, 0x141C18, 0x141C16,
    0x141C14, 0x161C14, 0x181C14, 0x1A1C14, 0x1C1C14, 0x1C1A14, 0x1C1814, 0x1C1614, 0x100000, 0x100004, 0x100008, 0x10000C,
    0x100010, 0x0C0010, 0x080010, 0x040010, 0x000010, 0x000410, 0x000810, 0x000C10, 0x001010, 0x00100C, 0x001008, 0x001004,
    0x001000, 0x041000, 0x081000, 0x0C1000, 0x101000, 0x100C00, 0x100800, 0x100400, 0x100808, 0x10080A, 0x10080C, 0x10080E,
    0x100810, 0x0E0810, 0x0C0810, 0x0A0810, 0x080810, 0x080A10, 0x080C10, 0x080E10, 0x081010, 0x08100E, 0x08100C, 0x08100A,
    0x081008, 0x0A1008, 0x0C1008, 0x0E1008, 0x101008, 0x100E08, 0x100C08, 0x100A08, 0x100B0B, 0x100B0C, 0x100B0D, 0x100B0F,
    0x100B10, 0x0F0B10, 0x0D0B10, 0x0C0B10, 0x0B0B10, 0x0B0C10, 0x0B0D10, 0x0B0F10, 0x0B1010, 0x0B100F, 0x0B100D, 0x0B100C,
    0x0B100B, 0x0C100B, 0x0D100B, 0x0F100B, 0x10100B, 0x100F0B, 0x100D0B, 0x100C0B, 0x000000, 0x000000, 0x000000, 0x000000,
    0x000000, 0x000000, 0x000000, 0x000000,
];

#[derive(Debug)]
pub struct Vga {
    mem: Arc<Mem32>,
    status: u8,
    modeset: Sender<ModeSet>,
    mode_select: Reg8,
    cga_palette: Reg8,
    sequencer: RegisterBank<SequencerRegisters, 3>,
    crt: RegisterBank<CrtRegisters, 5>,
    graphics: RegisterBank<GraphicsRegisters, 4>,
    attribute: RegisterBank<AttributeRegisters, 5>,
    write_attribute_data: bool,
    dispi: RegisterBank<DispiRegisters, 5>,
    video_memory: Arc<Shm>,
    last_modeset_sent: ModeSet,
    latches: [u8; 4],
    dac_palette: [u32; 256],
    dac_palette_write_offset: usize,
    dac_palette_read_offset: usize,
    feature_control: Reg8,
    miscellaneous_output_register: MiscellaneousOutputRegister,
    pci_header: GeneralDeviceHeader,
    vga_bios: Arc<Shm>,
    pci_rom_bar: Bar,
    ddc: Ddc,
    ddc_enabled: bool,
    vram_mapped_range: MappingRange,
    lfb_mapped_range: MappingRange,
    watcher: Arc<dyn MemoryWatcher>,
    video_memory_ref: VideoMemory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VgaSnapshot {
    status: u8,
    mode_select: Reg8,
    cga_palette: Reg8,
    sequencer: RegisterBank<SequencerRegisters, 3>,
    crt: RegisterBank<CrtRegisters, 5>,
    graphics: RegisterBank<GraphicsRegisters, 4>,
    attribute: RegisterBank<AttributeRegisters, 5>,
    write_attribute_data: bool,
    dispi: RegisterBank<DispiRegisters, 5>,
    last_modeset_sent: ModeSet,
    latches: [u8; 4],
    #[serde(with = "serde_big_array::BigArray")]
    dac_palette: [u32; 256],
    dac_palette_write_offset: usize,
    dac_palette_read_offset: usize,
    feature_control: Reg8,
    miscellaneous_output_register: MiscellaneousOutputRegister,
    pci_header: GeneralDeviceHeader,
    pci_rom_bar: BarSnapshot,
    ddc: Ddc,
    ddc_enabled: bool,
    video_memory: Vec<u8>,
    vram_mapped_range: MappingRangeSnapshot,
    lfb_mapped_range: MappingRangeSnapshot,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Text,
    Graphics,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Serialize, Deserialize)]
pub struct MiscellaneousOutputRegister {
    io_address_select: bool,
    enable_ram: bool,
    clock_select: u2,
    reserved: bool,
    select_high_bank: bool,
    horizontal_sync_polarity: bool,
    vertical_sync_polarity: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits)]
pub struct InputStatus1 {
    /// If false, display is in horizontical or vertical retrace
    display_enabled: bool,
    reserved: u2,

    /// If true, display is in vertical retrace
    in_vertical_retrace: bool,
    reserved: u4,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSet {
    pub width: u16,
    pub height: u16,
    pub enable_color: bool,
    pub enable_video: bool,
    pub enable_blink: bool,
    pub start_address: u32,
    pub palette: u8,
    pub is_graphics: bool,
    pub force_43_aspect_ratio: bool,
    pub memory_addressing: MemoryAddressing,
    #[serde(with = "serde_big_array::BigArray")]
    pub dac_palette: [u32; 256],
    pub vga_palette: [u8; 16],
    stride: u16,
}

impl Default for ModeSet {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            enable_color: false,
            enable_video: false,
            enable_blink: false,
            force_43_aspect_ratio: false,
            start_address: 0,
            palette: 0,
            is_graphics: false,
            memory_addressing: MemoryAddressing::default(),
            dac_palette: [0; 256],
            vga_palette: [0; 16],
            stride: 0,
        }
    }
}

#[bitsize(4)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, FromBits, Serialize, Deserialize)]
pub enum MemoryAddressing {
    #[default]
    OddEven,
    CgaOddEven,
    ShiftRegister,
    Planar4,
    #[fallback]
    Linear8,
    Linear15,
    Linear16,
    Linear24,
    Linear32,
}

#[derive(Debug)]
struct MappingRange {
    data: Option<MappingRangeData>,
    shm: Arc<Shm>,
    watcher: Option<Arc<dyn MemoryWatcher>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MappingRangeData {
    range: Range<u64>,
    offset: u32,
    mmio_id: Option<MmioId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MappingRangeSnapshot {
    data: Option<MappingRangeData>,
}

impl MappingRange {
    pub fn new(shm: Arc<Shm>, watcher: Option<Arc<dyn MemoryWatcher>>) -> Self {
        Self {
            data: None,
            shm,
            watcher,
        }
    }

    pub fn unmap(&mut self, mem: &Mem32) {
        if let Some(data) = self.data.take() {
            mem.map_physical_memory_to_default(data.range);
        }
    }

    pub fn map_at(&mut self, mem: &Mem32, range: Range<u64>, offset: u32, mmio_id: Option<MmioId>) {
        let new_data = MappingRangeData {
            range,
            offset,
            mmio_id,
        };

        if self.data.as_ref() != Some(&new_data) {
            self.unmap(mem);

            if let Some(mmio_id) = new_data.mmio_id {
                mem.map_physical_memory_to_mmio(new_data.range.clone(), mmio_id);
            } else {
                mem.map_physical_memory_to_shm(
                    new_data.range.clone(),
                    self.shm.clone(),
                    self.watcher.clone(),
                    new_data.offset as usize,
                    true,
                );
            }

            self.data = Some(new_data);
        }
    }

    pub fn snapshot(&self) -> MappingRangeSnapshot {
        MappingRangeSnapshot {
            data: self.data.clone(),
        }
    }

    pub fn restore(&mut self, snapshot: MappingRangeSnapshot, mem: &Mem32) {
        if let Some(data) = snapshot.data {
            self.map_at(mem, data.range, data.offset, data.mmio_id);
        } else {
            self.unmap(mem);
        }
    }

    fn current_range(&self) -> Option<Range<u64>> {
        self.data.as_ref().map(|data| data.range.clone())
    }
}

fn dirty_ranges(bitmap: FixedBitmapU64<4>) -> impl Iterator<Item = Range<usize>> {
    let mut pos = 0;
    repeat_with(move || {
        while pos < bitmap.len() && !bitmap.get(pos) {
            pos += 1;
        }

        if pos >= bitmap.len() {
            None
        } else {
            let start = pos;
            while pos < bitmap.len() && bitmap.get(pos) {
                pos += 1;
            }

            Some(start..pos)
        }
    })
    .take_while(|x| x.is_some())
    .flatten()
}

#[derive(Clone, Debug)]
struct Watcher {
    dirty_map: Arc<[AtomicU64; 4]>,

    /// Video memory size divided by 64 * 4.
    block_size: u64,
}

impl MemoryWatcher for Watcher {
    fn notify_dirty(&self, offset: u64) {
        let index = offset / self.block_size;
        self.dirty_map[index as usize / 64].fetch_or(1 << (index % 64), Ordering::Relaxed);

        let vals = FixedBitmapU64::from_u64_array(std::array::from_fn(|n| self.dirty_map[n].load(Ordering::Relaxed)));
        trace!(
            "Video memory at {offset:X} marked dirty -- dirty ranges: {:X?}",
            dirty_ranges(vals)
                .map(move |range| range.start as u32 * self.block_size as u32..range.end as u32 * self.block_size as u32)
                .format(", ")
        );
    }
}

#[derive(Clone)]
pub struct VideoMemory {
    dirty_map: Arc<[AtomicU64; 4]>,
    ranges: Arc<[Range<AtomicU64>; 2]>,
    shm: Arc<Shm>,
    mem: Arc<Mem32>,
}

impl std::fmt::Debug for VideoMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoMemory").finish()
    }
}

impl VideoMemory {
    pub fn read_slice(&self, start_address: u32, slice: &mut [u8]) {
        self.shm.view().read_slice(start_address, slice);
    }

    pub fn dirty_ranges(&self) -> impl Iterator<Item = Range<u32>> {
        let data = std::array::from_fn(|n| self.dirty_map[n].load(Ordering::Relaxed));
        let vals = FixedBitmapU64::from_u64_array(data);
        let block_size = (self.shm.len() / 64 / 4) as u32;
        dirty_ranges(vals).map(move |range| range.start as u32 * block_size..range.end as u32 * block_size)
    }

    pub fn get_and_clear_dirty_ranges(&self) -> impl Iterator<Item = Range<u32>> {
        // TODO: There might be some race conditions here. Double-check the atomic ordering if we ever run into any visual artifacts.
        // TODO: In particular: is there a way to perform a write such that it never ends up in the dirty ranges?

        // A write to memory that was previously dirty, but has now been cleaned, might not generate a dirty notification.
        // TODO: Is there any memory ordering on the clean/dirty marks we could use to guarantee this doesn't happen? Alternatively, we could just reload the entire VRAM every so often and accept that there might be a few pixels that don't update properly every now and then.
        let data = std::array::from_fn(|n| self.dirty_map[n].swap(0, Ordering::Acquire));
        for range in self.ranges.iter() {
            let start = range.start.load(Ordering::Acquire);
            let end = range.end.load(Ordering::Acquire);

            if start < end {
                self.mem.clean_phys_frame_range(start..end);
            }
        }

        let vals = FixedBitmapU64::from_u64_array(data);
        let block_size = (self.shm.len() / 64 / 4) as u32;
        dirty_ranges(vals).map(move |range| range.start as u32 * block_size..range.end as u32 * block_size)
    }

    pub fn size(&self) -> u64 {
        self.shm.len()
    }
}

impl Vga {
    pub fn new(modeset: Sender<ModeSet>, mem: Arc<Mem32>, vga_bios: Arc<Shm>) -> Self {
        // 16MiB of video memory
        // We map 32KiB at 0xb8000 for now.
        let video_memory = Arc::new(Shm::new("vga_video_memory", 16 << 20));

        let dirty_map = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
        let watcher = Arc::new(Watcher {
            dirty_map: dirty_map.clone(),
            block_size: video_memory.len() / 64 / 4,
        });

        let mut vram_mapped_range = MappingRange::new(video_memory.clone(), Some(watcher.clone()));
        vram_mapped_range.map_at(&mem, 0xb8000..0xc0000, 0, Some(MMIO_ID_VGA));

        let lfb_mapped_range = MappingRange::new(video_memory.clone(), Some(watcher.clone()));
        Self {
            watcher,
            video_memory_ref: VideoMemory {
                shm: video_memory.clone(),
                ranges: Arc::new([
                    AtomicU64::new(0xb8000)..AtomicU64::new(0xc0000),
                    AtomicU64::new(0)..AtomicU64::new(0),
                ]),
                dirty_map,
                mem: mem.clone(),
            },
            mem,
            status: 1,
            modeset,
            mode_select: Reg8::new(0),
            cga_palette: Reg8::new(0),
            sequencer: RegisterBank::new(),
            crt: RegisterBank::new(),
            graphics: RegisterBank::new(),
            dispi: RegisterBank::new(),
            attribute: RegisterBank::new(),
            write_attribute_data: false,
            video_memory,
            last_modeset_sent: Default::default(),
            latches: [0; 4],
            dac_palette: [0xffffffff; 256],
            dac_palette_write_offset: 0,
            dac_palette_read_offset: 0,
            feature_control: Reg8::new(0),
            pci_rom_bar: Bar::new(vga_bios.clone(), false),
            vga_bios,
            miscellaneous_output_register: MiscellaneousOutputRegister::new(true, true, u2::new(0), true, true, false),
            pci_header: GeneralDeviceHeader {
                common: CommonPciHeader {
                    vendor_id: 0x1234,
                    device_id: 0x1111,
                    command: PciCommandRegister::from(0),
                    status: 0x0280,
                    revision_id: 0,
                    class_code: 0x03,
                    subclass: 0x00,
                    prog_if: 0,
                    bist: 0,
                    cache_line_size: 0x08,
                    latency_timer: 0x20,
                    header_type: 0,
                },
                bar: [0xE000_0008, 0x0, 0x0, 0x0, 0x0, 0x0],
                cardbus_cis_pointer: 0,
                subsystem_vendor_id: 0,
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
            ddc: Ddc::new(),
            ddc_enabled: true,
            vram_mapped_range,
            lfb_mapped_range,
        }
    }

    pub fn clean_video_memory(&self) {
        if let Some(range) = self.vram_mapped_range.current_range() {
            self.mem.clean_phys_frame_range(range);
        }

        if let Some(range) = self.lfb_mapped_range.current_range() {
            self.mem.clean_phys_frame_range(range);
        }
    }

    fn max_resolution(&self) -> (u16, u16) {
        (4096, 4096)
    }

    pub fn read_dispi(&mut self, port: u8) -> u16 {
        match port {
            0x0 => self.dispi.read_addr() as u16,
            0x1 => {
                let result = match self.dispi.current_addr() {
                    DISPI_XRES if self.dispi.enable.caps() => self.max_resolution().0,
                    DISPI_YRES if self.dispi.enable.caps() => self.max_resolution().1,
                    DISPI_BPP if self.dispi.enable.caps() => 32,
                    DISPI_BANK if self.dispi.enable.caps() => 2560,
                    DISPI_VIDEO_MEMORY_64K => (self.video_memory.len() >> 16) as u16,
                    DISPI_DDC => {
                        if self.ddc_enabled {
                            0x80 | self.ddc.read().as_u8() as u16
                        } else {
                            0xf
                        }
                    },
                    _ => self.dispi.read(),
                };

                debug!("DISPI read: 0x{:X} = 0x{result:04X}", self.dispi.current_addr());

                result
            },
            _ => unreachable!(),
        }
    }

    pub fn write_dispi(&mut self, port: u8, val: u16, memory: &Mem32) {
        match port {
            0x0 => self.dispi.write_addr(val as u8),
            0x1 => {
                debug!("DISPI write: 0x{:X} = 0x{:04X}", self.dispi.current_addr(), val);
                match self.dispi.current_addr() {
                    DISPI_VIRT_HEIGHT => (),
                    DISPI_DDC => {
                        self.ddc_enabled = val & 0x80 != 0;
                        if self.ddc_enabled {
                            self.ddc.write(val & 0b01 != 0, val & 0b10 != 0);
                        }
                    },
                    _ => {
                        let was_enabled = self.dispi.enable.vbe_enabled();
                        self.dispi.write(val);

                        if self.dispi.current_addr() == DISPI_ENABLE && !was_enabled && self.dispi.enable.vbe_enabled() {
                            if !self.dispi.enable.no_clear_mem() {
                                for addr in 0..self.video_memory.len() as u32 {
                                    self.video_memory.view().write_byte(addr, 0);
                                }
                            }

                            self.send_modeset();
                            info!("DISPI was enabled: {:?}", self.dispi);
                        }

                        self.update_mappings(memory);
                    },
                }
            },
            _ => unreachable!(),
        }
    }

    // TODO: We need to have banked mode and LFB mapped at the same time.
    fn update_mappings(&mut self, memory: &Mem32) {
        if self.dispi.enable.vbe_enabled() {
            let bar0: u64 = (self.pci_header.bar[0] & self.bar0_mask()) as u64;
            info!("Mapping LFB at 0x{bar0:X}");
            self.lfb_mapped_range
                .map_at(memory, bar0..bar0 + self.video_memory.len(), 0, None);
        } else {
            info!("Unmapping LFB");
            self.lfb_mapped_range.unmap(memory);
            self.video_memory_ref.ranges[1].start.store(0, Ordering::Release);
            self.video_memory_ref.ranges[1].end.store(0, Ordering::Release);
        }

        let r = self.lfb_mapped_range.current_range();
        self.video_memory_ref.ranges[1]
            .start
            .store(r.as_ref().map(|r| r.start).unwrap_or(0), Ordering::Release);
        self.video_memory_ref.ranges[1]
            .end
            .store(r.map(|r| r.end).unwrap_or(0), Ordering::Release);

        if self.dispi.enable.vbe_enabled() {
            let bank_index = self.dispi.bank & 0x1fff;
            let bank_offset = if self.dispi.enable.bank_granularity_32k() {
                bank_index as usize * (32 << 10)
            } else {
                bank_index as usize * (64 << 10)
            };

            info!("Mapping banked framebuffer at {:X?}", self.mapped_range());
            self.vram_mapped_range
                .map_at(memory, self.mapped_range(), bank_offset as u32, None);
        } else {
            info!("Mapping MMIO framebuffer at {:X?}", self.mapped_range());
            self.vram_mapped_range
                .map_at(memory, self.mapped_range(), 0, Some(MMIO_ID_VGA));
        }

        let r = self.vram_mapped_range.current_range();
        self.video_memory_ref.ranges[0]
            .start
            .store(r.as_ref().map(|r| r.start).unwrap_or(0), Ordering::Release);
        self.video_memory_ref.ranges[0]
            .end
            .store(r.map(|r| r.end).unwrap_or(0), Ordering::Release);
    }

    pub fn read_vga(&mut self, port: u8) -> u8 {
        match port {
            0x0 => self.attribute.read_addr(),
            0x1 => self.attribute.read(),
            0x4 => self.sequencer.read_addr(),
            0x5 => self.sequencer.read(),
            0x9 => {
                let p = &mut self.dac_palette[self.dac_palette_read_offset / 3];
                let bit_offset = (self.dac_palette_read_offset % 3) * 8;

                self.dac_palette_read_offset += 1;
                (*p >> bit_offset) as u8
            },
            0xA => self.feature_control.read(),
            0xC => self.miscellaneous_output_register.value,
            0xE => self.graphics.read_addr(),
            0xF => self.graphics.read(),
            // TODO: 0xA: feature control
            // TODO: 0xC: miscellaneous output register
            _ => {
                warn!("unhandled read from VGA port 0x{port:X} = 0xFF");
                0xff
            },
        }
    }

    pub fn mapped_range(&self) -> Range<u64> {
        if self.dispi.enable.vbe_enabled() {
            0xa0000..0xc0000
        } else {
            match self.graphics.miscellaneous.memory_map() {
                MemoryMap::A0000For128Kb => 0xa0000..0xc0000,
                MemoryMap::A0000For64Kb => 0xa0000..0xb0000,
                MemoryMap::B0000For32Kb => 0xb0000..0xb8000,
                MemoryMap::B8000For32Kb => 0xb8000..0xc0000,
            }
        }
    }

    pub fn write_vga(&mut self, port: u8, val: u8, memory: &Mem32) {
        match port {
            0x0 => {
                if self.write_attribute_data {
                    self.attribute.write(val)
                } else {
                    self.attribute.write_addr(val)
                }

                self.write_attribute_data = !self.write_attribute_data;
            },
            0x1 => (),
            0x2 => {
                self.miscellaneous_output_register = MiscellaneousOutputRegister::from(val & !0x30);
            },
            0x4 => self.sequencer.write_addr(val),
            0x5 => self.sequencer.write(val),
            0x7 => self.dac_palette_read_offset = val as usize * 3,
            0x8 => self.dac_palette_write_offset = val as usize * 3,
            0x9 => {
                debug!("Write DAC palette {} = {val:02X}", self.dac_palette_write_offset);
                let p = &mut self.dac_palette[(self.dac_palette_write_offset / 3) % 256];
                let bit_offset = (self.dac_palette_write_offset % 3) * 8;
                let mask = 0xff << bit_offset;

                *p = (*p & !mask) | ((val as u32) << bit_offset);

                self.dac_palette_write_offset += 1;
                self.send_modeset();
            },
            0xA => self.feature_control.write(val),
            0xE => self.graphics.write_addr(val),
            0xF => {
                let prev_mmap = self.graphics.miscellaneous.memory_map();
                self.graphics.write(val);

                let new_mmap = self.graphics.miscellaneous.memory_map();
                if prev_mmap != new_mmap {
                    info!("Switching VGA memory mapping to {new_mmap:?}");
                    self.update_mappings(memory);
                }
            },
            _ => warn!("unhandled write to VGA port 0x{port:X} = 0x{val:X}"),
        }

        self.send_modeset();
    }

    pub fn read_cga(&mut self, port: u8) -> u8 {
        let val = match port {
            0 | 2 | 4 | 6 => self.crt.read_addr(),
            1 | 3 | 5 | 7 => self.crt.read(),
            8 => self.mode_select.read(),
            9 => self.cga_palette.read(),
            0xa => {
                // Input status register 1
                self.write_attribute_data = false;

                trace!("Read from CGA status register");

                self.status = self.status.wrapping_add(1);
                InputStatus1::new(self.status.is_multiple_of(10), self.status.is_multiple_of(100)).value
            },
            _ => 0xff, // 0xB-0xF not available
        };

        if port >= 8 && port != 0xA {
            trace!("CGA: Read value 0x{val:02X} from port 0x{port:X}");
        }

        val
    }

    pub fn write_cga(&mut self, port: u8, val: u8) {
        if port >= 8 {
            trace!("CGA: Write value 0x{val:02X} to port 0x{port:X}");
        }

        match port {
            0 | 2 | 4 | 6 => self.crt.write_addr(val),
            1 | 3 | 5 | 7 => {
                self.crt.write(val);
                self.send_modeset();
            },
            8 => {
                info!("CGA: Selecting mode 0x{val:X} with registers: {:#X?}", self);
                self.mode_select.write(val);
                self.send_modeset();
            },
            9 => {
                self.cga_palette.write(val);
                self.send_modeset();
            },

            // Writing to 0xA writes the VGA feature control register instead
            0xA => self.feature_control.write(val),

            // 0xB-0xF not available
            _ => (),
        }
    }

    fn send_modeset(&mut self) {
        assert!(!self.crt.vertical_retrace_end.enable_vertical_interrupt());

        let s = if self.dispi.enable.vbe_enabled() {
            if self.dispi.virt_width < self.dispi.xres {
                self.dispi.virt_width = self.dispi.xres;
            }

            if self.dispi.virt_height < self.dispi.yres {
                self.dispi.virt_height = self.dispi.yres;
            }

            let stride = if self.dispi.bpp == 4 {
                self.dispi.virt_width / 8
            } else {
                self.dispi.virt_width * (self.dispi.bpp / 8)
            };
            let start_address = self.dispi.x_offset as u32 + self.dispi.y_offset as u32 * stride as u32;

            ModeSet {
                is_graphics: true,
                memory_addressing: match self.dispi.bpp {
                    4 => MemoryAddressing::Planar4,
                    8 => MemoryAddressing::Linear8,
                    15 => MemoryAddressing::Linear15,
                    16 => MemoryAddressing::Linear16,
                    24 => MemoryAddressing::Linear24,
                    32 => MemoryAddressing::Linear32,
                    other => panic!("invalid bpp: {other}"),
                },
                width: self.dispi.xres,
                height: self.dispi.yres,
                stride,

                palette: self.cga_palette.value(),
                enable_color: self.mode_select.value() & 0x04 != 0,
                enable_video: self.mode_select.value() & 0x08 != 0,
                enable_blink: self.mode_select.value() & 0x20 != 0,
                force_43_aspect_ratio: false,
                start_address,
                vga_palette: self.attribute.palette(),
                dac_palette: self.dac_palette,
            }
        } else {
            // Start address counts only one plane, so we should multiply by 4.
            let start_address = (((self.crt.start_address_high as u32) << 8) | self.crt.start_address_low as u32) * 4;

            let (mut width, mut height) = (self.crt.effective_width(), self.crt.effective_height());
            if !self.graphics.miscellaneous.graphics_mode() {
                width /= 8;
                height /= 16;
            }

            if self.graphics.graphics_mode.n256_color_mode() {
                width /= 2;
            }

            ModeSet {
                is_graphics: self.graphics.miscellaneous.graphics_mode(),
                memory_addressing: if self.graphics.graphics_mode.n256_color_mode() {
                    MemoryAddressing::Linear8
                } else if self.graphics.graphics_mode.shift_register_mode() {
                    MemoryAddressing::ShiftRegister
                } else {
                    MemoryAddressing::Planar4
                },
                width,
                height,
                stride: width * 2,
                force_43_aspect_ratio: true,

                palette: self.cga_palette.value(),
                enable_color: self.mode_select.value() & 0x04 != 0,
                enable_video: self.mode_select.value() & 0x08 != 0,
                enable_blink: self.mode_select.value() & 0x20 != 0,
                start_address,
                vga_palette: self.attribute.palette(),
                dac_palette: self.dac_palette,
            }
        };

        if self.last_modeset_sent != s {
            self.last_modeset_sent = s;

            info!("Sending modeset: {s:X?}");
            self.modeset.send(s).unwrap();
        }
    }

    pub fn video_memory(&self) -> VideoMemory {
        self.video_memory_ref.clone()
    }

    fn compute_vram_address(&self, offset: u32) -> (u32, [bool; 4]) {
        let offset = offset & 0x3ffff; // TODO: self.graphics.miscellaneous.odd_even()?
        // let offset = if self.graphics.miscellaneous.odd_even() {
        //     (offset & !1) | ((offset >> 16) & 1)
        // } else {
        //     offset
        // } & 0xffff;

        if self.sequencer.memory_mode.chain_4() {
            let mut maps = [false; 4];
            maps[offset as usize & 3] = true;
            (offset >> 2, maps)
        } else if !self.sequencer.memory_mode.disable_odd_even() {
            let mut maps = [false; 4];
            maps[(offset & 1) as usize] = true;
            maps[(offset & 1) as usize + 2] = true;

            (offset & !1, maps)
        } else {
            (offset & 0xffff, [true; 4])
        }
    }

    fn plane_offset_to_addr(offset: u32, plane: u8) -> u32 {
        assert!(plane < 4);
        offset * 4 + plane as u32
    }

    pub fn read_video_memory(&mut self, offset: u32) -> u8 {
        let (address, planes) = self.compute_vram_address(offset);

        // read all planes
        let m = self.video_memory.view();
        for (index, val) in self.latches.iter_mut().enumerate() {
            *val = m.read_byte(Self::plane_offset_to_addr(address, index as u8));
        }

        trace!("READ: {address} = {:02X?}", self.latches);

        match self.graphics.graphics_mode.read_mode() {
            graphics::ReadMode::Normal => {
                if self.sequencer.memory_mode.chain_4() {
                    self.latches[self.graphics.read_map_select.map_select().as_usize()]
                } else if !self.sequencer.memory_mode.disable_odd_even() {
                    self.latches[(self.graphics.read_map_select.map_select().as_usize() & 2) + planes[1] as usize]
                } else {
                    self.latches[self.graphics.read_map_select.map_select().as_usize()]
                }
            },
            graphics::ReadMode::ColorCompare => {
                let colors = self.graphics.color_compare.values();
                let compare_enabled = self.graphics.color_dont_care.values();
                let mut result = 0;

                for n in 0..8 {
                    let pixels = self.latches.map(|b| (b >> n) & 1 == 1);
                    let matches = pixels
                        .iter()
                        .copied()
                        .zip(colors)
                        .zip(compare_enabled)
                        .all(|((found, expected), compare_enabled)| !compare_enabled || found == expected);
                    result |= (matches as u8) << n;
                }

                result
            },
        }
    }

    pub fn write_video_memory(&mut self, offset: u32, val: u8) {
        let (address, maps) = self.compute_vram_address(offset);

        trace!(
            "WRITE: {address} = 0x{val:02X} (write mode: {:?})",
            self.graphics.graphics_mode.write_mode()
        );

        let set_reset_map = self.graphics.set_reset.values();
        let set_reset_enabled = self.graphics.enable_set_reset.values();
        let rotate_count = self.graphics.data_rotate.rotate_count();
        let val = val.rotate_right(match self.graphics.graphics_mode.write_mode() {
            WriteMode::WriteSrOrData | WriteMode::WriteSr => rotate_count.as_u32(),
            WriteMode::WriteLatches | WriteMode::OneBitPerMap => 0,
        });
        let map_mask = self.sequencer.map_mask.map_enabled();

        let m = self.video_memory.view();
        for ((plane, &latch), (&enable, map_mask)) in self.latches.iter().enumerate().zip(maps.iter().zip(map_mask)) {
            if enable && map_mask {
                let (val, bit_mask) = match self.graphics.graphics_mode.write_mode() {
                    WriteMode::WriteSrOrData => (
                        {
                            if set_reset_enabled[plane] {
                                if set_reset_map[plane] { 0xff } else { 0x00 }
                            } else {
                                val
                            }
                        },
                        self.graphics.bit_mask,
                    ),
                    WriteMode::WriteLatches => (latch, 0xff),
                    WriteMode::OneBitPerMap => (if (val >> plane) & 1 != 0 { 0xff } else { 0x00 }, self.graphics.bit_mask),
                    WriteMode::WriteSr => (if set_reset_map[plane] { 0xff } else { 0x00 }, val & self.graphics.bit_mask),
                };

                let val = match self.graphics.data_rotate.function_select() {
                    DataRotateFunction::Unmodified => val,
                    DataRotateFunction::And => val & latch,
                    DataRotateFunction::Or => val | latch,
                    DataRotateFunction::Xor => val ^ latch,
                };

                let val = (latch & !bit_mask) | (val & bit_mask);
                trace!("Writing {val:X} to {address:X}:{plane}");
                let addr = Self::plane_offset_to_addr(address, plane as u8);
                m.write_byte(addr, val);

                self.watcher.notify_dirty(addr as u64);
            }
        }
    }

    fn bar0_mask(&mut self) -> u32 {
        !(u32::MAX >> (self.video_memory.len() as u32).leading_zeros())
    }

    pub fn snapshot(&self) -> VgaSnapshot {
        VgaSnapshot {
            status: self.status,
            mode_select: self.mode_select.clone(),
            cga_palette: self.cga_palette.clone(),
            sequencer: self.sequencer.clone(),
            crt: self.crt.clone(),
            graphics: self.graphics.clone(),
            attribute: self.attribute.clone(),
            write_attribute_data: self.write_attribute_data,
            dispi: self.dispi.clone(),
            last_modeset_sent: self.last_modeset_sent,
            latches: self.latches,
            dac_palette: self.dac_palette,
            dac_palette_write_offset: self.dac_palette_write_offset,
            dac_palette_read_offset: self.dac_palette_read_offset,
            feature_control: self.feature_control.clone(),
            miscellaneous_output_register: self.miscellaneous_output_register,
            pci_header: self.pci_header,
            pci_rom_bar: self.pci_rom_bar.snapshot(),
            ddc: self.ddc.clone(),
            ddc_enabled: self.ddc_enabled,
            video_memory: self.video_memory.view().to_vec(),
            vram_mapped_range: self.vram_mapped_range.snapshot(),
            lfb_mapped_range: self.lfb_mapped_range.snapshot(),
        }
    }

    pub fn restore(&mut self, vga: VgaSnapshot, memory: &Mem32) {
        self.status = vga.status;
        self.mode_select = vga.mode_select;
        self.cga_palette = vga.cga_palette;
        self.sequencer = vga.sequencer;
        self.crt = vga.crt;
        self.graphics = vga.graphics;
        self.attribute = vga.attribute;
        self.write_attribute_data = vga.write_attribute_data;
        self.dispi = vga.dispi;
        self.last_modeset_sent = vga.last_modeset_sent;
        self.latches = vga.latches;
        self.dac_palette = vga.dac_palette;
        self.dac_palette_write_offset = vga.dac_palette_write_offset;
        self.dac_palette_read_offset = vga.dac_palette_read_offset;
        self.feature_control = vga.feature_control;
        self.miscellaneous_output_register = vga.miscellaneous_output_register;
        self.pci_header = vga.pci_header;
        self.pci_rom_bar.restore(vga.pci_rom_bar, &self.mem);
        self.vram_mapped_range.restore(vga.vram_mapped_range, memory);
        self.lfb_mapped_range.restore(vga.lfb_mapped_range, memory);
        self.ddc = vga.ddc;
        self.ddc_enabled = vga.ddc_enabled;

        // Make sure the UI is aware of the current display mode
        self.modeset.send(self.last_modeset_sent).unwrap();
        self.video_memory.view().write_slice(0, &vga.video_memory);

        self.update_mappings(memory);
    }
}

impl PciDevice for Vga {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        error!("Write register 0x{index:X} = 0x{val:X}");
        match self.pci_header.write(index, val) {
            Some(DeviceWriteEvent::Common(CommonWriteEvent::CommandStatus)) => {
                info!(
                    "Command/status written: command={:X?}, status={:X?}",
                    self.pci_header.common.command, self.pci_header.common.status
                );
            },
            // For now, we require BAR0 to fill an entire page (because that's efficient for our memory implementation).
            // TODO: At some point, we should allow these to be mapped to 256-byte regions.
            Some(DeviceWriteEvent::Bar(0)) => {
                self.pci_header.bar[0] &= self.bar0_mask();
                info!("BAR0 = {:X}", self.pci_header.bar[0]);
            },
            // No other BARs
            Some(DeviceWriteEvent::Bar(n)) => self.pci_header.bar[n] = 0,
            Some(DeviceWriteEvent::ExpansionRom) => {
                info!(
                    "Expansion ROM address changed: {:X}",
                    self.pci_header.expansion_rom_base_address
                );

                let mask = !(u32::MAX >> (self.pci_rom_bar.len_rounded_up_to_page_bound() as u32).leading_zeros());

                info!("Masking with 0x{mask:X}, because BIOS is 0x{:X} long", self.vga_bios.len());
                self.pci_header.expansion_rom_base_address =
                    (self.pci_header.expansion_rom_base_address & mask) | (self.pci_header.expansion_rom_base_address & 1);
                if self.pci_header.expansion_rom_base_address & 1 != 0 {
                    info!(
                        "Exposing VGA ROM at 0x{:X}",
                        self.pci_header.expansion_rom_base_address & mask
                    );
                    self.pci_rom_bar
                        .enable_and_set_addr(self.pci_header.expansion_rom_base_address & mask, &self.mem);
                } else {
                    info!("Unmapping VGA ROM");
                    self.pci_rom_bar.disable(&self.mem);
                }
            },
            Some(ev) => {
                error!("TODO: Handle write event: {ev:X?}");
            },
            None => (),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        if let Some(result) = self.pci_header.read(index) {
            info!("Read register 0x{index:X} = 0x{result:X}");

            result
        } else {
            error!("TODO: read outside VGA generic pci header: index 0x{index:X}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::channel;

    use sem86_arch::mem::{Mem32, Shm};

    use super::Vga;

    #[test]
    pub fn test_address_translation() {
        let (sender, _) = channel();
        let mem = Arc::new(Mem32::new(Arc::new(Shm::new("test", 0x1000))));
        let mut vga = Vga::new(sender, mem, Arc::new(Shm::new("vgabios", 0x1000)));

        // set map_mask = [true, true, false, false]
        vga.sequencer.write_addr(2);
        vga.sequencer.write(0b0011);

        // set bit_mask = 0xff
        vga.graphics.write_addr(8);
        vga.graphics.write(0xff);

        assert_eq!(vga.compute_vram_address(0x0), (0, [true, false, true, false]));
        assert_eq!(vga.compute_vram_address(0x1), (0, [false, true, false, true]));

        vga.write_video_memory(0x0, 0xff);
        assert_eq!(vga.video_memory.view().read_byte(0), 0xff);

        vga.write_video_memory(0x1, 0xee);
        assert_eq!(vga.video_memory.view().read_byte(1), 0xee);

        vga.write_video_memory(0x2, 0xdd);
        assert_eq!(vga.video_memory.view().read_byte(8), 0xdd);
    }
}
