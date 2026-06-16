use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{debug, error, info, trace, warn};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

mod mouse;

use super::MouseMove;
use super::reg::Reg8;
use crate::hw::pic::DualIrqLine;
use crate::hw::ppi::mouse::{Ps2Mouse, Ps2MouseSnapshot};
use crate::icache::InstructionCache;

const SWITCH_SELECT: u8 = 0x08;

#[bitsize(2)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum RamSize {
    Ram64K,
    Ram128K,
    Ram192K,
    Ram256K,
}

#[bitsize(2)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum VideoMode {
    Rom,
    Cg40,
    Cg80,
    Mda,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits)]
pub struct SwitchBits {
    has_floppy: bool,
    has_fpu: bool,
    ram_size: RamSize,
    video_mode: VideoMode,
    num_fdds: u2,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits)]
pub struct Status {
    output_byte_available: bool,
    input_byte_available: bool,
    system_flag: bool,
    command_data: bool,
    keyboard_lock: bool,
    aux_output_buffer_full: bool,
    timeout: bool,
    parity_error: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, FromBits, DebugBits)]
pub struct OutputP2 {
    reset: bool,
    a20: bool,
    mouse_data: bool,
    mouse_clock: bool,
    irq1: bool,
    irq12: bool,
    keyboard_clock: bool,
    keyboard_data: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub enum Command {
    WriteP2,
    WriteRam(u8),
    WriteAux,
}

#[derive(Debug)]
pub struct Ppi {
    memory: Arc<Mem32>,
    mouse: Ps2Mouse,
    irq_line: DualIrqLine,
    state: State,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    port_b: Reg8,
    keyboard_clear: bool,
    keyboard_clock_low: bool,
    output_buf: VecDeque<u8>,
    was_reset: bool,
    last_byte_was_command: bool,
    pending_command: Option<Command>,
    #[serde(with = "serde_big_array::BigArray")]
    ram: [u8; 0x40],
    ready: bool,
    system_control_a: Reg8,
    next_is_typematic: bool,
    next_is_leds: bool,
    aux_enabled: bool,
    a20_enabled: bool,
    must_read_keyboard: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PpiSnapshot {
    mouse: Ps2MouseSnapshot,
    state: State,
}

impl Ppi {
    pub fn new(memory: Arc<Mem32>, irq_line: DualIrqLine, mouse_irq_line: DualIrqLine) -> Self {
        Self {
            memory,
            irq_line,
            mouse: Ps2Mouse::new(mouse_irq_line),
            state: State {
                port_b: Reg8::new(0x31),
                keyboard_clear: false,
                keyboard_clock_low: false,
                output_buf: VecDeque::new(),
                was_reset: false,
                last_byte_was_command: true,
                ram: [0; 0x40],
                pending_command: None,
                ready: false,
                system_control_a: Reg8::new(0x02),
                next_is_typematic: false,
                next_is_leds: false,
                aux_enabled: true,
                a20_enabled: false,
                must_read_keyboard: false,
            },
        }
    }

    pub fn read_system_control_a(&mut self) -> u8 {
        self.state.system_control_a.read()
    }

    pub fn write_system_control_a(&mut self, val: u8, icache: &mut InstructionCache) {
        self.state.system_control_a.write(val);

        self.state.a20_enabled = val & 0x02 != 0;
        self.update_a20(icache);
    }

    pub fn read_a(&mut self) -> u8 {
        if self.state.pending_command.take().is_some() {
            self.state.last_byte_was_command = false;

            0
        } else if !self.state.must_read_keyboard
            && let Some(aux_output) = self.mouse.read_byte()
        {
            self.state.must_read_keyboard = false;
            info!("PPI data port (from AUX): 0x{aux_output:02X}");
            aux_output
        } else {
            let kb_output = self.state.output_buf.pop_front().unwrap_or(0);
            self.state.must_read_keyboard = !self.state.output_buf.is_empty();
            self.update_irq();

            info!("PPI data port (from KB): 0x{kb_output:02X}");
            kb_output
        }
    }

    pub fn write_a(&mut self, val: u8, icache: &mut InstructionCache) {
        trace!("Write port A: 0x{val:02X}");
        self.state.last_byte_was_command = false;
        if self.state.next_is_leds {
            info!("Keyboard LEDs: {val:08b}");
            self.state.output_buf.push_back(0xFA); // ACK
            self.state.next_is_leds = false;

            self.update_irq();
        } else if self.state.next_is_typematic {
            info!("Typematic = {val:02X}");
            self.state.output_buf.push_back(0xFA); // ACK
            self.state.next_is_typematic = false;

            self.update_irq();
        } else if let Some(cmd) = self.state.pending_command.take() {
            match cmd {
                Command::WriteRam(n) => {
                    info!("Write controller RAM 0x{n:02X} = {val:02X}");
                    self.state.ram[n as usize] = val;

                    if n == 0 {
                        let disable_keyboard = (val >> 4) & 1 != 0;
                        let sysf = (val >> 2) & 1 != 0;
                        self.state.keyboard_clock_low = disable_keyboard;
                        self.state.ready = sysf;

                        // TODO: Enable/disable IRQs
                    }
                },
                Command::WriteP2 => {
                    let output = OutputP2::from(val);
                    self.state.a20_enabled = output.a20();
                    self.update_a20(icache);

                    if !output.reset() {
                        panic!("CPU reset requested");
                    }
                },
                Command::WriteAux => {
                    if self.state.aux_enabled {
                        self.mouse.receive_byte(val);
                    }
                },
            }
        } else {
            match val {
                0xed => {
                    self.state.output_buf.push_back(0xFA); // ACK
                    self.state.next_is_leds = true;
                },
                0xf3 => {
                    // Typematic?
                    self.state.output_buf.push_back(0xFA); // ACK
                    self.state.next_is_typematic = true;
                },
                0xf4 => {
                    // TODO: Enable keyboard
                    self.state.output_buf.push_back(0xFA); // ACK
                },
                0xf5 => {
                    // Reset to power-up settings
                    self.state.output_buf.push_back(0xFA); // ACK
                },
                0xff => {
                    self.state.output_buf.push_back(0xFA); // ACK
                    self.state.output_buf.push_back(0xAA); // BAT test passsed
                },
                _ => {
                    info!("TODO: Write PPI A (write port 0x60): 0x{val:02X}");
                    self.state.output_buf.push_back(0xFE);
                },
            }

            self.update_irq();
        }
    }

    fn update_a20(&mut self, icache: &mut InstructionCache) {
        self.update_a20_without_notification();
        icache.notify_all_page_mappings_updated(&self.memory);
    }

    fn update_a20_without_notification(&mut self) {
        let active = self.state.a20_enabled;
        warn!("Setting A20 line to: {active}");
        self.memory.set_a20_line(active);
    }

    pub fn read_b(&mut self) -> u8 {
        info!("TODO: Read PPI B (read port 0x61)");
        let val = self.state.port_b.read();

        // Refresh clock?
        self.state.port_b.write(val ^ 0x10);

        val
    }

    pub fn write_b(&mut self, val: u8) {
        info!("TODO: Write PPI B (write port 0x61) = 0x{val:02X}");

        let prev_keyboard_clear = self.state.keyboard_clear;
        let prev_clock_low = self.state.keyboard_clock_low;
        self.state.keyboard_clear = val & 0x80 != 0;
        self.state.keyboard_clock_low = val & 0x40 == 0;

        if prev_clock_low != self.state.keyboard_clock_low && !self.state.keyboard_clock_low {
            self.state.was_reset = true;
        }

        if prev_keyboard_clear != self.state.keyboard_clear && !self.state.keyboard_clear {
            if self.state.was_reset {
                // Reset the keyboard
                self.state.output_buf.clear();
                self.state.output_buf.push_back(0xAA);

                self.state.was_reset = false;
            } else {
                // Just clear the pending scancode
                self.state.output_buf.pop_front();
            }

            self.update_irq();
        }

        self.state.port_b.write(val);
    }

    pub fn read_c(&mut self) -> u8 {
        let switch_values = SwitchBits::new(true, false, RamSize::Ram256K, VideoMode::Cg80, u2::new(0));

        if self.state.port_b.value() & SWITCH_SELECT != 0 {
            // High switches
            switch_values.value >> 4
        } else {
            // Low switches
            switch_values.value & 0xf
        }
    }

    pub fn write_c(&mut self, _val: u8) {}

    pub fn read_status(&mut self) -> u8 {
        Status::new(
            !self.state.output_buf.is_empty() || (self.mouse.bytes_available() && self.state.aux_enabled),
            false,
            self.state.ready,
            self.state.last_byte_was_command,
            true,
            self.mouse.bytes_available() && self.state.aux_enabled,
            false,
            false,
        )
        .value
    }

    pub fn write_command(&mut self, val: u8) {
        debug!("Received command: 0x{val:02X}");
        self.state.last_byte_was_command = true;
        self.state.pending_command = Some(match val {
            0x20..0x40 => {
                let n = val & 0x1f;
                let val = self.state.ram[n as usize];
                self.state.output_buf.push_back(val);
                self.update_irq();

                return;
            },
            0x60..0x80 => Command::WriteRam(val & 0x1f),
            0xD1 => Command::WriteP2,
            0xD4 => Command::WriteAux,

            // Self test
            0xAA => {
                self.state.output_buf.push_back(0x55);
                self.state.ready = true;
                self.update_irq();
                return
            },

            // Interface test
            0xAB => {
                self.state.output_buf.push_back(0x00);
                self.update_irq();
                return
            },

            // Disable keyboard
            0xAD => {
                self.state.keyboard_clock_low = true;
                // TODO: Set bit 4 of command byte
                return;
            },

            // Enable keyboard
            0xAE => {
                self.state.keyboard_clock_low = false;
                // TODO: Clear bit 4 of the command byte
                return;
            },

            // Disable mouse
            0xA7 => {
                self.state.aux_enabled = false;
                self.state.output_buf.clear();
                self.update_irq();

                return;
            },

            // Enable mouse
            0xA8 => {
                self.state.aux_enabled = true;
                self.state.output_buf.clear();
                self.update_irq();

                // TODO: Clear bit 5 of the command byte??
                return;
            },

            0xFE => {
                std::thread::sleep(Duration::from_secs(15));
                todo!("Reset command")
            },
            // Magical reset command that is not supposed to do anything
            0xF0..=0xFF => {
                // Ignore useless command
                return
            },
            _ => {
                // TODO
                error!("Not implemented: keyboard controller command {val:02X}");
                return
            },
        })
    }

    pub fn enqueue_scancode(&mut self, scancode: u8) {
        info!("KEYBOARD: Enqueued scancode 0x{scancode:02X}");
        self.state.output_buf.push_back(scancode);
        self.update_irq();
    }

    fn update_irq(&self) {
        self.irq_line.set(!self.state.output_buf.is_empty());
    }

    pub fn handle_mouse_input(&mut self, m: MouseMove) {
        self.mouse.handle_input(m);
    }

    pub fn snapshot(&self) -> PpiSnapshot {
        PpiSnapshot {
            state: self.state.clone(),
            mouse: self.mouse.snapshot(),
        }
    }

    pub fn restore(&mut self, ppi: PpiSnapshot) {
        self.state = ppi.state;
        self.mouse.restore(ppi.mouse);

        self.update_a20_without_notification();
    }
}
