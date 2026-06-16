use std::collections::VecDeque;
use std::sync::Arc;

use atapi::{AtapiPacket, compute_simple_toc};
use bilge::prelude::*;
use bitcode::{Decode, Encode};
use bytemuck::Zeroable;
use log::{debug, error, info, warn};
use sem86_arch::mem::{Mem32, Mmio};
use serde::{Deserialize, Serialize};

use super::DiskData;
use super::pci::DeviceWriteEvent;
use super::pic::DualIrqLine;
use super::ports::{PortError, PortIoData};
use super::reg::Reg8;
use crate::hw::HwMmio;
use crate::hw::ide::atapi::{AtapiInterruptReason, Direction, Phase};
use crate::hw::pci::{CommonPciHeader, GeneralDeviceHeader, PciCommandRegister, PciDevice};
use crate::hw::ports::WithIoSpace;
use crate::hw::storage::DiskDataSnapshot;

mod atapi;

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Serialize, Deserialize, Encode, Decode)]
struct Status {
    /// Indicates whether an error occurred during the execution of the last command.
    err: bool,

    /// Set once per disk revolution.
    idx: bool,

    /// When true, a correctable data error was encountered and corrected.
    corr: bool,

    /// DRQ: Indicates that the drive is ready to transfer a word or byte.
    ready_to_transmit: bool,

    /// Indicates whether the drive heads are settled over a track.
    /// Should always be true unless the drive is seeking.
    drive_seek_complete: bool,

    /// Indicates whether an error occurred while writing to the drive.
    drive_write_fault: bool,

    /// When true, the drive is capable of responding to a command.
    drive_ready: bool,

    /// When true, the CPU should not access the command block registers.
    /// When true, reading any of the command block registers should return the status register.
    busy: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self::new(false, false, false, false, true, false, false, true)
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Serialize, Deserialize, Encode, Decode)]
struct DeviceControl {
    reserved: u1,
    interrupt_disabled: bool,
    software_reset: bool,
    reserved: u1, // TODO: This should always be 1.
    reserved: u4,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
struct DriveAddr {
    /// 0 when drive 0 is selected, 1 otherwise.
    nds0: bool,

    /// 0 when drive 1 is selected, 1 otherwise.
    nds1: bool,

    /// XOR with 0b1111 to get the currently active head
    head_complement: u4,

    /// 'write gate': false when a write is in progress
    not_writing: bool,

    /// always in a high-impedance state
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
struct DriveHeadVal {
    hs: u4,
    drive: u1,
    reserved: u1,
    use_lba: bool,
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, FromBits, Serialize, Deserialize, Encode, Decode)]
struct Error {
    address_mark_not_found: bool,
    track_0_not_found: bool,
    command_aborted: bool,
    media_change_requested: bool,
    id_not_found: bool,
    media_changed: bool,
    uncorrectable_data_error: bool,
    bad_block_detected: bool,
}

impl Error {
    const NONE: Error = Error {
        value: 0,
    };
}

#[allow(unused)]
#[derive(Debug)]
struct Drive {
    head: u8,
    write_in_progress: bool,
    data: DiskData,
    geometry: Geometry,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct Geometry {
    head_count: u16,
    sector_size: u16,
    sectors_per_track: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DriveSnapshot {
    head: u8,
    write_in_progress: bool,
    data: DiskDataSnapshot,
    geometry: Geometry,
}

// const HEAD_COUNT: u16 = 16;
// const SECTOR_SIZE: u16 = 512;
// const SECTORS_PER_TRACK: u16 = 63;

impl Drive {
    const fn str<const N: usize>(s: &str) -> [u8; N] {
        let mut data = [0x20; N];
        let mut index = 0;
        let bytes = s.as_bytes();
        while index < bytes.len() {
            data[index ^ 1] = bytes[index];
            index += 1;
        }

        data
    }

    pub fn new(data: DiskData) -> Self {
        Drive {
            head: 0,
            write_in_progress: false,
            data,
            geometry: Geometry {
                head_count: 16,
                sector_size: 512,
                sectors_per_track: 63,
            },
        }
    }

    pub fn identify(&self) -> DriveId {
        let is_cd = self.is_cd();
        let mut general = GeneralConfig::zeroed();
        general.set_fixed_drive(true);
        if is_cd {
            general.set_removable_cartridge_drive(true);
            general.set_disk_transfer_rate_less_than_5mbps(true);
            general.set_disk_transfer_rate_more_than_10mbps(true);
            general.set_not_an_ata_device(true);
        }

        let geometry = self.geometry;
        let total_sector_count: u64 = (self.data.len() / geometry.sector_size as u64)
            .try_into()
            .expect("drive size should fit within 32-bit limits");
        let cylinder_count = total_sector_count / (geometry.head_count as u64 * geometry.sectors_per_track as u64);

        if is_cd {
            DriveId {
                general,
                firmware_revision: Self::str("ALPHA1"),
                model_number: Self::str("Generic 1234"),
                serial_number: Self::str("BXCD00001"), // TODO: Don't use Bochs' ID
                capabilities: Capabilities::new(0, true, true),
                doubleword_io_supported: 1,
                counts_correct: 0x3,          // ???
                multiword_dma_supported: 0x7, // TODO: | (mdma_mode << 8)
                pio_modes_supported: 1,
                pio_dma_cycle_time: [0xB4, 0xB4, 0x12C, 0xB4],
                reserved3: [0, 0, 30, 30, 0, 0, 0, 0, 0, 0, 0],
                ata_support: 0x1E,
                ..DriveId::zeroed()
            }
        } else {
            DriveId {
                general,
                cylinder_count: cylinder_count as u16,
                head_count: geometry.head_count,
                unformatted_bytes_per_track: geometry.sector_size * geometry.sectors_per_track,
                unformatted_bytes_per_sector: geometry.sector_size,
                sectors_per_track: geometry.sectors_per_track,
                firmware_revision: Self::str("FIRMWARE"),
                model_number: Self::str("Virtual IDE Drive"),
                serial_number: Self::str("BXHD00011"), // TODO: Don't use Bochs' ID
                capabilities: Capabilities::new(0, true, true),
                total_lba_sectors: total_sector_count as u32,
                buffer_type: 0x3,
                buffer_size: 0x200,
                ecc_byte_num: 0x04,
                maximum_sectors_transferred_per_interrupt: 0x10,
                doubleword_io_supported: 1,
                pio_data_transfer_cycle_timing_mode: 0x02,
                dma_data_transfer_cycle_timing_mode: 0x02,
                counts_correct: 0x7, // ???
                current_cylinder_count: cylinder_count as u16,
                current_head_count: geometry.head_count,
                current_sectors_per_track: geometry.sectors_per_track,
                current_sector_capacity: total_sector_count as u32,
                multiword_dma_supported: 0x7, // TODO: | (mdma_mode << 8)
                pio_modes_supported: 0,
                ata_support: 0x7E,
                various_support_flags: [0x4000, 0x7400, 0x4000, 0x4000, 0x7400, 0x4000],
                udma_mode: 0x3F, // TODO: | (udma_mode << 8)
                unknown_number: 0x6001,
                total_sector_count_48bit: total_sector_count,
                ..DriveId::zeroed()
            }
        }
    }

    fn is_cd(&self) -> bool {
        self.data.is_cd()
    }

    fn snapshot(&self) -> DriveSnapshot {
        DriveSnapshot {
            head: self.head,
            write_in_progress: self.write_in_progress,
            data: self.data.snapshot(),
            geometry: self.geometry,
        }
    }

    fn from_snapshot(snapshot: DriveSnapshot, current: Option<&mut Drive>) -> Self {
        println!("TODO: Load drive geometry from snapshot");
        Self {
            head: snapshot.head,
            write_in_progress: snapshot.write_in_progress,
            data: DiskData::from_snapshot(snapshot.data, current.map(|d| &mut d.data)),
            // TODO: These MUST be loaded from snapshot
            geometry: snapshot.geometry,
        }
    }
}

#[bitsize(16)]
#[derive(Copy, Clone, DebugBits, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C, packed)]
pub struct GeneralConfig {
    // TODO: Most of these bits are deprecated/obsolete, we should figure out which ones and just keep them at 0.
    reserved: u1,
    hard_sectored: bool,
    soft_sectored: bool,
    not_mfm_encoded: bool,
    head_switch_time_more_than_15usec: bool,
    spindle_motor_control_option_implemented: bool,
    fixed_drive: bool,
    removable_cartridge_drive: bool,
    disk_transfer_rate_less_than_5mbps: bool,
    disk_transfer_rate_5_to_10mbps: bool,
    disk_transfer_rate_more_than_10mbps: bool,
    rotational_speed_tolerance_more_than_half_a_percent: bool,
    data_strobe_offset_option_available: bool,
    track_offset_option_available: bool,
    format_speed_tolerance_gap_required: bool,
    not_an_ata_device: bool,
}

#[bitsize(16)]
#[derive(Copy, Clone, DebugBits, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C, packed)]
pub struct Capabilities {
    vendor_unique: u8,
    dma_supported: bool,
    lba_supported: bool,
    reserved: u6,
}

#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C, packed)]
pub struct DriveId {
    general: GeneralConfig,

    /// Number of cylinders in the default translation mode.
    cylinder_count: u16,
    reserved1: u16,

    /// Number of heads in the default translation mode.
    head_count: u16,

    /// Number of bytes per track in the default translation mode.
    unformatted_bytes_per_track: u16,

    /// Number of bytes per sector in the default translation mode.
    unformatted_bytes_per_sector: u16,

    /// Number of sectors per track in the default translation mode.
    sectors_per_track: u16,
    vendor_specific: [u16; 3],

    /// Right-justified serial number padded with spaces
    serial_number: [u8; 20],
    buffer_type: u16,

    /// The buffer size / 512.
    buffer_size: u16,

    /// 0x0000 = not specified
    ecc_byte_num: u16,

    /// Left-justified firmware revision padded with spaces.
    firmware_revision: [u8; 8],

    /// Left-justified model number padded with spaces.
    model_number: [u8; 40],

    /// 0x00 = read/write multiple commands not implemented
    maximum_sectors_transferred_per_interrupt: u8,
    vendor_specific2: u8,

    /// 0x0000 if not supported, 0x0001 if supported
    doubleword_io_supported: u16,

    capabilities: Capabilities,
    reserved2: u16,
    vendor_specific3: u8,
    pio_data_transfer_cycle_timing_mode: u8,
    vendor_specific4: u8,
    dma_data_transfer_cycle_timing_mode: u8,

    /// 0x0001 if unclear, 0x0001 if values reported in the following 4 words are correct.
    counts_correct: u16,
    current_cylinder_count: u16,
    current_head_count: u16,
    current_sectors_per_track: u16,
    current_sector_capacity: u32,

    multiple_sector_commands_support: u16,
    total_lba_sectors: u32,
    single_word_dma_supported: u8,
    single_word_dma_active: u8,
    multiword_dma_supported: u8,
    multiword_dma_active: u8,
    pio_modes_supported: u16,
    pio_dma_cycle_time: [u16; 4],
    reserved3: [u16; 11],
    ata_support: u16,
    minor_version_number: u16,
    various_support_flags: [u16; 6],
    udma_mode: u16,
    reserved4: [u16; 4],
    unknown_number: u16,
    reserved5: [u16; 6],
    total_sector_count_48bit: u64,
    reserved6: [u16; 24],
    vendor_specific5: [u16; 32],
    reserved: [u16; 96],
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
pub struct BusmasterPrimaryStatus {
    /// True when an operation is in progress.
    active: bool,

    /// True when an error occurred.
    error: bool,

    /// True when an interrupt is pending. Write 1 to this field to reset the interrupt.
    interrupt_ready: bool,

    reserved: u5,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum PendingCommand {
    Packet,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChannelState {
    /// In CHS mode, these are sector number, cylinder low and cylinder high.
    block_addr: [Reg8; 3],
    drive_and_head: DriveHeadVal,
    features: u8,
    sector_count: Reg8,
    error: Error,
    sector_buffer: VecDeque<u8>,
    reading_atapi_packet: bool,
    status: Status,
    device_control: DeviceControl,
    num_pending_sector_writes: usize,
    pending_dma_command: Option<PendingDmaCommand>,
    prd_addr: u32,
    busmaster_status: BusmasterPrimaryStatus,
    pending_command: Option<PendingCommand>,
    packet_responses_via_dma: bool,
    dma_status: Option<DmaCommand>,
    // TODO: Sense per drive
    sense: Sense,
    atapi_byte_index: u16,
    atapi_byte_block_size: u16,
}

#[derive(Debug)]
pub struct Channel {
    irq: DualIrqLine,
    drives: [Option<Drive>; 2],
    memory: Arc<Mem32>,
    inner: ChannelState,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChannelSnapshot {
    drives: [Option<DriveSnapshot>; 2],
    inner: ChannelState,
}

#[allow(unused)]
#[derive(Clone, Debug, Default, Serialize, Deserialize, Encode, Decode)]
pub struct Sense {
    sense_key: u8,
    information: [u8; 4],
    specific_information: [u8; 4],
    key_spec: [u8; 4],
    fruc: u8,
    asc: u8,
    ascq: u8,
}

impl Channel {
    fn new(irq: DualIrqLine, memory: Arc<Mem32>) -> Self {
        Self {
            irq,
            memory,
            drives: [None, None],
            inner: ChannelState {
                features: 0,
                error: Error::new(false, false, false, false, false, false, false, false),
                sector_count: Reg8::new(0),
                block_addr: [0; 3].map(Reg8::new),
                drive_and_head: DriveHeadVal::new(u4::new(0), u1::new(0), true),
                sector_buffer: VecDeque::new(),
                status: Status::default(),
                device_control: DeviceControl::new(true, false),
                num_pending_sector_writes: 0,
                prd_addr: 0,
                pending_dma_command: None,
                busmaster_status: BusmasterPrimaryStatus::new(false, false, false),
                pending_command: None,
                packet_responses_via_dma: false,
                dma_status: None,
                sense: Sense::default(),
                reading_atapi_packet: false,
                atapi_byte_index: 0,
                atapi_byte_block_size: 0,
            },
        }
    }

    fn generate_command_abort(&mut self) {
        self.inner.pending_command = None;
        self.inner.status.set_busy(false);
        self.inner.status.set_drive_ready(true);
        self.inner.status.set_err(true);
        self.inner.status.set_ready_to_transmit(false);
        self.inner.status.set_corr(false);

        self.inner.error = Error::default();
        self.inner.error.set_command_aborted(true);

        self.inner
            .sector_count
            .write(AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0)).as_u8());

        self.raise_interrupt();
    }

    fn current_drive(&self) -> usize {
        self.inner.drive_and_head.drive().as_usize()
    }

    fn current_block_addr(&self) -> BlockAddr {
        if self.inner.drive_and_head.use_lba() {
            BlockAddr::Lba(
                self.inner.block_addr[0].value() as u32
                    | ((self.inner.block_addr[1].value() as u32) << 8)
                    | ((self.inner.block_addr[2].value() as u32) << 16)
                    | (self.inner.drive_and_head.hs().as_u32() << 24),
            )
        } else {
            BlockAddr::Chs {
                sector: self.inner.block_addr[0].value(),
                cylinder: self.inner.block_addr[1].value() as u16 | ((self.inner.block_addr[2].value() as u16) << 8),
                head: self.inner.drive_and_head.hs().as_u8(),
            }
        }
    }

    fn advance_block_addr(&mut self) {
        let drive = self.drives[self.current_drive()].as_ref().unwrap();
        let addr = self.current_block_addr().next_sector(&drive.geometry);
        match addr {
            BlockAddr::Lba(lba) => {
                self.inner.block_addr[0].write(lba as u8);
                self.inner.block_addr[1].write((lba >> 8) as u8);
                self.inner.block_addr[2].write((lba >> 16) as u8);
                self.inner.drive_and_head.set_hs(u4::new((lba >> 24) as u8));
            },
            BlockAddr::Chs {
                cylinder,
                head,
                sector,
            } => {
                self.inner.block_addr[0].write(sector);
                self.inner.block_addr[1].write(cylinder as u8);
                self.inner.block_addr[2].write((cylinder >> 8) as u8);
                self.inner.drive_and_head.set_hs(u4::new(head));
            },
        }
    }

    pub fn write<S: PortIoData>(&mut self, addr: u8, val: S, mmio: &mut HwMmio<'_, '_>) -> Result<(), PortError> {
        let drive = &mut self.drives[self.current_drive()];
        match addr {
            // Data
            0x0 => {
                if self.inner.num_pending_sector_writes > 0 {
                    self.inner.sector_buffer.extend(val.le_bytes().as_ref());
                    if self.inner.sector_buffer.len() == 512 {
                        let geometry = drive.as_ref().unwrap().geometry;
                        info!("IDE: Writing sector {:?} with sector buffer", self.current_block_addr());

                        let start_addr = self.current_block_addr().as_byte_offset(&geometry);

                        let data = self.inner.sector_buffer.iter().copied().collect::<Vec<_>>();
                        let drive = self.drives[self.current_drive()].as_mut().unwrap();
                        drive.data.write_slice(start_addr, &data);

                        self.advance_block_addr();
                        self.inner.sector_buffer.clear();
                        self.inner.status.set_err(false);
                        self.raise_interrupt();

                        self.inner.num_pending_sector_writes -= 1;

                        self.inner
                            .status
                            .set_ready_to_transmit(self.inner.num_pending_sector_writes > 0);
                    }
                } else if let Some(cmd) = self.inner.pending_command {
                    match cmd {
                        PendingCommand::Packet => {
                            self.inner.sector_buffer.extend(val.le_bytes().as_ref());
                            if self.inner.sector_buffer.len() >= 12 {
                                debug!("Executing packet: {:02X?}", self.inner.sector_buffer);

                                let packet = self.inner.sector_buffer.drain(..12).collect::<Vec<_>>();

                                match AtapiPacket::parse(&packet.as_slice().try_into().unwrap()) {
                                    Ok(AtapiPacket::TestUnitReady {
                                        ..
                                    }) => {
                                        info!("ATAPI: Test Unit Ready");
                                        if drive.is_some() {
                                            self.finish_atapi_command_without_output();
                                        } else {
                                            panic!("TODO: Handle CDROM not present")
                                        }

                                        self.raise_interrupt();
                                    },
                                    Ok(
                                        packet @ AtapiPacket::Inquiry {
                                            allocation_length, ..
                                        },
                                    ) => {
                                        info!("Executing inquiry: {packet:02X?}");
                                        // TODO: Set various things (see Bochs' init_send_atapi_command)
                                        let mut buf = Vec::new();
                                        buf.extend_from_slice(&[0x05, 0x80, 0x00, 0x21, 31, 0x00, 0x00, 0x00]);
                                        buf.extend_from_slice(b"BOCHS   ");
                                        buf.extend_from_slice(b"Generic CD-ROM  "); // TODO: Last byte of this array could identify CD-ROM index
                                        buf.extend_from_slice(b"1.0 ");

                                        if buf.len() > allocation_length as usize {
                                            buf.drain(allocation_length as usize..);
                                        }

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(
                                        packet @ AtapiPacket::ModeSense {
                                            pc,
                                            page_code,
                                            allocation_length,
                                            ..
                                        },
                                    ) => {
                                        info!("ATAPI ModeSense: {packet:#?}");

                                        // PC:
                                        // Current = 0
                                        // Changable = 1
                                        // Defaults = 2
                                        // Saved = 3

                                        let mut buf = Vec::new();
                                        match (pc.as_u8(), page_code.as_u8()) {
                                            // Current/Default Capabilities & status
                                            (0 | 2, 0x2a) => {
                                                let size = 28;
                                                buf.push(((size + 6) >> 8) as u8);
                                                buf.push((size + 6) as u8);
                                                buf.push(0x12); // 0x70 if no CD-ROM present
                                                buf.resize(8, 0);
                                                buf.extend_from_slice(&[
                                                    0x2a,
                                                    0x12,
                                                    0x03,
                                                    0x00,
                                                    0x71,
                                                    3 << 5,
                                                    1 | (1 << 3) | (1 << 5),
                                                    0x00,
                                                    ((16 * 176u16) >> 8) as u8,
                                                    (16 * 176u16) as u8,
                                                    0,
                                                    2,
                                                    (512 >> 8) as u8,
                                                    512u16 as u8,
                                                    ((16 * 176u16) >> 8) as u8,
                                                    (16 * 176u16) as u8,
                                                    0,
                                                    0,
                                                    0,
                                                    0,
                                                ]);
                                            },

                                            (0, 0x01) => {
                                                let size = 8;
                                                buf.push(((size + 6) >> 8) as u8);
                                                buf.push((size + 6) as u8);
                                                buf.push(0x12); // 0x70 if no CD-ROM present
                                                buf.resize(8, 0);
                                                // ??? Error recovery data
                                                buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
                                            },

                                            _ => {
                                                error!(
                                                    "Not implemented: MODE SENSE PC={pc}, page_code=0x{page_code:X} -- returning error"
                                                );
                                                const SENSE_ILLEGAL_REQUEST: Error = Error {
                                                    value: 0x50,
                                                };
                                                const ASC_INV_FIELD_IN_CMD_PACKET: u8 = 0x24;
                                                self.finish_atapi_command_with_error(
                                                    SENSE_ILLEGAL_REQUEST,
                                                    ASC_INV_FIELD_IN_CMD_PACKET,
                                                );
                                                return Ok(())
                                            },
                                        }

                                        info!("Responding to MODE SENSE with {buf:02X?}");

                                        if buf.len() > allocation_length as usize {
                                            buf.drain(allocation_length as usize..);
                                        }

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(
                                        packet @ AtapiPacket::RequestSense {
                                            allocation_length, ..
                                        },
                                    ) => {
                                        info!("ATAPI RequestSense: {packet:#?}");

                                        let mut buf = vec![
                                            0xF0,
                                            0,
                                            self.inner.sense.sense_key,
                                            self.inner.sense.information[0],
                                            self.inner.sense.information[1],
                                            self.inner.sense.information[2],
                                            self.inner.sense.information[3],
                                            17 - 7,
                                            self.inner.sense.specific_information[0],
                                            self.inner.sense.specific_information[1],
                                            self.inner.sense.specific_information[2],
                                            self.inner.sense.specific_information[3],
                                            self.inner.sense.asc,
                                            self.inner.sense.ascq,
                                            self.inner.sense.fruc,
                                            self.inner.sense.key_spec[0],
                                            self.inner.sense.key_spec[1],
                                            self.inner.sense.key_spec[2],
                                        ];

                                        const SENSE_NONE: u8 = 0;
                                        const SENSE_UNIT_ATTENTION: u8 = 6;
                                        if self.inner.sense.sense_key == SENSE_UNIT_ATTENTION {
                                            self.inner.sense.sense_key = SENSE_NONE;
                                        }

                                        if buf.len() > allocation_length as usize {
                                            buf.drain(allocation_length as usize..);
                                        }

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(AtapiPacket::ReadToc {
                                        msf,
                                        format,
                                        starting_track,
                                        allocation_length,
                                        ..
                                    }) => {
                                        info!(
                                            "Responding with TOC for msf={msf}, format={format}, starting_track={starting_track}, allocation_length={allocation_length}"
                                        );

                                        let mut buf = compute_simple_toc(
                                            msf,
                                            format,
                                            starting_track,
                                            drive.as_ref().map(|d| d.data.len()).unwrap_or(0).try_into().unwrap(),
                                        )
                                        .unwrap();

                                        info!("TOC = {buf:02X?}");
                                        if buf.len() > allocation_length as usize {
                                            buf.drain(allocation_length as usize..);
                                        }

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(AtapiPacket::Read {
                                        lba,
                                        transfer_length,
                                    }) => {
                                        let start = lba as usize * 2048;
                                        let end = start + transfer_length as usize * 2048;
                                        info!("Reading from CD @ 0x{start:X} for 0x{:X} bytes", transfer_length * 2048);
                                        // TODO: Error if CD is not ready

                                        // TODO: Error if LBA is out of bounds

                                        // TODO: Crop transfer_length to make sure we remain inside LBA bounds

                                        let buf = drive.as_mut().unwrap().data.read_slice(start..end);

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(AtapiPacket::ReadSubchannel {
                                        msf: _,
                                        sub_q,
                                        data_format,
                                        track_number: _,
                                        allocation_length,
                                    }) => {
                                        let mut buf = vec![0, 0, 0, 0];

                                        if sub_q {
                                            if data_format == 2 || data_format == 3 {
                                                buf.resize(24, 0);
                                                buf[4] = data_format;
                                                if data_format == 3 {
                                                    // !?!?!???
                                                    buf[5] = 0x14;
                                                    buf[6] = 1;
                                                }

                                                buf[8] = 0;
                                            } else {
                                                error!("ReadSubchannel - data format {data_format}");
                                                const SENSE_ILLEGAL_REQUEST: Error = Error {
                                                    value: 0x50,
                                                };
                                                const ASC_INV_FIELD_IN_CMD_PACKET: u8 = 0x24;
                                                self.finish_atapi_command_with_error(
                                                    SENSE_ILLEGAL_REQUEST,
                                                    ASC_INV_FIELD_IN_CMD_PACKET,
                                                );
                                            }
                                        }

                                        if buf.len() > allocation_length as usize {
                                            buf.drain(allocation_length as usize..);
                                        }

                                        self.send_atapi_response(&buf, mmio);
                                    },
                                    Ok(AtapiPacket::ReadCdromCapacity) => {
                                        let capacity = (drive.as_mut().unwrap().data.len() / 2048) - 1;
                                        self.send_atapi_response(
                                            &[
                                                (capacity >> 24) as u8,
                                                (capacity >> 16) as u8,
                                                (capacity >> 8) as u8,
                                                capacity as u8,
                                                (2048 >> 24) as u8,
                                                (2048 >> 16) as u8,
                                                (2048 >> 8) as u8,
                                                0,
                                            ],
                                            mmio,
                                        );
                                    },
                                    Ok(AtapiPacket::GetEventStatusNotification {
                                        polled,
                                        request,
                                        allocation_length,
                                    }) => {
                                        let mut buf = Vec::new();

                                        if polled {
                                            if request == 1 << 4 {
                                                buf.push(0);
                                                buf.push(4);
                                                buf.push(4);
                                                buf.push(1 << 4);
                                                buf.push(0); // TODO: 0 = no change, 4 = media changed, 3 = media removed
                                                buf.push(1 << 1); // TODO: 0 = no media present
                                                buf.push(0);
                                                buf.push(0);
                                            } else {
                                                buf.push(0);
                                                buf.push(0);
                                                buf.push((1 << 7) | request);
                                                buf.push(1 << 4);
                                            }

                                            if buf.len() > allocation_length as usize {
                                                buf.drain(allocation_length as usize..);
                                            }

                                            self.send_atapi_response(&buf, mmio);
                                        } else {
                                            const SENSE_ILLEGAL_REQUEST: Error = Error {
                                                value: 0x50,
                                            };
                                            const ASC_INV_FIELD_IN_CMD_PACKET: u8 = 0x24;
                                            self.finish_atapi_command_with_error(
                                                SENSE_ILLEGAL_REQUEST,
                                                ASC_INV_FIELD_IN_CMD_PACKET,
                                            );
                                        }
                                    },
                                    Ok(AtapiPacket::StartStopUnit {
                                        command,
                                    }) => {
                                        // TODO: Remove disk when ejected
                                        warn!("TODO: Handle start/stop command: {command:?}");
                                        self.finish_atapi_command_without_output();
                                    },
                                    Ok(AtapiPacket::ReadDiscInfo) => {
                                        const SENSE_ILLEGAL_REQUEST: Error = Error {
                                            value: 0x50,
                                        };
                                        const ASC_INV_FIELD_IN_CMD_PACKET: u8 = 0x24;
                                        self.finish_atapi_command_with_error(SENSE_ILLEGAL_REQUEST, ASC_INV_FIELD_IN_CMD_PACKET);
                                    },
                                    Ok(AtapiPacket::EjectLock) => {
                                        // TODO: Implement ejection lock
                                        warn!("TODO: Handle eject lock");
                                        self.finish_atapi_command_without_output();
                                    },
                                    Ok(packet) => {
                                        panic!("TODO: PACKET: {packet:#?}")
                                    },
                                    Err(e) => error!("TODO: Handle error {e:?}"),
                                }
                            }
                        },
                    }
                }
            },

            // Features
            0x1 => self.inner.features = val.u8(),

            // Sector count
            0x2 => {
                self.inner.sector_count.write(val.u8());
            },

            // LBA byte 0-4 / CHS (sector, cylinder low, cylinder high, drive/head)
            0x3..=0x5 => self.inner.block_addr[addr as usize - 0x3].write(val.require_u8()?),

            // drive/head
            0x6 => {
                self.inner.drive_and_head = DriveHeadVal::from(val.u8());
                let has_drive = self.drives[self.current_drive()].is_some();
                self.inner.status.set_drive_ready(has_drive);
                info!(
                    "Selected drive/head: {:?} -- status: {:?}",
                    self.inner.drive_and_head, self.inner.status
                );
                self.inner.status.set_busy(false);

                // TODO: Can't figure out when Bochs does this, but it seems to be required
                self.inner.status.set_ready_to_transmit(false);
            },

            // Command
            0x7 => {
                if drive.is_some() {
                    self.execute_command(val.u8());
                } else {
                    warn!("executed command 0x{val:X} on missing drive");

                    self.inner.status.set_err(true);
                    self.inner.error.set_command_aborted(true);
                }
            },
            _ => panic!("Read from invalid IDE port 0x{addr:X}"),
        }

        Ok(())
    }

    fn atapi_byte_count(&self) -> u16 {
        (self.inner.block_addr[1].value() as u16) | ((self.inner.block_addr[2].value() as u16) << 8)
    }

    fn set_atapi_byte_count(&mut self, val: u16) {
        self.inner.block_addr[1].write(val as u8);
        self.inner.block_addr[2].write((val >> 8) as u8);
    }

    fn send_atapi_response(&mut self, buf: &[u8], mmio: &mut HwMmio<'_, '_>) {
        self.inner.reading_atapi_packet = true;
        if !self.atapi_byte_count().is_multiple_of(2) {
            self.set_atapi_byte_count(self.atapi_byte_count() - 1);
        }

        self.inner.atapi_byte_index = 0;
        self.inner.atapi_byte_block_size = self.atapi_byte_count();

        if self.inner.atapi_byte_block_size as usize > buf.len() {
            self.inner.atapi_byte_block_size = buf.len() as u16;
        }

        debug!("Sending ATAPI response: {buf:02X?}");
        self.inner.status.set_busy(false);
        self.inner.status.set_ready_to_transmit(true);
        self.inner.status.set_err(false);

        let ir = AtapiInterruptReason::new(Phase::Data, Direction::DeviceToHost, false, u5::new(0));
        assert_eq!(ir.as_u8(), 0x02);
        self.inner.sector_count.write(ir.as_u8());

        if self.inner.packet_responses_via_dma {
            debug!("Sending ATAPI response via DMA");
            self.inner.pending_dma_command = Some(PendingDmaCommand::FromBuffer {
                buf: buf.to_vec(),
            });
            self.try_handle_pending_dma(mmio);
            self.inner
                .sector_count
                .write(AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0)).as_u8());
        } else {
            debug!("Storing ATAPI response in sector buffer");
            self.inner.sector_buffer.extend(buf);
            self.inner.status.set_drive_seek_complete(true);
            self.inner.error = Error::default();
            self.raise_interrupt();
        }
    }

    fn finish_atapi_command_without_output(&mut self) {
        self.inner
            .sector_count
            .write(AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0)).as_u8());

        self.inner.status.set_busy(false);
        self.inner.status.set_drive_ready(true);
        self.inner.status.set_ready_to_transmit(false);
        self.inner.status.set_err(false);
    }

    fn finish_atapi_command_with_error(&mut self, e: Error, asc: u8) {
        self.inner
            .sector_count
            .write(AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0)).as_u8());

        // ???
        self.inner.sense.sense_key = e.value >> 4;
        self.inner.sense.asc = asc;
        self.inner.sense.ascq = 0;

        // Copy upper 4 bits of error register, which seem to be repurposed for the sense key.
        self.inner.error.set_id_not_found(e.id_not_found());
        self.inner.error.set_media_changed(e.media_changed());
        self.inner.error.set_uncorrectable_data_error(e.uncorrectable_data_error());
        self.inner.error.set_bad_block_detected(e.bad_block_detected());
        self.inner.status.set_busy(false);
        self.inner.status.set_drive_ready(true);
        self.inner.status.set_ready_to_transmit(false);
        self.inner.status.set_drive_write_fault(false);
        self.inner.status.set_err(true);

        self.raise_interrupt();
    }

    pub fn write_control<S: PortIoData>(&mut self, val: S) -> Result<(), PortError> {
        let drive = &mut self.drives[self.current_drive()];
        let has_drive = drive.is_some();
        self.inner.device_control.value = val.u8();
        info!("IDE: Device control = {:?}", self.inner.device_control);

        if self.inner.device_control.software_reset() {
            warn!("TODO: Lower IRQ");
            self.inner.status.set_drive_ready(false);
            self.inner.status.set_busy(has_drive);
            self.inner.sector_count.write(1);

            let signature = if let Some(d) = drive {
                if d.is_cd() {
                    [if self.inner.drive_and_head.use_lba() { 0 } else { 1 }, 0x14, 0xEB]
                } else {
                    [if self.inner.drive_and_head.use_lba() { 0 } else { 1 }, 0, 0]
                }
            } else {
                [if self.inner.drive_and_head.use_lba() { 0 } else { 1 }, 0xff, 0xff]
            };

            for (b, &s) in self.inner.block_addr.iter_mut().zip(signature.iter()) {
                b.write(s);
            }
        } else {
            self.inner.status.set_busy(false);
            self.inner.status.set_drive_ready(has_drive);
        }

        self.inner.status.set_drive_seek_complete(has_drive);

        Ok(())
    }

    pub fn read<S: PortIoData>(&mut self, addr: u8) -> Result<S, PortError> {
        match addr {
            // Data
            0x0 => S::from_le_bytes(|| {
                let mut data = [0; 8];
                for d in data[..S::SIZE].iter_mut() {
                    *d = self.inner.sector_buffer.pop_front().unwrap_or(0xff);
                }

                self.inner.status.set_ready_to_transmit(!self.inner.sector_buffer.is_empty());

                let drive = self.drives[self.current_drive()].as_mut();
                if self.inner.sector_buffer.is_empty() && drive.map(|d| d.is_cd()).unwrap_or(false) {
                    self.inner
                        .sector_count
                        .write(AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0)).as_u8());
                }

                if self.inner.reading_atapi_packet {
                    self.inner.atapi_byte_index += S::SIZE as u16;
                    if self.inner.sector_buffer.is_empty() {
                        // TODO: Only if this "marks the end of a data phase"

                        let ir = AtapiInterruptReason::new(Phase::Command, Direction::DeviceToHost, false, u5::new(0));
                        self.inner.sector_count.write(ir.as_u8());
                        self.inner.status.set_drive_ready(true);
                        self.inner.status.set_busy(false);
                        self.inner.status.set_ready_to_transmit(false);
                        self.inner.status.set_err(false);

                        info!("ATAPI Packet buffer fully read, raising interrupt");
                        self.raise_interrupt();
                        self.inner.reading_atapi_packet = false;
                    } else if self.inner.atapi_byte_index >= self.inner.atapi_byte_block_size {
                        self.inner.atapi_byte_index = 0;

                        let ir = AtapiInterruptReason::new(Phase::Data, Direction::DeviceToHost, false, u5::new(0));
                        self.inner.sector_count.write(ir.as_u8());
                        self.inner.status.set_drive_ready(true);
                        self.inner.status.set_busy(false);
                        self.inner.status.set_ready_to_transmit(true);

                        let remaining = self.inner.sector_buffer.len();
                        if self.inner.atapi_byte_block_size as usize > remaining {
                            info!(
                                "Only {remaining} bytes remaining, cropping byte block size (was: {}).",
                                self.inner.atapi_byte_block_size
                            );
                            self.inner.atapi_byte_block_size = remaining as u16;
                            self.set_atapi_byte_count(self.inner.atapi_byte_block_size);
                            assert!(self.inner.atapi_byte_block_size != 0);
                            info!("New byte block size: {}", self.inner.atapi_byte_block_size);
                        }

                        info!(
                            "Read {} bytes from ATAPI packet buffer, raising interrupt",
                            self.inner.atapi_byte_block_size
                        );
                        self.raise_interrupt();
                    }
                } else {
                    if !self.inner.sector_buffer.is_empty() && self.inner.sector_buffer.len().is_multiple_of(512) {
                        debug!("IDE: {} bytes remaining in sector buffer", self.inner.sector_buffer.len());
                        self.raise_interrupt();
                    }
                }

                data
            }),

            // Error register
            0x1 => S::from_u8(|| self.inner.error.value),

            // Sector count
            0x2 => S::from_u8(|| self.inner.sector_count.read()),

            // LBA byte 0-4 / CHS (sector, cylinder low, cylinder high)
            0x3..=0x5 => S::from_u8(|| self.inner.block_addr[addr as usize - 0x3].read()),

            // drive/head addr
            0x6 => S::from_u8(|| self.inner.drive_and_head.value),

            // Status/drive address
            0x7 => S::from_u8(|| {
                warn!("TODO: Lower IRQ");
                let result = self.inner.status.value;
                self.inner.status.set_err(false);

                // TODO: Maybe should be this?
                // S::from_u8(|| {
                //     let drive = &mut self.drives[self.current_drive()];
                //     let current_head = drive.as_ref().map(|d| d.head).unwrap_or(u4::new(0));
                //     let write_in_progress = drive.as_ref().map(|d| d.write_in_progress).unwrap_or(false);
                //     let da = DriveAddr::new(self.current_drive() != 0, self.current_drive() != 1, u4::new(0xf) ^ current_head, !write_in_progress);
                //     da.value
                // })

                result
            }),

            _ => panic!("Read from invalid IDE port 0x{addr:X}"),
        }
    }

    pub fn read_control<S: PortIoData>(&mut self) -> Result<S, PortError> {
        S::from_u8(|| self.inner.status.value)
    }

    fn execute_command(&mut self, val: u8) {
        warn!("TODO: Lower IRQ");
        self.inner.status.set_err(false);

        self.inner.reading_atapi_packet = false;
        info!("Executing command 0x{val:02X}");

        let drive = self.drives[self.current_drive()].as_mut().unwrap();
        match val {
            // Identify drive
            0xEC => {
                if drive.is_cd() {
                    info!("Resetting CD drive with magic values");

                    self.inner.block_addr[0].write(0);
                    self.inner.block_addr[1].write(0x14);
                    self.inner.block_addr[2].write(0xEB);

                    self.generate_command_abort();
                } else {
                    let id = drive.identify();
                    let bytes = bytemuck::cast::<_, [u8; 512]>(id);
                    info!("IDE: Identify drive response queued: {id:#?} = {bytes:02X?}");

                    self.inner.sector_buffer.clear();
                    self.inner.sector_buffer.extend(bytes);

                    self.inner.status.set_ready_to_transmit(true);
                    self.inner.status.set_err(false);
                    self.inner.status.set_drive_seek_complete(true);
                    self.raise_interrupt();
                }
            },

            0xA0 => {
                info!("Starting PACKET command");

                assert_eq!(self.inner.features & 0b10, 0, "PACKET features not supported: OVL");

                self.inner.packet_responses_via_dma = self.inner.features & 0b01 != 0;
                let ir = AtapiInterruptReason::new(Phase::Command, Direction::HostToDevice, false, u5::new(0));
                assert_eq!(ir.as_u8(), 1);
                self.inner.sector_count.write(ir.as_u8());
                self.inner.status.set_busy(false);
                self.inner.status.set_drive_write_fault(false);
                self.inner.status.set_ready_to_transmit(true);
                self.inner.sector_buffer.clear();
                self.inner.pending_command = Some(PendingCommand::Packet);
            },

            0xA1 => {
                if drive.is_cd() {
                    info!("Identifying CD drive");

                    self.inner
                        .sector_count
                        .write(AtapiInterruptReason::new(Phase::Data, Direction::DeviceToHost, false, u5::new(0)).as_u8());

                    let id = drive.identify();
                    let bytes = bytemuck::cast::<_, [u8; 512]>(id);
                    info!("IDE: Identify drive response queued: {id:#?} = {bytes:02X?}");
                    self.inner.sector_buffer.clear();
                    self.inner.sector_buffer.extend(bytes);
                    self.inner.status.set_ready_to_transmit(true);
                    self.inner.status.set_err(false);
                    self.inner.status.set_drive_seek_complete(true);
                    self.raise_interrupt();
                } else {
                    self.inner.status.set_err(true);
                    self.inner.error = Error::new(false, false, true, false, false, false, false, false);
                }
            },

            0x08 => {
                if drive.is_cd() {
                    info!("Resetting CD drive");

                    self.inner.block_addr[0].write(0);
                    self.inner.block_addr[1].write(0x14);
                    self.inner.block_addr[2].write(0xEB);

                    self.inner.sector_buffer.clear();

                    self.inner.status.set_drive_ready(false);
                    self.inner.status.set_drive_seek_complete(false);
                    self.inner.status.set_drive_write_fault(false);
                    self.inner.status.set_ready_to_transmit(false);
                    self.inner.status.set_busy(false);
                    self.inner.status.set_corr(false);
                    self.inner.error.set_bad_block_detected(false);
                } else {
                    self.inner.status.set_err(true);
                    self.inner.error = Error::new(false, false, true, false, false, false, false, false);
                }
            },

            // Calibrate
            0x10 => {
                self.inner.error = Error::from(0);
                self.inner.block_addr[1].write(0);
                self.inner.block_addr[2].write(0);
                self.inner.status.set_drive_ready(true);
                self.inner.status.set_drive_seek_complete(true);
                self.inner.status.set_ready_to_transmit(false);
                self.raise_interrupt();
            },

            // Set features
            0xEF => {
                match self.inner.features {
                    // Disable write cache
                    0x82 => info!("IDE: Disabling write cache"),
                    other => error!("IDE: feature 0x{other:X} not implemented"),
                }

                self.inner.status.set_err(false);
                self.inner.error = Error::default();
                self.raise_interrupt();
            },

            // Seek
            0x70 => {
                info!("IDE: Seek to {:?}", self.current_block_addr());
                self.inner.status.set_drive_seek_complete(true);
                self.inner.status.set_err(false);
                self.inner.error = Error::default();
                self.raise_interrupt();
            },

            // Format track
            // The ATA specification allows us to do nothing here
            0x50 => {
                self.inner.status.set_err(false);
                self.inner.error = Error::default();
                self.raise_interrupt();
            },

            // Write sectors
            0x30 | 0xC5 => {
                // TODO: 0xC5 - write multiple should only generate an interrupt every N sectors (specified by 0xC6 - set multiple)
                let num_sectors = self.sector_count();

                info!(
                    "IDE: Writing {num_sectors} sector(s) starting at {:?}",
                    self.current_block_addr()
                );
                self.inner.num_pending_sector_writes = num_sectors;

                self.inner.status.set_err(false);
                self.inner.status.set_ready_to_transmit(true);
                self.inner.error = Error::default();
            },

            // Read sectors
            0xC6 => error!("TODO: Set multiple"),
            0x20 | 0x40 | 0xC4 => {
                if drive.is_cd() {
                    self.generate_command_abort();
                } else {
                    // TODO: 0xC4 - read multiple should only generate an interrupt every N sectors (specified by 0xC6 - set multiple)
                    let geometry = drive.geometry;
                    let num_sectors = self.sector_count();

                    info!(
                        "IDE: Reading {num_sectors} sector(s) starting at {:?}",
                        self.current_block_addr()
                    );

                    let start_addr = self.current_block_addr().as_byte_offset(&geometry);
                    let num_bytes = num_sectors as u64 * 512;
                    let end_addr = start_addr + num_bytes;

                    info!("Byte range: 0x{start_addr:X}..0x{end_addr:X}");

                    let drive = self.drives[self.current_drive()].as_mut().unwrap();
                    if end_addr > drive.data.len() {
                        // TODO: Set current block addr to last valid sector
                        self.inner.status.set_err(true);
                        self.inner.error = Error::new(true, false, false, false, false, false, false, false);
                    } else {
                        let bytes_read = &drive.data.read_slice(start_addr as usize..end_addr as usize);
                        let words_read = bytemuck::cast_slice(bytes_read);

                        if val != 0x40 {
                            self.inner.sector_buffer.clear();
                            self.inner.sector_buffer.extend(words_read);
                            self.inner.status.set_ready_to_transmit(true);
                        }

                        self.inner.status.set_err(false);
                        self.inner.status.set_drive_seek_complete(true);
                        self.inner.error = Error::default();
                    }

                    self.raise_interrupt();
                }
            },

            0x25 | 0xC8 | 0xC9 => {
                // TODO: 0x25: lba48 addressing
                let geometry = drive.geometry;
                let num_sectors = self.sector_count();

                info!(
                    "IDE: Read {num_sectors} sector(s) via DMA starting at {:?}",
                    self.current_block_addr()
                );

                let start_addr = self.current_block_addr().as_byte_offset(&geometry);
                let num_bytes = num_sectors as u64 * 512;
                let end_addr = start_addr + num_bytes;

                info!("Byte range: 0x{start_addr:X}..0x{end_addr:X}");

                let drive = self.drives[self.current_drive()].as_mut().unwrap();
                if end_addr > drive.data.len() {
                    // TODO: Set current block addr to last valid sector
                    self.inner.status.set_err(true);
                    self.inner.error = Error::new(true, false, false, false, false, false, false, false);
                } else {
                    self.inner.pending_dma_command = Some(PendingDmaCommand::Read {
                        start: start_addr,
                        len: num_bytes,
                    });
                }
            },

            0x35 | 0xCA | 0xCB => {
                // TODO: 0x35: implement lba48 addressing
                let geometry = drive.geometry;
                let num_sectors = self.sector_count();

                info!(
                    "IDE: Write {num_sectors} sector(s) via DMA starting at {:?}",
                    self.current_block_addr()
                );

                let start_addr = self.current_block_addr().as_byte_offset(&geometry);
                let num_bytes = num_sectors as u64 * 512;
                let end_addr = start_addr + num_bytes;

                info!("Byte range: 0x{start_addr:X}..0x{end_addr:X}");

                let drive = self.drives[self.current_drive()].as_mut().unwrap();
                if end_addr > drive.data.len() {
                    // TODO: Set current block addr to last valid sector
                    self.inner.status.set_err(true);
                    self.inner.error = Error::new(true, false, false, false, false, false, false, false);
                } else {
                    self.inner.pending_dma_command = Some(PendingDmaCommand::Write {
                        start: start_addr,
                        len: num_bytes,
                    });
                }
            },

            0x91 => {
                drive.geometry.sectors_per_track = self.inner.sector_count.read() as u16;
                drive.geometry.head_count = self.inner.drive_and_head.hs().as_u16() + 1;
                // TODO: Also set cylinder count?

                self.inner.status.set_err(false);
                self.inner.status.set_drive_ready(true);
                self.inner.error = Error::NONE;
                self.raise_interrupt();
            },

            // TODO: We should report 0x94 & 0x95 as not supported instead of silently accepting them.
            0x94 => {
                warn!("TODO: Standby immediately");
                self.inner.status.set_err(false);
                self.inner.status.set_drive_ready(true);
                self.inner.error = Error::NONE;
                self.raise_interrupt();
            },
            0x95 => {
                warn!("TODO: Idle immediately");
                self.inner.status.set_err(false);
                self.inner.status.set_drive_ready(true);
                self.inner.error = Error::NONE;
                self.raise_interrupt();
            },

            0xE0 | 0xE1 | 0xE7 | 0xEA => {
                info!("'Flushing' caches");
                self.inner.status.set_err(false);
                self.inner.status.set_drive_ready(true);
                self.inner.error = Error::NONE;
                self.raise_interrupt();
            },

            0xF5 => {
                warn!("TODO: Security Freeze Lock");
                self.inner.status.set_err(false);
                self.inner.status.set_drive_ready(true);
                self.inner.error = Error::NONE;
                self.raise_interrupt();
            },

            _ => error!("TODO: Handle IDE command {val:X} {self:#X?}"),
        }
    }

    fn sector_count(&mut self) -> usize {
        if self.inner.sector_count.value() == 0 {
            0x100
        } else {
            self.inner.sector_count.value() as usize
        }
    }

    fn raise_interrupt(&mut self) {
        // TODO: Only for channel 0 and 1
        self.inner.busmaster_status.set_interrupt_ready(true);

        if !self.inner.device_control.interrupt_disabled() {
            debug!("Raising IDE interrupt with status {:?}", self.inner.status);
            self.irq.pulse();
        }
    }

    pub fn read_busmaster<S: PortIoData>(&mut self, addr: u8) -> Result<S, PortError> {
        match addr {
            // Command register
            0x0 => S::from_u8(|| {
                error!("TODO: Read IDE busmaster command register");
                0
            }),
            0x1 => S::from_u8(|| 0xff),
            // Status register
            0x2 => S::from_u8(|| {
                info!("Read IDE busmaster status register = {:?}", self.inner.busmaster_status);
                self.inner.busmaster_status.value
            }),
            0x3 => S::from_u8(|| 0xff),
            // PRD address
            0x4..=0x7 => S::from_u32(addr & 3, || self.inner.prd_addr),
            _ => S::from_u8(|| 0xff),
        }
    }

    pub fn write_busmaster<S: PortIoData>(&mut self, addr: u8, val: S, mmio: &mut HwMmio) -> Result<(), PortError> {
        match addr {
            // Command register
            0x0 => {
                let command = DmaCommand::from(val.u8());
                info!("IDE busmaster command register = {command:?}");
                self.inner.dma_status = Some(command);

                self.try_handle_pending_dma(mmio);
            },
            0x1 => {
                val.require_u8()?;
            },
            // Status register
            0x2 => {
                let status = BusmasterPrimaryStatus::from(val.u8());
                info!("Wrote IDE busmaster primary status register = {status:?}");
                if status.interrupt_ready() {
                    self.inner.busmaster_status.set_interrupt_ready(false);
                    info!("interrupt_ready reset");
                }
            },
            0x3 => {
                val.require_u8()?;
            },
            // PRD address
            0x4 => {
                let addr = val.u32();
                info!("IDE busmaster primary PRD table address = 0x{addr:02X}");
                self.inner.prd_addr = addr;
            },
            _ => {
                error!("TODO: Write IDE busmaster at addr 0x{addr:04X} with 0x{val:X}");
            },
        }

        Ok(())
    }

    fn try_handle_pending_dma(&mut self, mmio: &mut HwMmio<'_, '_>) {
        if let Some(command) = self.inner.dma_status
            && command.start()
            && let Some(pending_dma_command) = self.inner.pending_dma_command.take()
        {
            self.inner.dma_status = None;
            let drive = self.drives[self.current_drive()].as_mut().unwrap();
            match pending_dma_command {
                PendingDmaCommand::Read {
                    start,
                    len,
                } => {
                    let end_addr = start + len;
                    let bytes_read = drive.data.read_slice(start as usize..end_addr as usize);
                    self.perform_dma_read(&bytes_read, mmio);
                },
                PendingDmaCommand::Write {
                    start,
                    len,
                } => {
                    let mut lba = start;
                    let end_addr = start + len;
                    let mut prd_addr = self.inner.prd_addr;
                    while lba < end_addr {
                        let mut prd = [0; 8];
                        self.memory.read_physical_slice(prd_addr, &mut prd, mmio).unwrap();
                        let prd = Prd::from(u64::from_le_bytes(prd));

                        info!("PRD: {prd:X?}");

                        let mut buf = vec![0; prd.computed_size()];
                        self.memory.read_physical_slice(prd.addr(), &mut buf, mmio).unwrap();
                        drive.data.write_slice(lba, &buf);
                        lba += buf.len() as u64;

                        if prd.eop() || prd.jmp() {
                            todo!("PRD: {prd:?}");
                        }

                        if prd.eot() {
                            break
                        }

                        prd_addr += 8;
                    }

                    self.inner.status.set_ready_to_transmit(false);
                    self.inner.status.set_err(false);
                    self.inner.status.set_drive_seek_complete(true);
                    self.inner.error = Error::default();
                    self.raise_interrupt();
                    // TODO: Use PCI INTA#
                },
                PendingDmaCommand::FromBuffer {
                    buf,
                } => {
                    self.perform_dma_read(&buf, mmio);
                }, // None => {
                   //     error!("Started DMA without any pending command");
                   //     self.busmaster_status.set_error(true);
                   //     self.busmaster_status.set_active(false);
                   //     self.busmaster_status.set_interrupt_ready(true);
                   //     self.raise_interrupt();
                   // },
            }
        }
    }

    fn perform_dma_read(&mut self, mut bytes: &[u8], mmio: &mut impl Mmio) {
        let mut prd_addr = self.inner.prd_addr;
        while !bytes.is_empty() {
            let mut prd = [0; 8];
            self.memory.read_physical_slice_no_mmio(prd_addr, &mut prd);
            let prd = Prd::from(u64::from_le_bytes(prd));

            info!("PRD: {prd:X?}");

            let (slice, rest) = if bytes.len() > prd.computed_size() {
                (&bytes[..prd.computed_size()], &bytes[prd.computed_size()..])
            } else {
                (bytes, &[] as &[_])
            };

            // TODO: Claim pages as dirty before we execute the DMA read
            self.memory.write_physical_slice(prd.addr(), slice, mmio).unwrap();
            bytes = rest;

            if prd.eop() || prd.jmp() {
                todo!("PRD: {prd:?}");
            }

            if prd.eot() {
                break
            }

            prd_addr += 8;
        }

        self.inner.status.set_ready_to_transmit(false);
        self.inner.status.set_err(false);
        self.inner.status.set_drive_seek_complete(true);
        self.inner.error = Error::default();
        self.inner.busmaster_status.set_interrupt_ready(true);
        self.raise_interrupt();
    }

    fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            drives: std::array::from_fn(|n| self.drives[n].as_ref().map(|d| d.snapshot())),
            inner: self.inner.clone(),
        }
    }

    fn restore(&mut self, channel: ChannelSnapshot) {
        for (drive, snapshot) in self.drives.iter_mut().zip(channel.drives) {
            let new_drive = snapshot.map(|d| Drive::from_snapshot(d, drive.as_mut()));
            *drive = new_drive;
        }

        self.inner = channel.inner;
    }
}

#[derive(Debug)]
pub struct Ide {
    pub(crate) channels: [Channel; 2],
    pci_header: GeneralDeviceHeader,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct IdeSnapshot {
    channels: [ChannelSnapshot; 2],
    pci_header: GeneralDeviceHeader,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub enum PendingDmaCommand {
    Read { start: u64, len: u64 },
    Write { start: u64, len: u64 },
    FromBuffer { buf: Vec<u8> },
}

#[derive(Copy, Clone, Debug)]
pub enum BlockAddr {
    /// The linear block address, which is simply the index of the sector.
    Lba(u32),
    Chs {
        cylinder: u16,
        head: u8,
        sector: u8,
    },
}

impl BlockAddr {
    pub fn as_byte_offset(&self, geometry: &Geometry) -> u64 {
        match *self {
            BlockAddr::Lba(n) => n as u64 * geometry.sector_size as u64,
            BlockAddr::Chs {
                cylinder,
                head,
                sector,
            } => {
                // LBA = (((C * NUM_HEADS) + H) * SECTORS_PER_TRACK)+(S−1)
                let cylinder_index = cylinder as u64;
                let track_index = cylinder_index * geometry.head_count as u64 + head as u64;
                let sector_index = track_index * geometry.sectors_per_track as u64 + (sector - 1) as u64;

                sector_index * geometry.sector_size as u64
            },
        }
    }

    pub fn next_sector(&self, geometry: &Geometry) -> BlockAddr {
        match *self {
            BlockAddr::Lba(n) => BlockAddr::Lba(n + 1),
            BlockAddr::Chs {
                cylinder,
                head,
                sector,
            } => {
                let sector = (sector % geometry.sectors_per_track as u8) + 1;
                let head = (head + (sector == 1) as u8) % geometry.head_count as u8;
                let cylinder = cylinder + (head == 0) as u16;

                BlockAddr::Chs {
                    cylinder,
                    head,
                    sector,
                }
            },
        }
    }
}

const BAR_MASK: u32 = !0xf;

impl Ide {
    pub fn new(irq_primary: DualIrqLine, irq_secondary: DualIrqLine, memory: Arc<Mem32>) -> Self {
        Self {
            channels: [
                Channel::new(irq_primary, memory.clone()),
                Channel::new(irq_secondary, memory.clone()),
            ],
            pci_header: GeneralDeviceHeader {
                common: CommonPciHeader {
                    vendor_id: 0x8086,
                    device_id: 0x7010,
                    command: PciCommandRegister::from(0),
                    status: 0,
                    revision_id: 0,
                    class_code: 0x01,
                    subclass: 0x01,
                    prog_if: 0x80,
                    cache_line_size: 0,
                    latency_timer: 0,
                    header_type: 0,
                    bist: 0,
                },
                bar: [0; 6],
                cardbus_cis_pointer: 0,
                subsystem_vendor_id: 0,
                subsystem_id: 0,
                expansion_rom_base_address: 0,
                capabilities_pointer: 0,
                reserved1: [0; 3],
                reserved2: 0,
                interrupt_line: 0,
                interrupt_pin: 0,
                min_grant: 0,
                max_latency: 0,
            },
        }
    }

    pub fn set_disk(&mut self, channel: usize, drive: usize, data: Option<DiskData>) {
        self.channels[channel].drives[drive] = data.map(Drive::new)
    }

    fn base_addresses(&self) -> BaseAddresses {
        let prog_if = ProgIf::from(self.pci_header.common.prog_if);
        BaseAddresses {
            primary_channel: if prog_if.primary_channel_pci_supported() && prog_if.primary_channel_is_native() {
                (self.pci_header.bar[0] & BAR_MASK) as u16
            } else {
                0x1F0
            },
            primary_channel_control: if prog_if.primary_channel_pci_supported() && prog_if.primary_channel_is_native() {
                ((self.pci_header.bar[1] & BAR_MASK) as u16) + 2
            } else {
                0x3F6
            },
            secondary_channel: if prog_if.secondary_channel_pci_supported() && prog_if.secondary_channel_is_native() {
                (self.pci_header.bar[2] & BAR_MASK) as u16
            } else {
                0x170
            },
            secondary_channel_control: if prog_if.secondary_channel_pci_supported() && prog_if.secondary_channel_is_native() {
                ((self.pci_header.bar[3] & BAR_MASK) as u16) + 2
            } else {
                0x376
            },
            // TODO: Conditional of prog_if bit 3 or 7?
            busmaster: (self.pci_header.bar[4] & BAR_MASK) as u16,
        }
    }

    pub fn snapshot(&self) -> IdeSnapshot {
        IdeSnapshot {
            channels: std::array::from_fn(|n| self.channels[n].snapshot()),
            pci_header: self.pci_header,
        }
    }

    pub fn restore(&mut self, ide: IdeSnapshot) {
        for (c, s) in self.channels.iter_mut().zip(ide.channels) {
            c.restore(s);
        }

        self.pci_header = ide.pci_header;
    }
}

impl PciDevice for Ide {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        info!("Write PCI register 0x{index:X} = 0x{val:X}");
        let Some(ev) = self.pci_header.write(index, val) else { return };

        let prog_if = ProgIf::from(self.pci_header.common.prog_if);
        match ev {
            DeviceWriteEvent::Common(_) => (),
            DeviceWriteEvent::Bar(n @ 0..2) if prog_if.primary_channel_is_native() => {
                self.pci_header.bar[n] = (self.pci_header.bar[n] & BAR_MASK) | 1;
                info!("BAR{n} = {:X}", self.pci_header.bar[n]);
            },

            DeviceWriteEvent::Bar(n @ 2..4) if prog_if.secondary_channel_is_native() => {
                self.pci_header.bar[n] = (self.pci_header.bar[n] & BAR_MASK) | 1;
                info!("BAR{n} = {:X}", self.pci_header.bar[n]);
            },
            DeviceWriteEvent::Bar(n @ 4) => {
                self.pci_header.bar[n] = (self.pci_header.bar[n] & BAR_MASK) | 1;
                info!("BAR{n} = {:X}", self.pci_header.bar[n]);
            },
            DeviceWriteEvent::Bar(n) => self.pci_header.bar[n] = 0,
            _ => (),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        let val = self.pci_header.read(index).unwrap_or_else(|| match index {
            0x10 => {
                // TODO: Each byte represents one of the channels (0-0, 0-1, 1-0, 1-1) -- highest bit indicates DMA enabled. Must be set to 1 if the channel contains a drive.
                error!("TODO: Proper implementation for PCI IDETIM register 0x10");
                0x8000_8000
            },
            _ => {
                error!("Tried to read nonexistant PCI IDE register #{index}");
                0
            },
        });

        info!("Read PCI register 0x{index:X} = 0x{val:X}");
        val
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits, Serialize, Deserialize, Encode, Decode)]
struct DmaCommand {
    start: bool,
    reserved: u2,
    write: bool,
    reserved: u4,
}

#[bitsize(8)]
#[derive(FromBits)]
struct ProgIf {
    primary_channel_is_native: bool,
    primary_channel_pci_supported: bool,
    secondary_channel_is_native: bool,
    secondary_channel_pci_supported: bool,
    reserved: u3,
    bmdma_supported: bool,
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
struct Prd {
    addr: u32,
    size: u16,
    reserved: u8,
    reserved: u5,
    jmp: bool,
    eop: bool,
    eot: bool,
}

impl Prd {
    pub fn computed_size(&self) -> usize {
        if self.size() == 0 { 0x1_0000 } else { self.size() as usize }
    }
}

struct BaseAddresses {
    primary_channel: u16,
    primary_channel_control: u16,
    secondary_channel: u16,
    secondary_channel_control: u16,
    busmaster: u16,
}

impl WithIoSpace for Ide {
    fn try_read<S: PortIoData>(&mut self, addr: u16, _mmio: &mut HwMmio) -> Option<Result<S, PortError>> {
        let a = self.base_addresses();
        if addr & !7 == a.primary_channel {
            Some(self.channels[0].read((addr & 7) as u8))
        } else if addr == a.primary_channel_control {
            Some(self.channels[0].read_control())
        } else if a.busmaster != 0 && addr & !0xF == a.busmaster {
            Some(self.channels[((addr >> 3) & 1) as usize].read_busmaster((addr & 7) as u8))
        } else if addr & !7 == a.secondary_channel {
            Some(self.channels[1].read((addr & 7) as u8))
        } else if addr == a.secondary_channel_control {
            Some(self.channels[1].read_control())
        } else {
            None
        }
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, mmio: &mut HwMmio) -> Option<Result<(), PortError>> {
        let a = self.base_addresses();
        if addr & !7 == a.primary_channel {
            debug!("Primary IDE: Writing 0x{val:X} to 0x{addr:02X}");
            Some(self.channels[0].write((addr & 7) as u8, val, mmio))
        } else if addr == a.primary_channel_control {
            debug!("Primary IDE control: Writing 0x{val:X} to 0x{addr:02X}");
            Some(self.channels[0].write_control(val))
        } else if a.busmaster != 0 && addr & !0xF == a.busmaster {
            let channel = ((addr >> 3) & 1) as usize;
            debug!(
                "{} IDE Busmaster: Writing 0x{val:X} to 0x{addr:02X}",
                ["Primary", "Secondary"][channel]
            );
            Some(self.channels[channel].write_busmaster((addr & 7) as u8, val, mmio))
        } else if addr & !7 == a.secondary_channel {
            debug!("Secondary IDE: Writing 0x{val:X} to 0x{addr:02X}");
            Some(self.channels[1].write((addr & 7) as u8, val, mmio))
        } else if addr == a.secondary_channel_control {
            debug!("Secondary IDE control: Writing 0x{val:X} to 0x{addr:02X}");
            Some(self.channels[1].write_control(val))
        } else {
            None
        }
    }
}
