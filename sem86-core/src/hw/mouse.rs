use std::collections::VecDeque;

use log::info;

use super::MouseMove;
use super::uart::UartDevice;

#[derive(Clone, Debug)]
pub struct UartMouse {
    send_buffer: VecDeque<u8>,
    can_send: bool,
}

impl Default for UartMouse {
    fn default() -> Self {
        Self::new()
    }
}

impl UartMouse {
    pub fn new() -> Self {
        UartMouse {
            send_buffer: VecDeque::new(),
            can_send: false,
        }
    }

    pub fn update_position(&mut self, update: &MouseMove) {
        self.send_buffer.extend(&[
            0xc0 | ((update.left_pressed as u8) << 5)
                | ((update.right_pressed as u8) << 4)
                | ((update.y as u8 & 0xc0) >> 4)
                | ((update.x as u8 & 0xc0) >> 6),
            update.x as u8 & 0x3f,
            update.y as u8 & 0x3f,
        ]);
    }
}

impl UartDevice for UartMouse {
    fn set_request_to_send(&mut self, rts: bool) {
        if !self.can_send && rts {
            info!("Mouse resetting");
            self.send_buffer.clear();
            self.send_buffer.extend([b'M', 0xc0, 0x00, 0x00]);
        }

        self.can_send = rts;
    }

    fn set_data_terminal_ready(&mut self, _dtr: bool) {}

    fn begin_receive(&mut self) {}

    fn recv(&mut self) -> u8 {
        if self.can_send {
            self.send_buffer.pop_front().unwrap_or(0)
        } else {
            0x00
        }
    }

    fn data_remaining(&self) -> bool {
        !self.send_buffer.is_empty() && self.can_send
    }

    fn data_available(&self) -> bool {
        !self.send_buffer.is_empty()
    }

    fn end_receive(&mut self) {
        self.send_buffer.clear()
    }
}
