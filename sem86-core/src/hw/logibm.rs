use std::i8;

use bilge::prelude::*;
use bytemuck::Zeroable;
use log::{debug, info, warn};

use super::MouseMove;
use super::pic::DualIrqLine;

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum ReadBits {
    Low = 0,
    High = 1,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
enum Axis {
    X = 0,
    Y = 1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits)]
struct Control {
    irq_mask: u4,
    disable_irq: bool,
    bits: ReadBits,
    axis: Axis,
    hold: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Zeroable, PartialEq, Eq)]
struct Buttons {
    reserved: u5,
    right: bool,
    middle: bool,
    left: bool,
}

#[derive(Debug)]
pub struct LogiBM {
    control: Control,
    config: u8,
    signature: u8,
    pending_delta_x: f64,
    pending_delta_y: f64,
    delta_x: i8,
    delta_y: i8,
    pending_buttons: Buttons,
    current_buttons: Buttons,
    next_update: u64,
    toggle_counter: u16,
    irq_line: DualIrqLine,
}

impl LogiBM {
    pub fn new(irq_line: DualIrqLine) -> Self {
        Self {
            control: Control::new(u4::new(0xf), true, ReadBits::Low, Axis::X, false),
            config: 0xe,
            signature: 0xA5,
            delta_x: 0,
            delta_y: 0,
            pending_delta_x: 0.,
            pending_delta_y: 0.,
            current_buttons: Buttons::new(false, false, false),
            pending_buttons: Buttons::zeroed(),
            next_update: 0,
            toggle_counter: 0,
            irq_line,
        }
    }

    pub fn read(&mut self, port: u8) -> u8 {
        match port {
            // Data
            0 => {
                let val = match (self.control.axis(), self.control.bits()) {
                    (Axis::X, ReadBits::Low) => self.delta_x as u8 & 0xf,
                    (Axis::X, ReadBits::High) => self.delta_x as u8 >> 4,
                    (Axis::Y, ReadBits::Low) => self.delta_y as u8 & 0xf,
                    (Axis::Y, ReadBits::High) => (self.delta_y as u8 >> 4) | (self.current_buttons.value ^ 0xe0),
                };

                debug!("read value 0x{val:02X} from LogiBM data port");

                val
            },

            // Magic signature byte
            1 => self.signature,

            // Control
            2 => {
                let val = self.control;
                self.control
                    .set_irq_mask(u4::new(if self.toggle_counter > 0x3ff { 0xe } else { 0xf }));

                self.toggle_counter = (self.toggle_counter + 1) % 0x7ff;

                val.value
            },

            // Config
            3 => self.config,

            _ => unreachable!(),
        }
    }

    pub fn write(&mut self, port: u8, val: u8) {
        match port {
            // Data
            0 => warn!("Unexpected write to BusMouse data port"),

            // Signature
            1 => self.signature = val,

            // Control
            2 => {
                self.control.value = val;
                self.control.set_irq_mask(u4::new(0xf));
                // TODO: Clear IRQ line

                info!("control: {:?}", self.control);
            },

            // Config
            3 => self.config = val,

            _ => unreachable!(),
        }
    }

    pub fn update(&mut self, data: &MouseMove) {
        self.pending_delta_x += data.x / 3.;
        self.pending_delta_y += data.y / 3.;
        self.pending_buttons = Buttons::new(data.right_pressed, false, data.left_pressed);
    }

    pub fn data_pending(&self) -> bool {
        self.pending_delta_x.abs() >= 1. || self.pending_delta_y.abs() >= 1. || self.pending_buttons != self.current_buttons
    }

    pub fn apply_pending(&mut self) {
        if self.pending_delta_x >= i8::MAX as f64 {
            self.delta_x = i8::MAX;
            self.pending_delta_x -= i8::MAX as f64;
        } else if self.pending_delta_x <= i8::MIN as f64 {
            self.delta_x = i8::MIN;
            self.pending_delta_x -= i8::MIN as f64;
        } else {
            self.delta_x = self.pending_delta_x as i8;
            self.pending_delta_x = self.pending_delta_x.fract();
        }
        if self.pending_delta_y >= i8::MAX as f64 {
            self.delta_y = i8::MAX;
            self.pending_delta_y -= i8::MAX as f64;
        } else if self.pending_delta_y <= i8::MIN as f64 {
            self.delta_y = i8::MIN;
            self.pending_delta_y -= i8::MIN as f64;
        } else {
            self.delta_y = self.pending_delta_y as i8;
            self.pending_delta_y = self.pending_delta_y.fract();
        }

        self.current_buttons = self.pending_buttons;
    }

    pub fn check_data_to_send(&mut self, k: u64) {
        if k >= self.next_update && self.data_pending() && !self.control.disable_irq() {
            debug!("Raising logibm IRQ: {},{}", self.delta_x, self.delta_y);
            self.irq_line.set(true);
            self.next_update = k + 30_000;

            if !self.control.hold() {
                self.apply_pending();
            }
        }
    }
}
