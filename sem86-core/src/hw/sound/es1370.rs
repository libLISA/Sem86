use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{debug, error, info, trace, warn};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

use crate::hw::pci::{CommonPciHeader, CommonWriteEvent, DeviceWriteEvent, GeneralDeviceHeader, PciCommandRegister, PciDevice};
use crate::hw::pic::DualDynamicIrqLine;
use crate::hw::ports::{PortError, PortIoData, WithIoSpace};
use crate::hw::sound::backends::Frontend;
use crate::hw::sound::backends::device::DeviceBackend;
use crate::time::{EmulatorClock, EmulatorTimestamp, Timer};

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
struct FrontendControl {
    enabled: bool,
    interrupt_enabled: bool,
    data_size: DataSize,
    data_mode: DataMode,
    reserved: u4,
}

struct Es1370Frontend {
    buf_start_addr: AtomicU32,
    buf_dword_size: AtomicU16,
    required_buffer_size: AtomicU32,

    /// The read position in the buffer, as a number of dwords.
    buf_dword_pos: AtomicU16,
    samples_remaining: AtomicU16,
    sample_count: AtomicU16,
    control: AtomicU8,
    mem: Arc<Mem32>,
    frequency: AtomicU32,
    irq: DualDynamicIrqLine,
    interrupt_pending: AtomicBool,
    interrupt_line: AtomicU8,
    samples: Mutex<VecDeque<f32>>,
}

impl Debug for Es1370Frontend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Es1370Frontend")
            .field("buf_start_addr", &self.buf_start_addr)
            .field("buf_dword_size", &self.buf_dword_size)
            .field("buf_dword_pos", &self.buf_dword_pos)
            .field("samples_remaining", &self.samples_remaining)
            .field("sample_count", &self.sample_count)
            .field("control", &self.control)
            .field("mem", &self.mem)
            .field("frequency", &self.frequency)
            .field("irq", &self.irq)
            .field("interrupt_pending", &self.interrupt_pending)
            .field("interrupt_line", &self.interrupt_line)
            .finish()
    }
}

impl Frontend for Es1370Frontend {
    fn fill_buffer<T: cpal::SizedSample + cpal::FromSample<f32>>(&self, buf: &mut [T], channels: usize) {
        assert_eq!(channels, 2, "todo: support {channels} channels");
        let control = self.control();
        if !control.enabled() {
            // trace!("Filling audio buffer with zeros, because playback is disabled: {control:?}, buffer @ 0x{buf_addr:X} (size {buf_size})");
            buf.fill(T::from_sample(0.));
        } else {
            let mut samples = self.samples.lock().unwrap();
            let mut num_missing_samples = 0;
            for sample in buf.iter_mut() {
                *sample = T::from_sample(if let Some(sample) = samples.pop_front() {
                    sample
                } else {
                    num_missing_samples += 1;
                    0.0
                });
            }

            debug!(
                "Read {} samples from sample buffer at 0x{:X?}, {} samples remaining",
                buf.len(),
                self.buf_start_addr,
                samples.len()
            );

            if samples.is_empty() {
                let new = self.required_buffer_size.fetch_add(num_missing_samples, Ordering::Relaxed) + num_missing_samples;
                warn!("Sample buffer is not being filled fast enough! Increased required buffer size to {new}");
            }
        }
    }
}

impl Es1370Frontend {
    fn convert_u8_sample(val: u8) -> f32 {
        (val as f32 - 128.0) / 128.0
    }

    fn convert_i16_sample(val: i16) -> f32 {
        val as f32 / 32768.0
    }

    fn control(&self) -> FrontendControl {
        self.control.load(Ordering::SeqCst).into()
    }

    fn fill_samples_buffer(&self) {
        let control = self.control();
        let buf_size = self.buf_dword_size.load(Ordering::SeqCst);
        let buf_addr = self.buf_start_addr.load(Ordering::SeqCst);

        if control.enabled() {
            let mut samples = self.samples.lock().unwrap();
            let count_before = samples.len();
            // TODO: Dynamically size buffer to be as small as possible without causing any underruns
            while samples.len() < self.required_buffer_size.load(Ordering::Relaxed) as usize {
                let samples_in_u32 = 4 / control.data_size().as_usize();
                let samples_remaining = self.samples_remaining.fetch_sub(samples_in_u32 as u16, Ordering::SeqCst);
                if samples_remaining < samples_in_u32 as u16 || samples_remaining.checked_add(samples_in_u32 as u16).is_none() {
                    let sample_count = self.sample_count.load(Ordering::SeqCst);
                    self.samples_remaining.store(sample_count, Ordering::SeqCst);
                    if control.interrupt_enabled() && !self.interrupt_pending.load(Ordering::SeqCst) {
                        let interrupt_line = self.interrupt_line.load(Ordering::SeqCst);
                        let buf_pos = self.buf_dword_pos.load(Ordering::SeqCst);
                        trace!(
                            "Sample count reached 0 at buffer position 0x{buf_pos:X}, raising IRQ 0x{interrupt_line:02X} and resetting sample count to {sample_count}"
                        );

                        self.interrupt_pending.store(true, Ordering::SeqCst);
                        self.irq.pulse(interrupt_line);
                        // TODO: If we're very low on samples we could potentially read more of the buffer here.
                        break
                    }
                }

                let mut buf = [0; 4];
                let buf_pos = self.buf_dword_pos.fetch_add(1, Ordering::SeqCst);
                let buf_pos = if buf_pos >= buf_size {
                    // TODO: If not looping, buf_pos should stay at buf_size
                    self.buf_dword_pos.store(0, Ordering::SeqCst);
                    0
                } else {
                    buf_pos
                };
                let addr = buf_addr + (buf_pos as u32) * 4;
                self.mem.read_physical_slice_no_mmio(addr, &mut buf);

                for data in buf.chunks(control.data_size().as_usize()) {
                    match (control.data_mode(), control.data_size()) {
                        (DataMode::Mono, DataSize::U8) => {
                            let s = Self::convert_u8_sample(data[0]);
                            samples.extend([s, s])
                        },
                        (DataMode::Mono, DataSize::U16) => {
                            let s = Self::convert_i16_sample(i16::from_le_bytes(data[0..2].try_into().unwrap()));
                            samples.extend([s, s])
                        },
                        (DataMode::Stereo, DataSize::U8) => samples.extend([Self::convert_u8_sample(data[0])]),
                        (DataMode::Stereo, DataSize::U16) => {
                            samples.extend([Self::convert_i16_sample(i16::from_le_bytes(data[0..2].try_into().unwrap()))])
                        },
                    }
                }

                if samples.is_empty() {
                    warn!("Audio buffer underrun");
                }
            }

            trace!(
                "{} samples in sample buffer - added {} samples this tick",
                samples.len(),
                samples.len() - count_before
            );
        } else {
            trace!("Channel is disabled, no data to send");
        }
    }
}

#[derive(Clone, Debug)]
struct Channel {
    /// The physical address of the buffer
    buffer_address: u32,

    /// The number of samples - 1 that will be played.
    sample_count: u16,

    /// The number of frames (u32) in the buffer - 1.
    /// A frame consists of one sample per channel.
    /// If the channel is set to mono mode, buffer size and sample count are equal.
    buffer_dword_size: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct ChannelSnapshot {
    buffer_address: u32,
    sample_count: u16,
    buffer_dword_size: u16,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            buffer_address: 0,
            buffer_dword_size: 0,
            sample_count: 0,
        }
    }

    pub fn sample_count_reg(&self) -> u32 {
        self.sample_count as u32
    }

    pub fn set_sample_count_reg(&mut self, data: u16) {
        self.sample_count = data;
    }

    pub fn buffer_dword_size(&self) -> u16 {
        self.buffer_dword_size
    }

    pub fn set_buffer_dword_size(&mut self, size: u16) {
        self.buffer_dword_size = size;
    }

    fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            buffer_address: self.buffer_address,
            sample_count: self.sample_count,
            buffer_dword_size: self.buffer_dword_size,
        }
    }

    fn restore(&mut self, snapshot: ChannelSnapshot) {
        self.buffer_address = snapshot.buffer_address;
        self.sample_count = snapshot.sample_count;
        self.buffer_dword_size = snapshot.buffer_dword_size;
    }
}

#[derive(Clone, Debug)]
struct TimerInfo {
    enabled: Arc<AtomicBool>,
    busmaster_enabled: Arc<AtomicBool>,
}

impl Drop for TimerInfo {
    fn drop(&mut self) {
        self.enabled.store(false, Ordering::Relaxed)
    }
}

#[derive(Clone, Debug)]
struct ChannelTimer {
    frontend: Arc<Es1370Frontend>,
    next_tick: EmulatorTimestamp,
    info: TimerInfo,
}

impl Timer for ChannelTimer {
    fn tick(&mut self, now: EmulatorTimestamp) -> bool {
        let enabled = self.info.enabled.load(Ordering::SeqCst);
        if enabled {
            if self.info.busmaster_enabled.load(Ordering::SeqCst) {
                self.frontend.fill_samples_buffer();
            } else {
                trace!("Skipped filling bufer, because PCI busmastering is disabled")
            }

            let control = self.frontend.control();
            let freq = self.frontend.frequency.load(Ordering::SeqCst);
            let sample_period = self.frontend.sample_count.load(Ordering::SeqCst) as u32;
            let ticks_per_period = sample_period / control.data_mode().samples_per_frame() as u32;
            let ticks_per_update = ticks_per_period / 4;

            let delay = (Duration::from_secs(1) * ticks_per_update) / freq;
            self.next_tick += delay;

            while self.next_tick <= now {
                warn!("Skipping missed timer tick (delay: {delay:?})");
                self.next_tick += delay;
            }
        }

        enabled
    }

    fn next_tick(&self) -> EmulatorTimestamp {
        self.next_tick
    }
}

struct PlaybackChannel {
    channel: Channel,
    device: Option<DeviceBackend>,
    frontend: Arc<Es1370Frontend>,
    timer_info: Option<TimerInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct PlaybackChannelSnapshot {
    channel: ChannelSnapshot,
    buf_start_addr: u32,
    buf_dword_size: u16,
    buf_dword_pos: u16,
    samples_remaining: u16,
    sample_count: u16,
    control: u8,
    frequency: u32,
    interrupt_pending: bool,
    interrupt_line: u8,
    samples: VecDeque<f32>,
}

impl Debug for PlaybackChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackChannel").field("channel", &self.channel).finish()
    }
}

impl PlaybackChannel {
    pub fn new(mem: &Arc<Mem32>, irq: DualDynamicIrqLine) -> Self {
        let frontend = Arc::new(Es1370Frontend {
            buf_start_addr: AtomicU32::new(0),
            buf_dword_size: AtomicU16::new(0),
            buf_dword_pos: AtomicU16::new(0),
            required_buffer_size: AtomicU32::new(4096),
            control: AtomicU8::new(0),
            samples_remaining: AtomicU16::new(0),
            sample_count: AtomicU16::new(0),
            mem: mem.clone(),
            frequency: AtomicU32::new(44_100),
            irq,
            interrupt_pending: AtomicBool::new(false),
            interrupt_line: AtomicU8::new(0xff),
            samples: Mutex::new(VecDeque::new()),
        });

        Self {
            channel: Channel::new(),
            device: None,
            frontend,
            timer_info: None,
        }
    }

    pub fn buffer_address(&self) -> u32 {
        self.channel.buffer_address
    }

    pub fn set_buffer_address(&mut self, addr: u32) {
        self.channel.buffer_address = addr;
        self.frontend.buf_start_addr.store(addr, Ordering::SeqCst);
    }

    pub fn sample_count_reg(&self) -> u32 {
        self.channel.sample_count as u32 | (self.frontend.samples_remaining.load(Ordering::SeqCst) as u32) << 16
    }

    pub fn set_sample_count_reg(&mut self, data: u32) {
        self.channel.set_sample_count_reg(data as u16);
        self.frontend.sample_count.store(data as u16, Ordering::SeqCst);
    }

    pub fn buffer_def_reg(&self) -> u32 {
        self.channel.buffer_dword_size as u32 | (self.frontend.buf_dword_pos.load(Ordering::SeqCst) as u32) << 16
    }

    pub fn set_buffer_def_reg(&mut self, data: u32) {
        self.channel.set_buffer_dword_size(data as u16);
        self.frontend.buf_dword_size.store(data as u16, Ordering::SeqCst);
        self.frontend.buf_dword_pos.store((data >> 16) as u16, Ordering::SeqCst);
    }

    pub fn set_control(&self, enabled: bool, interrupt_enabled: bool, data_size: DataSize, data_mode: DataMode) {
        let val = FrontendControl::new(enabled, interrupt_enabled, data_size, data_mode);
        self.frontend.control.store(val.value, Ordering::SeqCst);
    }

    fn interrupt_pending(&self) -> bool {
        self.frontend.interrupt_pending.load(Ordering::SeqCst)
    }

    fn clear_pending_interrupt(&self) {
        self.frontend.interrupt_pending.store(false, Ordering::SeqCst);
    }

    fn set_irq_line(&self, interrupt_line: u8) {
        self.frontend.interrupt_line.store(interrupt_line, Ordering::SeqCst)
    }

    fn snapshot(&self) -> PlaybackChannelSnapshot {
        PlaybackChannelSnapshot {
            channel: self.channel.snapshot(),
            buf_start_addr: self.frontend.buf_start_addr.load(Ordering::SeqCst),
            buf_dword_size: self.frontend.buf_dword_size.load(Ordering::SeqCst),
            buf_dword_pos: self.frontend.buf_dword_pos.load(Ordering::SeqCst),
            samples_remaining: self.frontend.samples_remaining.load(Ordering::SeqCst),
            sample_count: self.frontend.sample_count.load(Ordering::SeqCst),
            control: self.frontend.control.load(Ordering::SeqCst),
            frequency: self.frontend.frequency.load(Ordering::SeqCst),
            interrupt_pending: self.frontend.interrupt_pending.load(Ordering::SeqCst),
            interrupt_line: self.frontend.interrupt_line.load(Ordering::SeqCst),
            samples: self.frontend.samples.lock().unwrap().clone(),
        }
    }

    fn restore(&mut self, channel: PlaybackChannelSnapshot) {
        self.channel.restore(channel.channel);
        self.frontend.buf_start_addr.store(channel.buf_start_addr, Ordering::SeqCst);
        self.frontend.buf_dword_size.store(channel.buf_dword_size, Ordering::SeqCst);
        self.frontend.buf_dword_pos.store(channel.buf_dword_pos, Ordering::SeqCst);
        self.frontend
            .samples_remaining
            .store(channel.samples_remaining, Ordering::SeqCst);
        self.frontend.sample_count.store(channel.sample_count, Ordering::SeqCst);
        self.frontend.control.store(channel.control, Ordering::SeqCst);
        self.frontend.frequency.store(channel.frequency, Ordering::SeqCst);
        self.frontend
            .interrupt_pending
            .store(channel.interrupt_pending, Ordering::SeqCst);
        self.frontend.interrupt_line.store(channel.interrupt_line, Ordering::SeqCst);
        *self.frontend.samples.lock().unwrap() = channel.samples;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct Codec {
    regs: [u8; 26],
    index: usize,
}

#[derive(Debug)]
pub struct Es1370Core {
    control: Control,
    memory_page: u32,
    serial_control: SerialControl,
    channels: [PlaybackChannel; 2],
    record: Channel,
    legacy_redirect: u32,
    #[allow(unused)]
    wave_volume: u16,
    codec: Codec,
    irq: DualDynamicIrqLine,
    irq_line: u8,
    busmaster_enabled: Arc<AtomicBool>,
}

#[bitsize(2)]
#[derive(Copy, Clone, Debug, FromBits)]
enum Playback1SampleRate {
    Rate5k512,
    Rate11k025,
    Rate22k05,
    Rate44k1,
}

impl Playback1SampleRate {
    pub fn frequency(&self) -> u32 {
        match self {
            Playback1SampleRate::Rate5k512 => 5_512,
            Playback1SampleRate::Rate11k025 => 11_025,
            Playback1SampleRate::Rate22k05 => 22_050,
            Playback1SampleRate::Rate44k1 => 44_100,
        }
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
struct Control {
    serr_disable: bool,

    codec_interface_enabled: bool,

    joystick_enabled: bool,

    uart_enabled: bool,

    /// To restart a stopped channel, this bit must be set low, then high.
    record_enabled: bool,

    /// To restart a stopped channel, this bit must be set low, then high.
    channel2_enabled: bool,

    /// To restart a stopped channel, this bit must be set low, then high.
    channel1_enabled: bool,

    /// For testing only.
    /// On real hardware, this would disable memory access for internal blocks.
    /// We don't implement it.
    memory_bus_request_enabled: bool,

    /// General purpose bit.
    xctl0: bool,

    /// When true, MPEG is used as source. When false, codec ADC is used.
    record_channel_source: bool,

    ///
    voice_interrupt_enable: bool,

    /// When true, playback is run in sync.
    dac_sync: bool,

    playback1_sample_rate: Playback1SampleRate,

    /// When true, source is MPEG clock. When 0, source is programmable clock generator.
    clock_generator_source: bool,

    /// Selects between SONY and I2S.
    /// We don't need to output to hardware, so for us this bit doesn't matter.
    mpeg_serial_data_format: bool,

    /// Clock divide ratio for playback2.
    pclkdiv: u13,

    /// Unused.
    open: bool,

    /// Either a general purpose output bit or an interrupt output bit depending on serr_disable.
    xctl1: bool,

    /// When true, disables recording.
    adc_stop: bool,
}

impl Control {
    pub fn playback2_sample_rate(&self) -> u32 {
        1411200 / (self.pclkdiv().as_u32() + 2)
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
struct Status {
    adc_interrupt_pending: bool,
    dac2_interrupt_pending: bool,
    dac1_interrupt_pending: bool,
    uart_interrupt_pending: bool,
    /// Set when a PCI bus abort condition occurs
    masked_ccb_interrupt_pending: bool,
    /// Indicates which interrupt has triggered (00 = DAC1, 01 = DAC2, 10 = ADC, 11 = Undefined)
    voice_code: u2,
    reserved: u1,
    codec_write_in_progress: bool,
    codec_busy: bool,
    codec_status: bool,
    reserved: u20,
    interrupt_pending: bool,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum LoopMode {
    Loop,
    Stop,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq)]
enum PlayMode {
    /// Normal playback
    Play,
    /// Repeatedly plays the last sample
    Pause,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum DataSize {
    U8,
    U16,
}

impl DataSize {
    fn as_usize(&self) -> usize {
        match self {
            DataSize::U8 => 1,
            DataSize::U16 => 2,
        }
    }
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum DataMode {
    Mono,
    Stereo,
}

impl DataMode {
    pub fn samples_per_frame(&self) -> usize {
        match self {
            DataMode::Mono => 1,
            DataMode::Stereo => 2,
        }
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize, Encode, Decode)]
struct SerialControl {
    p1_s_mb: DataMode,
    p1_s_eb: DataSize,
    p2_s_mb: DataMode,
    p2_s_eb: DataSize,
    r1_s_mb: DataMode,
    r1_s_eb: DataSize,
    /// When true, repeatedly plays last sample when disabled and in stop mode.
    p2_dac_sen: bool,
    /// When set to true, the sample counter is reloaded from the sample count register.
    p1_sct_reload: bool,
    p1_irq_enable: bool,
    p2_irq_enable: bool,
    r1_irq_enable: bool,
    p1_pause: PlayMode,
    p2_pause: PlayMode,
    p1_loop_sel: LoopMode,
    p2_loop_sel: LoopMode,
    r1_loop_sel: LoopMode,

    /// Offset value that will be added to the sample address counter when the channel is started/restarted.
    p2_st_inc: u3,

    /// Offset value that will be added to the sample address counter at the end of the loop.
    p2_end_inc: u3,
    reserved: u10,
}

#[allow(clippy::erasing_op)]
const REG_CONTROL: u16 = 0x00 / 4;
#[allow(clippy::erasing_op)]
const REG_STATUS: u16 = 0x04 / 4;
#[allow(unused)]
const REG_UART_DATA: u16 = 0x08 / 4;
#[allow(unused)]
const REG_UART_STATUS_CONTROL: u16 = 0x09 / 4;
#[allow(unused)]
const REG_UART_TEST_MODE: u16 = 0x0a / 4;
const REG_MEMORY_PAGE: u16 = 0x0c / 4;
const REG_CODEC_RW: u16 = 0x10 / 4; // Codec Read/Write
const REG_LEGACY: u16 = 0x18 / 4;
const REG_SERIAL_CONTROL: u16 = 0x20 / 4; // Serial Interface
const REG_PLAYBACK1_FRAME_COUNT: u16 = 0x24 / 4;
const REG_PLAYBACK2_FRAME_COUNT: u16 = 0x28 / 4;
const REG_RECORD_FRAME_COUNT: u16 = 0x2c / 4;

const REG_PAGED_CHANNEL1_BUFFER_ADDR: u16 = 0x30 / 4;
const REG_PAGED_CHANNEL1_BUFFER_DEF: u16 = 0x34 / 4;
const REG_PAGED_CHANNEL2_BUFFER_ADDR: u16 = 0x38 / 4;
const REG_PAGED_CHANNEL2_BUFFER_DEF: u16 = 0x3c / 4;

const PAGE_PLAYBACK: u32 = 0xC;
const PAGE_RECORD: u32 = 0xD;

impl Es1370Core {
    pub fn read(&self, index: u16) -> u32 {
        let result = match index {
            REG_CONTROL => self.control.value,
            REG_STATUS => {
                let interrupt_pending = self.channels[0].interrupt_pending() || self.channels[1].interrupt_pending();
                let status = Status::new(
                    false,
                    self.channels[1].interrupt_pending(),
                    self.channels[0].interrupt_pending(),
                    false,
                    false,
                    u2::new(0),
                    false,
                    false,
                    false,
                    interrupt_pending,
                );

                status.value
            },
            // 2: UART data/status/test mode
            REG_MEMORY_PAGE => self.memory_page,
            REG_CODEC_RW => {
                let val = self.codec.regs[self.codec.index];
                ((self.codec.index as u32) << 8) | val as u32
            },
            REG_LEGACY => self.legacy_redirect, // TODO: Legacy register
            REG_SERIAL_CONTROL => self.serial_control.value,
            REG_PLAYBACK1_FRAME_COUNT => self.channels[0].sample_count_reg(),
            REG_PLAYBACK2_FRAME_COUNT => self.channels[1].sample_count_reg(),
            REG_RECORD_FRAME_COUNT => self.record.sample_count_reg(),
            REG_PAGED_CHANNEL1_BUFFER_ADDR => match self.memory_page {
                PAGE_PLAYBACK => self.channels[0].buffer_address(),
                PAGE_RECORD => self.record.buffer_address,
                _ => {
                    error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page);
                    u32::MAX
                },
            },
            REG_PAGED_CHANNEL1_BUFFER_DEF => match self.memory_page {
                PAGE_PLAYBACK => self.channels[0].buffer_def_reg(),
                PAGE_RECORD => self.record.buffer_dword_size() as u32,
                _ => {
                    error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page);
                    u32::MAX
                },
            },
            REG_PAGED_CHANNEL2_BUFFER_ADDR => match self.memory_page {
                PAGE_PLAYBACK => self.channels[1].buffer_address(),
                PAGE_RECORD => 0, // Phantom
                _ => {
                    error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page);
                    u32::MAX
                },
            },
            REG_PAGED_CHANNEL2_BUFFER_DEF => match self.memory_page {
                PAGE_PLAYBACK => self.channels[1].buffer_def_reg(),
                PAGE_RECORD => 0, // Phantom
                _ => {
                    error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page);
                    u32::MAX
                },
            },
            _ => {
                error!("TODO: Read ES1370 I/O space register 0x{index:X}");
                u32::MAX
            },
        };
        debug!("Read register 0x{index:X} (offset 0x{:X}) = 0x{result:X}", index * 4);

        result
    }

    pub fn write(&mut self, index: u16, data: u32, time: &EmulatorClock) {
        debug!("Write register 0x{index:X} = 0x{data:08X}");
        match index {
            REG_CONTROL => {
                let data = Control::from(data);
                self.control = data;
                error!("Control = {data:X?}");

                self.channels[0].set_control(
                    self.control.channel1_enabled(),
                    self.serial_control.p1_irq_enable(),
                    self.serial_control.p1_s_eb(),
                    self.serial_control.p1_s_mb(),
                );
                self.channels[1].set_control(
                    self.control.channel2_enabled(),
                    self.serial_control.p2_irq_enable(),
                    self.serial_control.p2_s_eb(),
                    self.serial_control.p2_s_mb(),
                );

                // TODO: We should enable the gameport here.
                self.update_backends(time);
            },
            REG_LEGACY => {
                self.legacy_redirect = data;
                info!(
                    "Legacy register written, setting IRQ line 0x{:X} to {}",
                    self.irq_line,
                    self.legacy_redirect & 0x0100_0000 != 0
                );
                self.irq.set(self.irq_line, self.legacy_redirect & 0x0100_0000 != 0);
            },
            REG_STATUS => warn!("Illegal write to status register"),
            REG_MEMORY_PAGE => self.memory_page = data & 0xf,
            REG_SERIAL_CONTROL => {
                let data = SerialControl::from(data);
                debug!("Serial control = {data:X?}");
                let old = self.serial_control;
                self.serial_control = data;

                self.channels[0].set_control(
                    self.control.channel1_enabled(),
                    self.serial_control.p1_irq_enable(),
                    self.serial_control.p1_s_eb(),
                    self.serial_control.p1_s_mb(),
                );
                self.channels[1].set_control(
                    self.control.channel2_enabled(),
                    self.serial_control.p2_irq_enable(),
                    self.serial_control.p2_s_eb(),
                    self.serial_control.p2_s_mb(),
                );

                if old.p1_irq_enable() && !self.serial_control.p1_irq_enable() {
                    debug!("Clearing pending interrupts for playback1");
                    self.channels[0].clear_pending_interrupt();
                }

                if old.p2_irq_enable() && !self.serial_control.p2_irq_enable() {
                    debug!("Clearing pending interrupts for playback2");
                    self.channels[1].clear_pending_interrupt();
                }

                self.update_backends(time);
            },
            REG_CODEC_RW => {
                let index = ((data >> 8) & 0xff) as usize;
                let val = data as u8;

                self.codec.index = index;
                self.codec.regs[index] = val;

                if (0..4).contains(&index) {
                    // TODO: Set wave volume
                }

                info!("TODO: Write 0x{val:X} to codec register #{index:X}");
            },
            REG_PLAYBACK1_FRAME_COUNT => {
                self.channels[0].set_sample_count_reg(data);
                info!("Channel 0 framecount: 0x{data:08X}");
            },
            REG_PLAYBACK2_FRAME_COUNT => {
                self.channels[1].set_sample_count_reg(data);
                info!("Channel 1 framecount: 0x{data:08X}");
            },
            REG_RECORD_FRAME_COUNT => self.record.set_sample_count_reg(data as u16),
            REG_PAGED_CHANNEL1_BUFFER_ADDR => match self.memory_page {
                PAGE_PLAYBACK => {
                    self.channels[0].set_buffer_address(data);
                    info!("Channel 0 buffer address: 0x{data:08X}");
                },
                PAGE_RECORD => self.record.buffer_address = data,
                _ => error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page),
            },
            REG_PAGED_CHANNEL1_BUFFER_DEF => match self.memory_page {
                PAGE_PLAYBACK => {
                    self.channels[0].set_buffer_def_reg(data);
                    info!("Channel 0 buffer def: 0x{data:08X}");
                },
                PAGE_RECORD => self.record.set_buffer_dword_size(data as u16),
                _ => error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page),
            },
            REG_PAGED_CHANNEL2_BUFFER_ADDR => match self.memory_page {
                PAGE_PLAYBACK => {
                    self.channels[1].set_buffer_address(data);
                    info!("Channel 1 buffer address: 0x{data:08X}");
                },
                PAGE_RECORD => (), // Phantom
                _ => error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page),
            },
            REG_PAGED_CHANNEL2_BUFFER_DEF => match self.memory_page {
                PAGE_PLAYBACK => {
                    self.channels[1].set_buffer_def_reg(data);
                    info!("Channel 1 buffer def: 0x{data:08X}");
                },
                PAGE_RECORD => (), // Phantom
                _ => error!("unknown page: 0x{:X} for register index 0x{index:X}", self.memory_page),
            },
            _ => error!("TODO: Write ES1370 I/O space register 0x{index:X} = 0x{data:X}"),
        }
    }

    fn update_backends(&mut self, time: &EmulatorClock) {
        // TODO: Let audio backend know new frequency if it changed
        // TODO: Let audio backend know new format (mono/stereo + 8/16bit) if it changed

        // TODO: Let audio backend know which channels are on
        let _p1_on = self.control.channel1_enabled() && self.serial_control.p1_pause() == PlayMode::Play;
        let _p2_on = self.control.channel2_enabled() && self.serial_control.p2_pause() == PlayMode::Play;
        let _r1_on = self.control.record_enabled();

        let [channel0, channel1] = &mut self.channels;
        for (name, channel, enabled, new_freq) in [
            (
                "playback1",
                channel0,
                self.control.channel1_enabled(),
                self.control.playback1_sample_rate().frequency(),
            ),
            (
                "playback2",
                channel1,
                self.control.channel2_enabled(),
                self.control.playback2_sample_rate(),
            ),
        ] {
            if enabled {
                if channel.device.is_none() || new_freq != channel.frontend.frequency.load(Ordering::SeqCst) {
                    info!("Switching {name} to {}Hz", new_freq);
                    if let Ok(device) = DeviceBackend::new(new_freq, channel.frontend.clone()) {
                        channel.frontend.frequency.store(new_freq, Ordering::SeqCst);
                        channel
                            .frontend
                            .required_buffer_size
                            .store(device.buffer_size() + 100, Ordering::SeqCst);
                        channel.device = Some(device);
                    } else {
                        error!("Unable to start audio backend with frequency {new_freq}");
                    }
                }

                if channel.timer_info.is_none() {
                    let info = TimerInfo {
                        enabled: Arc::new(AtomicBool::new(true)),
                        busmaster_enabled: self.busmaster_enabled.clone(),
                    };

                    info!("Starting timer for channel {name}");
                    time.register_timer(Box::new(ChannelTimer {
                        info: info.clone(),
                        next_tick: EmulatorTimestamp::now(time),
                        frontend: channel.frontend.clone(),
                    }));

                    channel.timer_info = Some(info);
                }
            } else if channel.device.is_some() || channel.timer_info.is_some() {
                info!("Dropping audio backend device and timer, because channel is disabled");
                channel.device = None;
                channel.timer_info = None;
            }
        }
    }
}

#[derive(Debug)]
pub struct Es1370 {
    pci_header: GeneralDeviceHeader,
    core: Es1370Core,
    #[allow(unused)]
    mem: Arc<Mem32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct Es1370Snapshot {
    pci_header: GeneralDeviceHeader,
    control: Control,
    memory_page: u32,
    serial_control: SerialControl,
    channels: [PlaybackChannelSnapshot; 2],
    record: ChannelSnapshot,
    legacy_redirect: u32,
    #[allow(unused)]
    wave_volume: u16,
    codec: Codec,
    irq_line: u8,
}

impl Es1370 {
    pub fn new(mem: Arc<Mem32>, irq: DualDynamicIrqLine) -> Self {
        Self {
            pci_header: GeneralDeviceHeader {
                common: CommonPciHeader {
                    vendor_id: 0x1274,
                    device_id: 0x5000,
                    command: PciCommandRegister::from(0),
                    status: 0x0400,
                    revision_id: 0,
                    prog_if: 0,
                    class_code: 0x04,
                    subclass: 0x01,
                    bist: 0,
                    cache_line_size: 0,
                    latency_timer: 0,
                    header_type: 0,
                },
                bar: [
                    0x1, // I/O space
                    0x0, 0x0, 0x0, 0x0, 0x0,
                ],
                cardbus_cis_pointer: 0,
                subsystem_vendor_id: 0x4942,
                subsystem_id: 0x4c4c,
                expansion_rom_base_address: 0,
                capabilities_pointer: 0,
                reserved1: [0; _],
                reserved2: 0,
                interrupt_line: 0xff,
                interrupt_pin: 1,
                min_grant: 0,
                max_latency: 0,
            },
            core: Es1370Core {
                control: Control::from(0),
                memory_page: 0,
                serial_control: SerialControl::from(0),
                channels: [
                    PlaybackChannel::new(&mem, irq.clone()),
                    PlaybackChannel::new(&mem, irq.clone()),
                ],
                record: Channel::new(),
                legacy_redirect: 0,
                wave_volume: 0,
                codec: Codec {
                    index: 0,
                    regs: [0; _],
                },
                irq,
                irq_line: 0x0B,
                busmaster_enabled: Arc::new(AtomicBool::new(false)),
            },
            mem,
        }
    }

    pub fn clear_pending_interrupts(&self) {
        self.core.channels[0].clear_pending_interrupt();
        self.core.channels[1].clear_pending_interrupt();
    }

    pub fn core(&mut self) -> &mut Es1370Core {
        &mut self.core
    }

    pub fn snapshot(&self) -> Es1370Snapshot {
        Es1370Snapshot {
            pci_header: self.pci_header,
            control: self.core.control,
            memory_page: self.core.memory_page,
            serial_control: self.core.serial_control,
            channels: std::array::from_fn(|n| self.core.channels[n].snapshot()),
            record: self.core.record.snapshot(),
            legacy_redirect: self.core.legacy_redirect,
            wave_volume: self.core.wave_volume,
            codec: self.core.codec.clone(),
            irq_line: self.core.irq_line,
        }
    }

    pub fn restore(&mut self, es1370: Es1370Snapshot, clock: &EmulatorClock) {
        self.pci_header = es1370.pci_header;
        self.core.control = es1370.control;
        self.core.memory_page = es1370.memory_page;
        self.core.serial_control = es1370.serial_control;
        for (dst, src) in self.core.channels.iter_mut().zip(es1370.channels) {
            dst.restore(src);
        }

        self.core.record.restore(es1370.record);
        self.core.legacy_redirect = es1370.legacy_redirect;
        self.core.wave_volume = es1370.wave_volume;
        self.core.codec = es1370.codec;
        self.core.irq_line = es1370.irq_line;

        self.core.update_backends(clock);
        self.core
            .busmaster_enabled
            .store(self.pci_header.common.command.enable_bus_master(), Ordering::SeqCst);
    }
}

impl PciDevice for Es1370 {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        debug!("Write ES1370 PCI register 0x{index:X} = 0x{val:X}");
        match self.pci_header.write(index, val) {
            Some(DeviceWriteEvent::Common(CommonWriteEvent::CommandStatus)) => {
                debug!(
                    "Command/status written: command={:X?}, status={:X?}",
                    self.pci_header.common.command, self.pci_header.common.status
                );
                self.core
                    .busmaster_enabled
                    .store(self.pci_header.common.command.enable_bus_master(), Ordering::SeqCst);
            },
            Some(DeviceWriteEvent::InterruptConfig) => {
                info!("Interrupt line = 0x{:02X}", self.pci_header.interrupt_line);
                self.core.channels[0].set_irq_line(self.pci_header.interrupt_line);
                self.core.channels[1].set_irq_line(self.pci_header.interrupt_line);
                self.core.irq_line = self.pci_header.interrupt_line;
            },
            Some(DeviceWriteEvent::Bar(0)) => {
                self.pci_header.bar[0] = (self.pci_header.bar[0] & !0x3f) | 1;
                info!("BAR0 = {:X}", self.pci_header.bar[0]);
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
            .expect("TODO: read outside ES1370 generic pci header");

        debug!("Read ES1370 PCI register 0x{index:X} = 0x{result:X}");

        result
    }
}

impl WithIoSpace for Es1370 {
    fn try_read<S: PortIoData>(&mut self, addr: u16, _mmio: &mut crate::hw::HwMmio) -> Option<Result<S, PortError>> {
        if !self.pci_header.common.command.enable_io_space() {
            return None
        }

        if addr & !0x7f == (self.pci_header.bar[0] & !0x7f) as u16 {
            Some(S::from_u32(addr & 3, || self.core.read((addr & 0x3f) / 4)))
        } else {
            None
        }
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, mmio: &mut crate::hw::HwMmio) -> Option<Result<(), PortError>> {
        if !self.pci_header.common.command.enable_io_space() {
            return None
        }

        if addr & !0x7f == (self.pci_header.bar[0] & !0x7f) as u16 {
            let index = (addr & 0x3f) / 4;
            let new_val = val.blend_into_u32(addr & 3, || self.core.read(index));
            self.core.write((addr & 0x3f) / 4, new_val, mmio.clock);

            Some(Ok(()))
        } else {
            None
        }
    }
}
