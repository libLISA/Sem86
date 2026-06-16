use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use jiff::Zoned;
use log::{info, warn};
use serde::{Deserialize, Serialize};

use super::fdc::Fdc;
use crate::hw::bcd::IntoBcd;
use crate::hw::pic::DualIrqLine;
use crate::time::{EmulatorClock, EmulatorTimestamp, Timer};

// https://bochs.sourceforge.io/techspec/CMOS-reference.txt

#[bitsize(3)]
#[derive(Copy, Clone, FromBits, Debug)]
pub enum DisketteType {
    Try360In360,
    Try360In12,
    Try12In12,
    Established360In360,
    Established360In12,
    Established12In12,
    Reserved,
    Other,
}

#[bitsize(2)]
#[derive(Copy, Clone, FromBits, Debug)]
pub enum DataRate {
    Rate500Kbps,
    Rate300Kbps,
    Rate250Kbps,
    Rate1Mbps,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct DisketteMediaState {
    ty: DisketteType,
    supports_4mb_media: bool,
    media_type_established: bool,
    double_stepping_required: bool,
    data_rate: DataRate,
}

#[bitsize(2)]
#[derive(Copy, Clone, FromBits, Debug)]
pub enum MonitorType {
    EgaVga,
    Cga40x25,
    Cga80x25,
    Mda,
}

#[bitsize(1)]
#[derive(Copy, Clone, FromBits, Debug)]
pub enum DataMode {
    Bcd,
    Binary,
}

impl DataMode {
    fn convert(&self, val: u8) -> u8 {
        match self {
            DataMode::Bcd => val.to_bcd(),
            DataMode::Binary => val,
        }
    }
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
pub struct StatusA {
    interrupt_rate: u4,
    divider: u3,
    time_update_in_progress: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
pub struct StatusB {
    enable_dst: bool,
    enable_24h_mode: bool,
    data_mode: DataMode,
    enable_square_wave_output: bool,
    enable_update_ended_interrupt: bool,
    enable_alarm_interrupt: bool,
    enable_periodic_interrupt: bool,
    enable_cycle_update: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct EquipmentByte {
    floppy_drive_installed: bool,
    x87_installed: bool,
    keyboard_enabled: bool,
    display_enabled: bool,
    monitor_type: MonitorType,
    /// number of drives + 1
    floppy_drive_count: u2,
}

#[derive(Clone, Debug)]
pub struct Cmos {
    registers: [u8; 128],
    nmi: bool,
    selected_reg: usize,
    rtc_irq: DualIrqLine,
    status_c: Arc<AtomicU8>,
    status_b: StatusB,
    phys_mem_size: u64,
    status_a: StatusA,
    timer_info: Option<TimerInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct CmosSnapshot {
    #[serde(with = "serde_big_array::BigArray")]
    registers: [u8; 128],
    nmi: bool,
    selected_reg: usize,
    status_c: u8,
    status_b: StatusB,
    phys_mem_size: u64,
    status_a: StatusA,
}

#[derive(Clone, Debug)]
struct TimerInfo {
    interrupt_rate_hz: u32,
    enabled: Arc<AtomicBool>,
}

impl Drop for TimerInfo {
    fn drop(&mut self) {
        self.enabled.store(false, Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct CmosTimer {
    info: TimerInfo,
    next_interrupt: EmulatorTimestamp,
    rtc_irq: DualIrqLine,
    status_c: Arc<AtomicU8>,
}

impl Timer for CmosTimer {
    fn tick(&mut self, now: EmulatorTimestamp) -> bool {
        let enabled = self.info.enabled.load(Ordering::Relaxed);
        if enabled {
            self.status_c.fetch_or(0xc0, Ordering::SeqCst);
            self.rtc_irq.pulse();

            let period = Duration::from_secs(1) / self.info.interrupt_rate_hz;
            let next = self.next_interrupt + period;
            if next < now {
                warn!("CMOS timer skipped a tick");
                self.next_interrupt = now + period;
            } else {
                self.next_interrupt = next;
            }
        }

        enabled
    }

    fn next_tick(&self) -> EmulatorTimestamp {
        self.next_interrupt
    }
}

impl Cmos {
    pub fn new(rtc_irq: DualIrqLine, phys_mem_size: u64) -> Self {
        Self {
            registers: [0; 128],
            nmi: false,
            selected_reg: 0,
            rtc_irq,
            status_a: StatusA::new(u4::new(0b0110), u3::new(0b010), false),
            status_b: StatusB::new(false, true, DataMode::Bcd, false, false, false, false, false),
            status_c: Arc::new(AtomicU8::new(0)),
            phys_mem_size,
            timer_info: None,
        }
    }

    pub fn write_port(&mut self, val: u8) {
        self.selected_reg = val as usize & 0x7f;
        self.nmi = val & 0x80 != 0;
    }

    pub fn write_data(&mut self, val: u8, time: &EmulatorClock) {
        info!("Write 0x{val:02X} to register 0x{:X}", self.selected_reg);
        match self.selected_reg {
            0xA => {
                let mut new_status = StatusA::from(val);
                new_status.set_time_update_in_progress(self.status_a.time_update_in_progress());
                self.status_a = new_status;
                info!("Status A written: {:X?}", self.status_a);

                self.update_timer(time);
            },
            0xB => {
                self.status_b = StatusB::from(val);
                info!("Status B written: {:X?}", self.status_b);

                self.update_timer(time);
            },
            0xC => self.status_c.store(val, Ordering::SeqCst),
            _ => {
                warn!("TODO: Write 0x{val:02X} to register 0x{:X}", self.selected_reg);
                self.registers[self.selected_reg] = val
            },
        }

        self.selected_reg = 0;
    }

    fn update_timer(&mut self, time: &EmulatorClock) {
        let enabled = self.irq_enabled() && self.status_a.interrupt_rate() != u4::new(0);
        if !enabled {
            info!("Timer IRQ is disabled");
            // Stop the current timer
            self.timer_info.take();
        } else {
            // 0b1111 = 2, 0b1110 = 4, 0b1101 = 8, etc.
            let val = self.status_a.interrupt_rate().as_u8();
            let val = if val <= 2 { val + 7 } else { val };
            let interrupt_rate_hz = 2 << (val ^ 0xf);

            if let Some(info) = self.timer_info.as_ref()
                && info.interrupt_rate_hz != interrupt_rate_hz
            {
                self.timer_info = None;
            }

            if self.timer_info.is_none() {
                info!("Timer IRQ is enabled @ {interrupt_rate_hz} Hz");
                let info = TimerInfo {
                    enabled: Arc::new(AtomicBool::new(true)),
                    interrupt_rate_hz,
                };

                time.register_timer(Box::new(CmosTimer {
                    info: info.clone(),
                    next_interrupt: EmulatorTimestamp::now(time),
                    rtc_irq: self.rtc_irq.clone(),
                    status_c: self.status_c.clone(),
                }));

                self.timer_info = Some(info);
            }
        }
    }

    pub fn read(&mut self, fdc: &Fdc) -> u8 {
        // Extended memory in blocks of KiB (63MiB seems to be the max)
        let phys_kbs = self.phys_mem_size / 1024;
        let extended_memory = phys_kbs.saturating_sub(1024).min(0xfc00);
        // Extended memory in blocks of 64KiB
        let full_extended_memory = (phys_kbs.saturating_sub(16 * 1024) / 64).min(0xbf00);

        // TODO: Memory above 4G in 0x5b, 0x5c and 0x5d

        let now = Zoned::now();
        let val = match self.selected_reg {
            0x0 => self.status_b.data_mode().convert(now.second() as u8),
            0x2 => self.status_b.data_mode().convert(now.minute() as u8),
            // TODO: 12-hour output if status_b.enable_24h_mode() == false
            0x4 => self.status_b.data_mode().convert(now.hour() as u8),
            0x6 => self.status_b.data_mode().convert(now.weekday().to_sunday_one_offset() as u8),
            0x7 => self.status_b.data_mode().convert(now.days_in_month() as u8),
            0x8 => self.status_b.data_mode().convert(now.month() as u8),
            0x9 => self.status_b.data_mode().convert((now.year() % 100) as u8),

            0xa => {
                // Fake an update of 4 milliseconds
                self.status_a
                    .set_time_update_in_progress(now.millisecond() >= 999 || now.millisecond() <= 2);

                self.status_a.value
            },
            0xb => self.status_b.value,
            0xc => {
                let val = self.status_c.load(Ordering::SeqCst);
                self.status_c.store(0, Ordering::SeqCst);

                // TODO: Only if irq is enabled?
                self.rtc_irq.set(false);

                val
            },

            // Indicate that CMOS battery was connected and RAM is valid
            0xD => 0x80,

            0x10 => fdc.cmos_media_state().into(),
            0x12 => 0xf0, // TODO

            // TODO: We might have to inspect the actual FDC to know how many drives we have installed.
            0x14 => EquipmentByte::new(true, true, true, false, MonitorType::EgaVga, u2::new(0)).into(),
            0x19 => 0x2f, // TODO
            0x1f => 0xff,
            0x1e => 0xff,
            0x20 => 0xc8,
            0x21 => 0x41,
            0x1b => 0x41,
            0x1d => 0x10,
            0x23 => 0x3f,

            0x17 | 0x30 => extended_memory as u8,
            0x18 | 0x31 => (extended_memory >> 8) as u8,
            0x32 => ((now.year() / 100) as u8).to_bcd(),
            0x34 => full_extended_memory as u8,
            0x35 => (full_extended_memory >> 8) as u8,

            // Boot order: 0x3D low nibble, 0x3D high nibble, 0x38 high nibble.
            // 0x1 = floppy
            // 0x2 = HDD
            // 0x3 = CD-ROM
            // 0x4 = PCMCIA
            // 0x5 = USB
            // 0x6 = Network
            0x3d => 0x32,
            0x38 => 0x01,

            // Bochs-specific ATA translation mode
            // 00 = ATA_TRANSLATION_NONE
            // 01 = ATA_TRANSLATION_LBA
            // 10 = ATA_TRANSLATION_LARGE
            // 11 = ATA_TRANSLATION_RECHS
            0x39 => 1, // Primary IDE   = LBA
            0x3A => 1, // Secondary IDE = LBA
            _ => {
                // let index = self.selected_reg;
                // self.selected_reg = 0;
                // self.registers[index]
                0 // TODO
            },
        };
        info!("Read register 0x{:02X} from CMOS = 0x{:02X}", self.selected_reg, val);

        val
    }

    pub fn irq_enabled(&self) -> bool {
        self.status_b.enable_periodic_interrupt()
    }

    pub fn snapshot(&self) -> CmosSnapshot {
        CmosSnapshot {
            registers: self.registers,
            nmi: self.nmi,
            selected_reg: self.selected_reg,
            status_c: self.status_c.load(Ordering::SeqCst),
            status_b: self.status_b,
            phys_mem_size: self.phys_mem_size,
            status_a: self.status_a,
        }
    }

    pub fn restore(&mut self, cmos: CmosSnapshot, clock: &EmulatorClock) {
        self.registers = cmos.registers;
        self.nmi = cmos.nmi;
        self.selected_reg = cmos.selected_reg;
        self.status_c.store(cmos.status_c, Ordering::SeqCst);
        self.status_b = cmos.status_b;
        self.phys_mem_size = cmos.phys_mem_size;
        self.status_a = cmos.status_a;
        self.update_timer(clock);
    }
}
