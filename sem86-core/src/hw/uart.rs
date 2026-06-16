use std::collections::VecDeque;

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use bytemuck::Zeroable;
use log::{debug, error};
use serde::{Deserialize, Serialize};

use crate::hw::pic::DualIrqLine;

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct Ier {
    received_data_available: bool,
    transmitter_holding_register_empty: bool,
    receiver_line_status: bool,
    modem_status: bool,
    reserved: u4,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct Iir {
    interrupt_not_pending: bool,
    interrupt_id: u2,
    reserved: u5,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct LineControl {
    /// Actual linegth is 5 + this value, for a range of 5-8 bits.
    word_length_select: u2,
    stop_bit_num: bool,
    parity_enabled: bool,
    even_parity: bool,
    stick_parity: bool,
    set_break: bool,
    divisor_latch_access: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct ModemControl {
    data_terminal_ready: bool,
    request_to_send: bool,
    out1: u1,
    out2: u1,
    loopback: bool,
    reserved: u3,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct LineStatus {
    data_ready: bool,
    overrun_error: bool,
    parity_error: bool,
    framing_error: bool,
    break_interrupt: bool,
    transmitter_holding_register_empty: bool,
    transmitter_empty: bool,
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, Default, Zeroable, PartialEq, Eq)]
pub struct ModemStatus {
    delta_clear_to_send: bool,
    delta_data_set_ready: bool,
    trailing_edge_ring_indicator: bool,
    delta_data_carrier_detect: bool,
    clear_to_send: bool,
    data_set_ready: bool,
    ring_indicator: bool,
    data_carrier_detect: bool,
}

#[derive(Debug)]
pub struct Uart<D> {
    receive_buffer: VecDeque<u8>,
    send_buffer: VecDeque<u8>,
    interrupt_enable: Ier,
    line_control: LineControl,
    modem_control: ModemControl,
    modem_status: ModemStatus,
    scratch: u8,
    divisor_latch: u16,
    device: D,
    #[allow(unused)]
    irq_line: DualIrqLine,
}

// TODO
#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct UartSnapshot {}

pub trait UartDevice {
    fn set_request_to_send(&mut self, rts: bool);
    fn set_data_terminal_ready(&mut self, dtr: bool);
    fn begin_receive(&mut self);
    fn recv(&mut self) -> u8;
    fn data_remaining(&self) -> bool;
    fn data_available(&self) -> bool;
    fn end_receive(&mut self);
}

impl<D: UartDevice> Uart<D> {
    pub fn new(device: D, irq_line: DualIrqLine) -> Self {
        Self {
            receive_buffer: VecDeque::new(),
            send_buffer: VecDeque::new(),
            interrupt_enable: Ier::zeroed(),
            line_control: LineControl::zeroed(),
            modem_control: ModemControl::zeroed(),
            modem_status: ModemStatus::zeroed(),
            scratch: 0,
            divisor_latch: 0,
            device,
            irq_line,
        }
    }

    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    pub fn read_u8(&mut self, port: u8) -> u8 {
        match port {
            0 => {
                if self.line_control.divisor_latch_access() {
                    self.divisor_latch as u8
                } else {
                    let val = self.device.recv() & (u8::MAX >> (3 - self.line_control.word_length_select().value()));
                    debug!("received 0x{val:02X} from device");
                    val
                }
            },
            1 => {
                if self.line_control.divisor_latch_access() {
                    (self.divisor_latch >> 8) as u8
                } else {
                    self.interrupt_enable.value
                }
            },
            2 => {
                let received_data_available = self.interrupt_enable.received_data_available() && self.device.data_available();
                Iir::new(!received_data_available, u2::new(0b10)).value
            },
            3 => self.line_control.value,
            4 => self.modem_control.value,
            5 => {
                let status = LineStatus::new(
                    self.device.data_remaining(),
                    false,
                    false,
                    false,
                    self.receive_buffer.is_empty(),
                    self.send_buffer.is_empty(),
                    self.send_buffer.is_empty()
                    // TODO: Are we supposed to check the receive buffer here?
                        && self.receive_buffer.is_empty(),
                );

                debug!("current line status: {status:?}");

                status.value
            },
            6 => {
                // TODO: Clear modem status interrupt
                let mut status = self.modem_status;
                self.modem_status.set_delta_clear_to_send(false);
                self.modem_status.set_data_set_ready(false);
                self.modem_status.set_trailing_edge_ring_indicator(false);
                self.modem_status.set_delta_data_carrier_detect(false);

                if self.modem_control.loopback() {
                    status.set_clear_to_send(self.modem_control.request_to_send());
                    status.set_data_set_ready(self.modem_control.data_terminal_ready());
                    status.set_ring_indicator(self.modem_control.out1() == u1::new(1));
                    status.set_data_carrier_detect(self.modem_control.out2() == u1::new(1));
                }

                status.value
            },
            7 => self.scratch,
            _ => unreachable!(),
        }
    }

    pub fn write_u8(&mut self, port: u8, val: u8) {
        match port {
            0 => {
                if self.line_control.divisor_latch_access() {
                    self.divisor_latch = (self.divisor_latch & !0xff) | val as u16;

                    debug!("Divisor latch: {}", self.divisor_latch);
                } else {
                    self.send_buffer.push_back(val);
                    error!("TODO: Send UART data")
                }
            },
            1 => {
                if self.line_control.divisor_latch_access() {
                    self.divisor_latch = (self.divisor_latch & !0xff00) | ((val as u16) << 8);

                    debug!("Divisor latch: {}", self.divisor_latch);
                } else {
                    self.interrupt_enable.value = val;

                    debug!("IER: {:?}", self.interrupt_enable);
                    if self.interrupt_enable.modem_status()
                        || self.interrupt_enable.receiver_line_status()
                        || self.interrupt_enable.transmitter_holding_register_empty()
                    {
                        error!("TODO: Unimplemented UART interrupts enabled: {:?}", self.interrupt_enable)
                    }
                }
            },

            // The IIR is read-only
            2 => (),
            3 => {
                self.line_control.value = val;
                debug!("Line control: {:?}", self.line_control);
            },
            4 => {
                self.modem_control.value = val;
                debug!("Modem control: {:?}", self.modem_control);
                self.device.set_data_terminal_ready(self.modem_control.data_terminal_ready());
                self.device.set_request_to_send(self.modem_control.request_to_send());
            },

            // Not sure what a write to the line status register would do
            5 => todo!("Write to serial line status"),
            6 => {
                self.modem_status.value = val;
                debug!("Modem status: {:?}", self.modem_status);
            },
            7 => self.scratch = val,
            _ => unreachable!(),
        }
    }

    pub fn snapshot(&self) -> UartSnapshot {
        UartSnapshot {}
    }

    pub fn restore(&mut self, _com1: UartSnapshot) {
        // TODO
    }
}
