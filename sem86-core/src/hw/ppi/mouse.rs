use std::collections::VecDeque;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};

use crate::hw::MouseMove;
use crate::hw::pic::DualIrqLine;

#[derive(Debug)]
pub struct Ps2Mouse {
    irq_line: DualIrqLine,
    state: State,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    pending_delta_x: f64,
    pending_delta_y: f64,
    pending_delta_z: f64,
    send_buf: VecDeque<u8>,
    recv_buf: VecDeque<u8>,
    data_reporting_enabled: bool,
    left_pressed: bool,
    right_pressed: bool,
    next_update: u64,
    resolution: u8,
    sample_rate: u8,
    scaling: Scaling,
    mode: Mode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Mode {
    Standard { im_unlock_progress: u8 },
    IntelliMouse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ps2MouseSnapshot {
    state: State,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, Serialize, Deserialize)]
enum Scaling {
    Scale1_1 = 0,
    Scale2_1 = 1,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum ExpandedScaling {
    Scale1_1 = 0,
    Scale2_1 = 1,
}

impl From<Scaling> for ExpandedScaling {
    fn from(value: Scaling) -> Self {
        match value {
            Scaling::Scale1_1 => ExpandedScaling::Scale1_1,
            Scaling::Scale2_1 => ExpandedScaling::Scale2_1,
        }
    }
}

impl From<ExpandedScaling> for Scaling {
    fn from(value: ExpandedScaling) -> Self {
        match value {
            ExpandedScaling::Scale1_1 => Scaling::Scale1_1,
            ExpandedScaling::Scale2_1 => Scaling::Scale2_1,
        }
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits)]
struct MousePacket {
    left_pressed: bool,
    right_pressed: bool,
    middle_pressed: bool,
    always_true: bool,
    x_sign_bit: bool,
    y_sign_bit: bool,
    x_overflow: bool,
    y_overflow: bool,
    x_movement: u8,
    y_movement: u8,
    z_movement: u8,
}

#[bitsize(24)]
#[derive(Copy, Clone, DebugBits)]
struct StatusResponse {
    left_pressed: bool,
    right_pressed: bool,
    middle_pressed: bool,
    reserved: bool,
    scaling: Scaling,
    enable: bool,
    mode: bool,
    reserved: bool,
    resolution: u8,
    sample_rate: u8,
}

impl Ps2Mouse {
    pub fn new(irq_line: DualIrqLine) -> Self {
        Self {
            irq_line,
            state: State {
                pending_delta_x: 0.,
                pending_delta_y: 0.,
                pending_delta_z: 0.,
                send_buf: VecDeque::new(),
                recv_buf: VecDeque::new(),
                data_reporting_enabled: false,
                left_pressed: false,
                right_pressed: false,
                next_update: 0,
                resolution: 0,
                sample_rate: 0,
                scaling: Scaling::Scale1_1,
                mode: Mode::Standard {
                    im_unlock_progress: 0,
                },
            },
        }
    }

    pub fn receive_byte(&mut self, byte: u8) {
        debug!("Received byte: 0x{byte:X}, after {:02X?}", self.state.recv_buf);
        self.state.recv_buf.push_back(byte);

        match self.state.recv_buf[0] {
            0xE6 => {
                info!("TODO: Set scaling 1:1");
                self.state.scaling = Scaling::Scale1_1;
            },
            0xE7 => {
                info!("TODO: Set scaling 2:1");
                self.state.scaling = Scaling::Scale2_1;
            },
            0xE8 => {
                self.state.send_buf.push_back(0xFA);

                if self.state.recv_buf.len() < 2 {
                    self.update_irq();
                    return
                } else {
                    info!("Set resolution to {}", self.state.recv_buf[1]);
                    self.state.resolution = self.state.recv_buf[1];
                    self.state.recv_buf.clear();
                    self.update_irq();
                    return
                }
            },
            0xE9 => {
                // Status request
                self.state.send_buf.push_back(0xFA);
                let packet = StatusResponse::new(
                    self.state.left_pressed,
                    self.state.right_pressed,
                    false,
                    self.state.scaling,
                    self.state.data_reporting_enabled,
                    false, // TODO: Remote
                    self.state.resolution,
                    self.state.sample_rate,
                );
                self.state.send_buf.extend(&packet.value.to_le_bytes()[..3]);
                self.state.recv_buf.clear();
                self.update_irq();
                return;
            },
            0xF2 => {
                info!("Responding with device ID 0");
                self.state.send_buf.push_back(0xFA);
                self.state.send_buf.push_back(match self.state.mode {
                    Mode::Standard {
                        ..
                    } => 0x00,
                    Mode::IntelliMouse => 0x03,
                });
                self.state.recv_buf.clear();
                self.update_irq();
                return;
            },
            0xF3 => {
                self.state.send_buf.push_back(0xFA);

                if self.state.recv_buf.len() < 2 {
                    self.update_irq();
                    return
                } else {
                    info!("Set sample rate to {}", self.state.recv_buf[1]);
                    self.state.sample_rate = self.state.recv_buf[1];
                    self.state.recv_buf.clear();

                    match (self.state.sample_rate, &mut self.state.mode) {
                        (
                            200,
                            Mode::Standard {
                                im_unlock_progress: p @ 0,
                            },
                        )
                        | (
                            100,
                            Mode::Standard {
                                im_unlock_progress: p @ 1,
                            },
                        ) => *p += 1,
                        (
                            80,
                            Mode::Standard {
                                im_unlock_progress: 2,
                            },
                        ) => {
                            info!("Switched to IntelliMouse mode");
                            self.state.mode = Mode::IntelliMouse;
                            self.state.pending_delta_z = 0.;
                        },
                        (
                            _,
                            Mode::Standard {
                                im_unlock_progress,
                            },
                        ) => *im_unlock_progress = 0,
                        _ => (),
                    }

                    self.update_irq();
                    return
                }
            },
            0xF4 => {
                info!("Data reporting enabled");
                self.state.data_reporting_enabled = true;
            },
            0xF5 => {
                info!("Data reporting disabled");
                self.state.data_reporting_enabled = false;
            },
            0xFF => {
                info!("Entering reset mode");
                self.state.sample_rate = 100;
                self.state.resolution = 4;
                self.state.scaling = Scaling::Scale1_1;
                self.state.data_reporting_enabled = false;
                self.state.send_buf.extend([0xFA, 0xAA, 0x00]);
                self.state.recv_buf.clear();
                self.state.mode = Mode::Standard {
                    im_unlock_progress: 0,
                };
                self.update_irq();
                return
            },
            other => {
                error!("PS/2 mouse: unknown command 0x{other:X}, responding with NACK");
                self.state.send_buf.push_back(0xFE);
                self.state.recv_buf.clear();
                self.update_irq();
                return
            },
        }

        self.state.send_buf.push_back(0xFA);
        self.state.recv_buf.clear();
        self.update_irq();
    }

    pub fn bytes_available(&self) -> bool {
        !self.state.send_buf.is_empty()
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        let val = self
            .state
            .send_buf
            .pop_front()
            .inspect(|val| debug!("Read byte 0x{val:02X} from buffer, pending: {:02X?}", self.state.send_buf));
        self.update_irq();

        val
    }

    fn update_irq(&mut self) {
        let val = self.bytes_available();
        self.irq_line.set(val);
    }

    pub fn handle_input(&mut self, m: MouseMove) {
        let mut send_update = false;
        self.state.pending_delta_x += m.x * 0.6;
        self.state.pending_delta_y -= m.y * 0.6;
        self.state.pending_delta_z -= m.z as f64;

        if self.state.left_pressed != m.left_pressed {
            self.state.left_pressed = m.left_pressed;
            send_update = true;
        }

        if self.state.right_pressed != m.right_pressed {
            self.state.right_pressed = m.right_pressed;
            send_update = true;
        }

        if self.state.data_reporting_enabled && send_update {
            self.send_update();
        }

        while self.state.data_reporting_enabled
            && (self.state.pending_delta_x.abs() >= 1.
                || self.state.pending_delta_y.abs() >= 1.
                || self.state.pending_delta_z.abs() >= 1.)
        {
            self.send_update();
        }
    }

    fn send_update(&mut self) {
        let delta_x = self.state.pending_delta_x.clamp(-256., 255.).trunc() as i16;
        let delta_y = self.state.pending_delta_y.clamp(-256., 255.).trunc() as i16;
        let delta_z = self.state.pending_delta_z.clamp(-8., 7.).trunc() as i8;
        let packet = MousePacket::new(
            self.state.left_pressed,
            self.state.right_pressed,
            false,
            true,
            (delta_x >> 8) & 1 != 0,
            (delta_y >> 8) & 1 != 0,
            false,
            false,
            delta_x as u16 as u8,
            delta_y as u16 as u8,
            delta_z as u8,
        );

        let packet_size = match self.state.mode {
            Mode::Standard {
                ..
            } => 3,
            Mode::IntelliMouse => 4,
        };
        let bytes = &packet.value.to_le_bytes()[..packet_size];
        debug!("Sending mouse update packet: {packet:X?} = {bytes:02X?}");

        self.state.send_buf.extend(bytes);

        self.state.pending_delta_x -= delta_x as f64;
        self.state.pending_delta_y -= delta_y as f64;
        self.state.pending_delta_z -= delta_z as f64;

        debug!(
            "Remaining delta: ({:.2}, {:.2})",
            self.state.pending_delta_x, self.state.pending_delta_y
        );

        self.update_irq();
    }

    pub fn snapshot(&self) -> Ps2MouseSnapshot {
        Ps2MouseSnapshot {
            state: self.state.clone(),
        }
    }

    pub fn restore(&mut self, mouse: Ps2MouseSnapshot) {
        self.state = mouse.state;
    }
}
