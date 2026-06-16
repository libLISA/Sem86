use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, Ordering};

use arrayvec::ArrayVec;
use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{error, info};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

use super::cmos::{DataRate, DisketteMediaState, DisketteType};
use super::pic::DualIrqLine;
use super::{DiskData, HwMmio};
use crate::hw::storage::DiskDataSnapshot;

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
struct DigitalOutput {
    drive_select: u1,
    reserved: u1,
    enabled: bool,
    dma_enabled: bool,
    motor_enable: [bool; 2],
    reserved: u2,
}

enum CmdId {
    ReadData,
    ReadDeletedData,
    WriteData,
    WriteDeletedData,
    ReadTrack,
    ReadId,
    FormatTrack,
    ScanLow,
    ScanLowOrEqual,
    ScanHighOrEqual,
    Recalibrate,
    SenseInterruptStatus,
    Specify,
    SenseDriveStatus,
    Seek,
    Invalid,
}

impl CmdId {
    pub fn total_len(&self) -> usize {
        match self {
            CmdId::ReadData
            | CmdId::ReadDeletedData
            | CmdId::WriteData
            | CmdId::WriteDeletedData
            | CmdId::ReadTrack
            | CmdId::ScanLow
            | CmdId::ScanLowOrEqual
            | CmdId::ScanHighOrEqual => 9,
            CmdId::FormatTrack => 6,
            CmdId::Specify | CmdId::Seek => 3,
            CmdId::ReadId | CmdId::Recalibrate | CmdId::SenseDriveStatus => 2,
            CmdId::SenseInterruptStatus | CmdId::Invalid => 1,
        }
    }

    pub fn from_byte(b: u8) -> Self {
        let mapping = [
            (0b11111, 0b00110, Self::ReadData),
            (0b11111, 0b01100, Self::ReadDeletedData),
            (0b111111, 0b000101, Self::WriteData),
            (0b111111, 0b001001, Self::WriteDeletedData),
            (0b10011111, 0b00000010, Self::ReadTrack),
            (0b10011111, 0b00001010, Self::ReadId),
            (0b10111111, 0b00001101, Self::FormatTrack),
            (0b11111, 0b10001, Self::ScanLow),
            (0b00011111, 0b11001, Self::ScanLowOrEqual),
            (0b00011111, 0b11101, Self::ScanHighOrEqual),
            (0b11111111, 0b00000111, Self::Recalibrate),
            (0b11111111, 0b00001000, Self::SenseInterruptStatus),
            (0b11111111, 0b00000011, Self::Specify),
            (0b11111111, 0b00000100, Self::SenseDriveStatus),
            (0b11111111, 0b00001111, Self::Seek),
        ];

        for (mask, val, cmd) in mapping {
            if b & mask == val {
                return cmd
            }
        }

        Self::Invalid
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Chrn {
    c: u8,
    h: u8,
    r: u8,
    n: u8,
}

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
struct RwData {
    mt: bool,
    mfm: bool,
    sk: bool,
    chrn: Chrn,
    eot: u8,
    gpl: u8,
    // STL for scans, DTL for rest
    dtl_or_stp: u8,
    disk: DiskSpecifier,
}

impl RwData {
    fn parse(data: &[u8]) -> RwData {
        Self {
            mt: data[0] & 0x80 != 0,
            mfm: data[0] & 0x40 != 0,
            sk: data[0] & 0x20 != 0,
            disk: DiskSpecifier::parse(data[1]),
            chrn: Chrn {
                c: data[2],
                h: data[3],
                r: data[4],
                n: data[5],
            },
            eot: data[6],
            gpl: data[7],
            dtl_or_stp: data[8],
        }
    }
}

#[bitsize(3)]
#[derive(Copy, Clone, DebugBits, FromBits, Default)]
struct DiskSpecifier {
    ds: u2,
    hds: u1,
}

impl DiskSpecifier {
    fn parse(b: u8) -> DiskSpecifier {
        Self::from(u3::new(b & 0x7))
    }
}

#[allow(unused)]
#[derive(Copy, Clone, Debug)]
enum Command {
    ReadData(RwData),
    ReadDeletedData(RwData),
    WriteData(RwData),
    WriteDeletedData(RwData),
    ReadTrack(RwData),
    ReadId {
        mfm: bool,
        disk: DiskSpecifier,
    },
    FormatTrack {
        mfm: bool,
        disk: DiskSpecifier,
        n: u8,
        sc: u8,
        gpl: u8,
        d: u8,
    },
    ScanLow(RwData),
    ScanLowOrEqual(RwData),
    ScanHighOrEqual(RwData),
    Recalibrate(DiskSpecifier),
    SenseInterruptStatus,
    Specify {
        srt: u8,
        hut: u8,
        hlt: u8,
        dma_disabled: bool,
    },
    SenseDriveStatus(DiskSpecifier),
    Seek {
        disk: DiskSpecifier,
        ncn: u8,
    },
    Invalid,
}

impl Command {
    pub fn try_parse(data: &[u8]) -> Option<Command> {
        let id = CmdId::from_byte(data[0]);
        if data.len() < id.total_len() {
            return None
        }

        Some(match id {
            CmdId::ReadData => Command::ReadData(RwData::parse(data)),
            CmdId::ReadDeletedData => Command::ReadDeletedData(RwData::parse(data)),
            CmdId::WriteData => Command::WriteData(RwData::parse(data)),
            CmdId::WriteDeletedData => Command::WriteDeletedData(RwData::parse(data)),
            CmdId::ReadTrack => Command::ReadTrack(RwData::parse(data)),
            CmdId::ReadId => Command::ReadId {
                disk: DiskSpecifier::parse(data[1]),
                mfm: data[0] & 0x40 != 0,
            },
            CmdId::FormatTrack => Command::FormatTrack {
                mfm: data[0] & 0x40 != 0,
                disk: DiskSpecifier::parse(data[1]),
                n: data[2],
                sc: data[3],
                gpl: data[4],
                d: data[5],
            },
            CmdId::ScanLow => Command::ScanLow(RwData::parse(data)),
            CmdId::ScanLowOrEqual => Command::ScanLowOrEqual(RwData::parse(data)),
            CmdId::ScanHighOrEqual => Command::ScanHighOrEqual(RwData::parse(data)),
            CmdId::Recalibrate => Command::Recalibrate(DiskSpecifier::parse(data[1])),
            CmdId::SenseInterruptStatus => Command::SenseInterruptStatus,
            CmdId::Specify => Command::Specify {
                srt: data[1] >> 4,
                hut: data[1] & 0xf,
                hlt: data[2] >> 1,
                dma_disabled: data[2] & 1 != 0,
            },
            CmdId::SenseDriveStatus => Command::SenseDriveStatus(DiskSpecifier::parse(data[1])),
            CmdId::Seek => Command::Seek {
                disk: DiskSpecifier::parse(data[1]),
                ncn: data[2],
            },
            CmdId::Invalid => Command::Invalid,
        })
    }
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq, Hash, Default)]
pub enum Dir {
    #[default]
    CpuToFdd,
    FddToCpu,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, PartialEq, Eq, Hash, Default)]
pub struct MainStatus {
    disk_busy: [bool; 4],
    command_in_progress: bool,
    dma_disabled: bool,
    transfer_direction: Dir,
    ready_to_transfer: bool,
}

#[bitsize(2)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq, Hash, Default)]
pub enum CommandStatus {
    #[default]
    Ok,
    TerminatedAbnormally,
    InvalidCommand,
    ReadySignalChange,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode)]
pub struct St0 {
    disk: DiskSpecifier,
    not_ready: bool,
    check_equipment: bool,
    seeked: bool,
    status: CommandStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskSnapshot {
    head: u8,
    track: u16,
    cylinder: u8,
    data: Option<DiskDataSnapshot>,
}

#[allow(unused)]
#[derive(Debug, Default)]
pub struct Disk {
    head: u8,
    track: u16,
    cylinder: u8,
    data: Option<DiskData>,
}

impl Disk {
    pub fn num_cylinders(&self) -> u32 {
        80
    }

    pub fn num_heads(&self) -> u32 {
        2
    }

    pub fn sector_size(&self) -> u32 {
        512
    }

    pub fn sectors_per_track(&self) -> u32 {
        let num_bytes = self.data.as_ref().map(|d| d.len() as u32).unwrap_or(0);
        let num_sectors = num_bytes.div_ceil(self.sector_size());
        let num_tracks = self.num_heads() * self.num_cylinders();
        num_sectors.div_ceil(num_tracks)
    }

    pub fn sectors_per_head(&self) -> u32 {
        self.sectors_per_track() * self.num_heads()
    }

    pub fn chrn_to_offset(&self, chrn: &Chrn) -> u32 {
        let track_offset = chrn.c as u32 * self.num_heads() + chrn.h as u32;
        let sector_offset = track_offset * self.sectors_per_track() + (chrn.r - 1) as u32;

        sector_offset * self.sector_size()
    }

    pub fn increment_chrn(&self, chrn: &Chrn, num_sectors: u32) -> Chrn {
        let new_sectors = (chrn.r as u32 - 1) + num_sectors;
        Chrn {
            c: chrn.c + (new_sectors / self.sectors_per_head()) as u8,
            h: 0,
            r: (new_sectors % self.sectors_per_head()) as u8 + 1,
            ..*chrn
        }
    }

    pub fn read_to_mem(
        &self, chrn: &Chrn, addr: u32, len: u32, mode: u8, memory: &Mem32, mmio: &mut HwMmio,
    ) -> Result<Chrn, CommandStatus> {
        if let Some(data) = &self.data {
            let offset = self.chrn_to_offset(chrn);
            info!("Reading {len} bytes at offset 0x{offset:X} from disk into memory at 0x{addr:X}");
            println!("Reading FDC to memory at 0x{addr:X}");

            assert_eq!(
                ((addr & 0xffff) + len - 1) >> 16,
                0,
                "write must not overflow DMA address register"
            );

            let bytes = &data.read_slice(offset as usize..offset as usize + len as usize);
            for chunk in bytes.chunks(512) {
                let digest = md5::compute(chunk);
                println!("Sector digest: {digest:02X?}");
            }

            // Only write when the mode is '01 write to memory'
            if mode == 0b01 {
                memory.write_physical_slice(addr, bytes, mmio).unwrap();
            }

            if addr <= 0x1f4b7 && addr + len > 0x1f4b7 && bytes[0x1f4b7 - addr as usize] != 0x68 {
                println!("FISHY: Writing to 0x1f4b7: {:02X?}", bytes[0x1f4b7 - addr as usize]);
            }

            Ok(self.increment_chrn(chrn, len.div_ceil(512)))
        } else {
            Err(CommandStatus::TerminatedAbnormally)
        }
    }

    pub fn snapshot(&self) -> DiskSnapshot {
        DiskSnapshot {
            head: self.head,
            track: self.track,
            cylinder: self.cylinder,
            data: self.data.as_ref().map(|d| d.snapshot()),
        }
    }
}

#[derive(Debug)]
pub struct Fdc {
    disks: [Disk; 4],
    enabled: bool,
    irq: DualIrqLine,
    selected_disk: u2,
    interrupts_enabled: bool,
    dma_disabled: bool,
    cmd_buf: ArrayVec<u8, 16>,
    output_buf: ArrayVec<u8, 16>,
    st0: St0,
    dma_addr: Arc<AtomicU32>,
    dma_len: Arc<AtomicU16>,
    memory: Arc<Mem32>,
    dma_mode: Arc<AtomicU8>,
    dor: DigitalOutput,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FdcSnapshot {
    disks: [DiskSnapshot; 4],
    enabled: bool,
    selected_disk: u8,
    interrupts_enabled: bool,
    dma_disabled: bool,
    cmd_buf: ArrayVec<u8, 16>,
    output_buf: ArrayVec<u8, 16>,
    st0: St0,
    dma_addr: u32,
    dma_len: u16,
    dma_mode: u8,
    dor: DigitalOutput,
}

impl Fdc {
    pub fn new(
        data: DiskData, irq: DualIrqLine, dma_addr: Arc<AtomicU32>, dma_len: Arc<AtomicU16>, dma_mode: Arc<AtomicU8>,
        memory: Arc<Mem32>,
    ) -> Self {
        Self {
            disks: [
                Disk {
                    data: Some(data),
                    head: 0,
                    track: 0,
                    cylinder: 0,
                },
                Disk::default(),
                Disk::default(),
                Disk::default(),
            ],
            enabled: false,
            irq,
            selected_disk: u2::new(0),
            dma_disabled: false,
            interrupts_enabled: true,
            cmd_buf: ArrayVec::new(),
            output_buf: ArrayVec::new(),
            st0: St0::default(),
            dma_addr,
            dma_len,
            dma_mode,
            memory,
            dor: DigitalOutput::new(u1::new(0), true, true, [false; 2]),
        }
    }

    pub fn replace_disk(&mut self, index: usize, new_data: Vec<u8>) {
        self.disks[index] = Disk {
            head: 0,
            track: 0,
            cylinder: 0,
            data: Some(DiskData::from_mem(new_data)),
        };

        info!("Disk replaced; current FDC = {self:#?}");
    }

    pub fn read(&mut self, addr: u8) -> u8 {
        match addr {
            2 => self.dor.value,
            4 => {
                // 1 = RQM data register is ready
                // 0 = system -> controller
                // 0 = DMA mode
                // 0 = diskette is not busy
                // 0000 = drive 3, 2, 1, 0 are not busy

                let dir = if self.output_buf.is_empty() {
                    Dir::CpuToFdd
                } else {
                    Dir::FddToCpu
                };

                // Set when any read or write transfer is in progress
                let command_in_progress = !self.output_buf.is_empty(); // || !self.cmd_buf.is_empty();

                MainStatus::new([false; 4], command_in_progress, self.dma_disabled, dir, true).value
            },
            5 => {
                if self.output_buf.is_empty() {
                    // 00 = last command OK
                    // 0 = no seek completed
                    // 0 = no equipment check after error
                    // 0 = ready
                    // 0 = head 0
                    // xx = selected drive
                    self.selected_disk.value()
                } else {
                    self.output_buf.remove(0)
                }
            },
            _ => 0xff, // TODO
        }
    }

    pub fn write(&mut self, addr: u8, val: u8, mmio: &mut HwMmio) {
        match addr {
            2 => {
                self.dor = DigitalOutput::from(val);

                let was_enabled = self.enabled;
                self.enabled = self.dor.enabled();

                self.selected_disk = u2::new(self.dor.drive_select().as_u8());
                self.interrupts_enabled = self.dor.dma_enabled();

                for disk in self.disks.iter_mut() {
                    disk.cylinder = 0;
                }

                info!("FDC: {self:?}");

                if self.enabled && !was_enabled {
                    info!("FDC: Raising interrupt");
                    self.raise_interrupt();
                    self.st0 = St0::new(
                        DiskSpecifier::new(self.selected_disk, u1::new(0)),
                        false,
                        false,
                        false,
                        CommandStatus::ReadySignalChange,
                    );
                }
            },
            5 => {
                self.cmd_buf.push(val);

                if let Some(cmd) = Command::try_parse(&self.cmd_buf) {
                    self.cmd_buf.clear();

                    info!("FDC: {cmd:?}");
                    match cmd {
                        Command::ReadData(rw_data) => {
                            let disk = &self.disks[rw_data.disk.ds().as_usize()];
                            let dma_addr = self.dma_addr.load(Ordering::Relaxed);
                            let dma_len = self.dma_len.load(Ordering::Relaxed) as u32 + 1;
                            let dma_mode = self.dma_mode.load(Ordering::Relaxed);
                            assert!(self.output_buf.is_empty());

                            assert!(rw_data.mt);

                            match disk.read_to_mem(&rw_data.chrn, dma_addr, dma_len, dma_mode, &self.memory, mmio) {
                                Ok(result_chrn) => {
                                    let mut disk = rw_data.disk;
                                    disk.set_hds(u1::new(result_chrn.h));
                                    self.st0 = St0::new(disk, false, false, false, CommandStatus::Ok);

                                    self.disks[disk.ds().as_usize()].cylinder = result_chrn.c;
                                    self.output_buf.extend([
                                        self.st0.value,
                                        0, // TODO: ST1
                                        0, // TODO: ST2
                                        result_chrn.c,
                                        result_chrn.h,
                                        result_chrn.r,
                                        result_chrn.n,
                                    ]);
                                },
                                Err(status) => {
                                    let disk = rw_data.disk;
                                    self.st0 = St0::new(disk, false, false, false, status);
                                    self.output_buf.extend([
                                        self.st0.value,
                                        0, // TODO: ST1
                                        0, // TODO: ST2
                                        rw_data.chrn.c,
                                        rw_data.chrn.h,
                                        rw_data.chrn.r,
                                        rw_data.chrn.n,
                                    ]);
                                },
                            }

                            info!("ST0: {:?}", self.st0);
                            info!("FDC: Read result: {:02X?}", self.output_buf);
                            self.raise_interrupt();
                        },
                        Command::ReadDeletedData(_) => error!("FDC command: {cmd:?}"),
                        Command::WriteData(_) => error!("FDC command: {cmd:?}"),
                        Command::WriteDeletedData(_) => error!("FDC command: {cmd:?}"),
                        Command::ReadTrack(_) => error!("FDC command: {cmd:?}"),
                        Command::ReadId {
                            ..
                        } => error!("FDC command: {cmd:?}"),
                        Command::FormatTrack {
                            ..
                        } => error!("FDC command: {cmd:?}"),
                        Command::ScanLow(_) => error!("FDC command: {cmd:?}"),
                        Command::ScanLowOrEqual(_) => error!("FDC command: {cmd:?}"),
                        Command::ScanHighOrEqual(_) => error!("FDC command: {cmd:?}"),
                        Command::Recalibrate(disk) => {
                            let d = &mut self.disks[disk.ds().as_usize()];
                            d.track = 0;

                            if d.data.is_some() {
                                self.st0 = St0::new(disk, false, false, true, CommandStatus::Ok);
                            } else {
                                self.st0 = St0::new(disk, true, false, true, CommandStatus::TerminatedAbnormally);
                            }

                            info!("FDC: Recalibration result: ST = {:?}", self.st0);

                            self.raise_interrupt();
                        },
                        Command::SenseInterruptStatus => {
                            info!("FDC: ST0 = {:?}", self.st0);
                            self.output_buf
                                .extend([self.st0.value, self.disks[self.selected_disk.as_usize()].cylinder]);
                        },
                        Command::Specify {
                            dma_disabled, ..
                        } => self.dma_disabled = dma_disabled,
                        Command::SenseDriveStatus(_) => todo!("FDC command: {cmd:?}"),
                        Command::Seek {
                            disk,
                            ncn,
                        } => {
                            self.disks[disk.ds().as_usize()].cylinder = ncn;
                            self.st0 = St0::new(disk, false, false, true, CommandStatus::Ok);
                            self.raise_interrupt();
                        },
                        Command::Invalid => error!("FDC command: {cmd:?}"),
                    }
                }
            },
            _ => (),
        }
    }

    fn raise_interrupt(&self) {
        if self.interrupts_enabled {
            self.irq.pulse();
        } else {
            info!("FDC would have triggered interrupt if enabled");
        }
    }

    pub fn cmos_media_state(&self) -> DisketteMediaState {
        DisketteMediaState::new(DisketteType::Established360In360, false, false, false, DataRate::Rate300Kbps)
    }

    pub fn snapshot(&self) -> FdcSnapshot {
        FdcSnapshot {
            disks: std::array::from_fn(|n| self.disks[n].snapshot()),
            enabled: self.enabled,
            selected_disk: self.selected_disk.as_u8(),
            interrupts_enabled: self.interrupts_enabled,
            dma_disabled: self.dma_disabled,
            cmd_buf: self.cmd_buf.clone(),
            output_buf: self.output_buf.clone(),
            st0: self.st0,
            dma_addr: self.dma_addr.load(Ordering::SeqCst),
            dma_len: self.dma_len.load(Ordering::SeqCst),
            dma_mode: self.dma_mode.load(Ordering::SeqCst),
            dor: self.dor,
        }
    }

    pub fn restore(&mut self, fdc: FdcSnapshot) {
        self.disks = fdc.disks.map(|disk| Disk {
            head: disk.head,
            track: disk.track,
            cylinder: disk.cylinder,
            data: disk.data.map(|d| DiskData::from_snapshot(d, None)),
        });
        self.enabled = fdc.enabled;
        self.selected_disk = u2::new(fdc.selected_disk);
        self.interrupts_enabled = fdc.interrupts_enabled;
        self.dma_disabled = fdc.dma_disabled;
        self.cmd_buf = fdc.cmd_buf;
        self.output_buf = fdc.output_buf;
        self.st0 = fdc.st0;
        self.dma_addr.store(fdc.dma_addr, Ordering::SeqCst);
        self.dma_len.store(fdc.dma_len, Ordering::SeqCst);
        self.dma_mode.store(fdc.dma_mode, Ordering::SeqCst);
        self.dor = fdc.dor;
    }
}

#[cfg(test)]
mod test {
    use crate::hw::fdc::{Chrn, Disk, DiskData};

    #[test]
    pub fn disk_geometry() {
        let disk = Disk {
            head: 0,
            track: 0,
            cylinder: 0,
            data: Some(DiskData::from_mem(vec![0; 720 * 1024])),
        };

        assert_eq!(disk.num_cylinders(), 80);
        assert_eq!(disk.num_heads(), 2);
        assert_eq!(disk.sectors_per_track(), 9);
    }

    #[test]
    pub fn chrn_to_offset() {
        let disk = Disk {
            head: 0,
            track: 0,
            cylinder: 0,
            data: Some(DiskData::from_mem(vec![0; 720 * 1024])),
        };

        assert_eq!(
            disk.chrn_to_offset(&Chrn {
                c: 0,
                h: 0,
                r: 1,
                n: 2,
            }),
            0x0
        );

        assert_eq!(
            disk.chrn_to_offset(&Chrn {
                c: 0,
                h: 0,
                r: 2,
                n: 2,
            }),
            0x200
        );

        assert_eq!(
            disk.chrn_to_offset(&Chrn {
                c: 0,
                h: 0,
                r: 9,
                n: 2,
            }),
            0x1000
        );

        assert_eq!(
            disk.chrn_to_offset(&Chrn {
                c: 0,
                h: 1,
                r: 1,
                n: 2,
            }),
            0x1200
        );
    }
}
