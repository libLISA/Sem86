use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use bitcode::{Decode, Encode};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};

use crate::hw::intr::{IntrHandle, PendingRequest};

pub const PIC_INTERRUPT_ID_TIMER: u8 = 0;
pub const PIC_INTERRUPT_ID_KBM_RTC: u8 = 1;
pub const PIC_INTERRUPT_ID_VIDEO: u8 = 2;
pub const PIC_INTERRUPT_ID_SERIAL2: u8 = 3;
pub const PIC_INTERRUPT_ID_SERIAL1: u8 = 4;
pub const PIC_INTERRUPT_ID_HDD: u8 = 5;
pub const PIC_INTERRUPT_ID_FDD: u8 = 6;
pub const PIC_INTERRUPT_ID_PRINTER: u8 = 7;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum State {
    Ready,
    Icw2 { need_icw4: bool },
    Icw3 { need_icw4: bool },
    Icw4,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum Cascade {
    /// The IRQ line which we use to cascade events to the main PIC.
    Single { cascade_id: u8 },
    /// If bit N in cascade_map is 1, then another PIC is connected to that IRQ line.
    Cascade { cascade_map: u8 },
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum Adi {
    Interval4,
    Interval8,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, Encode, Decode)]
enum TriggerMode {
    Level,
    Edge,
}

/// returns `(priority, line)`
fn highest_priority_line(lines: u16) -> (u8, Option<u8>) {
    INTERRUPT_LINE_PRIORITY_ORDER
        .iter()
        .copied()
        .enumerate()
        .find(|(_, line)| (lines & (1 << line)) != 0)
        .map(|(p, l)| (p as u8, Some(l)))
        .unwrap_or((0xff, None))
}

const INTERRUPT_LINE_PRIORITY_ORDER: [u8; 16] = [0, 1, 2, 8, 9, 10, 11, 12, 13, 14, 15, 3, 4, 5, 6, 7];

#[derive(Debug)]
pub struct SharedPicCore {
    intr: IntrHandle,
    pending_request: Mutex<Option<PendingRequest>>,
    pending_interrupts: [AtomicU8; 2],
    in_service: [AtomicU8; 2],
    imr: [AtomicU8; 2],
    vector_offset: [AtomicU8; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct SharedPicCoreSnapshot {
    has_pending_request: bool,
    pending_interrupts: [u8; 2],
    in_service: [u8; 2],
    imr: [u8; 2],
    vector_offset: [u8; 2],
}

impl SharedPicCore {
    pub fn new(intr: IntrHandle) -> Self {
        Self {
            intr,
            pending_request: Mutex::new(None),
            pending_interrupts: [AtomicU8::new(0), AtomicU8::new(0)],
            in_service: [AtomicU8::new(0), AtomicU8::new(0)],
            imr: [AtomicU8::new(0), AtomicU8::new(0)],
            vector_offset: [AtomicU8::new(0), AtomicU8::new(0)],
        }
    }

    fn update_pending_interrupt(&self) {
        let pending = self.effective_pending_interrupts();

        let in_service_priority = self.in_service_interrupt_priority();

        let (pending_priority, _) = highest_priority_line(pending);

        debug!(
            "Effective pending interrupts: {pending:016b} - in service priority: {in_service_priority:X?}, pending priority: {pending_priority:X?}"
        );

        let mut req = self.pending_request.lock().unwrap();
        if pending != 0 && pending_priority < in_service_priority {
            if req.is_none() {
                debug!("Obtaining pending interrupt request");
                *req = Some(self.intr.request());
            } else {
                debug!("Already have pending interrupt request");
            }
        } else if req.take().is_some() {
            debug!("Released pending interrupt request");
        }
    }

    fn in_service_interrupt_priority(&self) -> u8 {
        let in_service = u16::from_le_bytes([
            self.in_service[0].load(Ordering::Relaxed),
            self.in_service[1].load(Ordering::Relaxed),
        ]);
        let (in_service_priority, _) = highest_priority_line(in_service);
        in_service_priority
    }

    pub fn get_next_interrupt(&self) -> Option<u8> {
        let pending = self.effective_pending_interrupts();
        if pending != 0 {
            let (pending_priority, Some(line)) = highest_priority_line(pending) else {
                unreachable!("pending is not zero, so some interrupt should be pending")
            };

            if pending_priority < self.in_service_interrupt_priority() {
                debug!(
                    "Effective pending interrupts: {pending:016b}, we will start servicing line 0x{line:X}, which is the highest-priority pending interrupt"
                );

                self.start_servicing_interrupt(line);

                Some((line % 8) + self.vector_offset[line as usize / 8].load(Ordering::SeqCst))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn effective_pending_interrupts(&self) -> u16 {
        self.pending_interrupts
            .iter()
            .zip(self.in_service.iter())
            .zip(self.imr.iter())
            .map(|((pending_interrupts, in_service), imr)| {
                pending_interrupts.load(Ordering::Relaxed) & !in_service.load(Ordering::Relaxed) & !imr.load(Ordering::Relaxed)
            })
            .enumerate()
            .fold(0, |acc, (index, val)| acc | ((val as u16) << (index * 8)))
    }

    fn start_servicing_interrupt(&self, index: u8) {
        // Once we trigger an interrupt, we shouldn't trigger it again until the end-of-interrupt is marked.
        self.in_service[index as usize / 8].fetch_or(1u8 << (index % 8), Ordering::Relaxed);
        self.update_pending_interrupt();
    }

    pub fn snapshot(&self) -> SharedPicCoreSnapshot {
        SharedPicCoreSnapshot {
            has_pending_request: self.pending_request.lock().unwrap().is_some(),
            pending_interrupts: [
                self.pending_interrupts[0].load(Ordering::SeqCst),
                self.pending_interrupts[1].load(Ordering::SeqCst),
            ],
            in_service: [
                self.in_service[0].load(Ordering::SeqCst),
                self.in_service[1].load(Ordering::SeqCst),
            ],
            imr: [self.imr[0].load(Ordering::SeqCst), self.imr[1].load(Ordering::SeqCst)],
            vector_offset: [
                self.vector_offset[0].load(Ordering::SeqCst),
                self.vector_offset[1].load(Ordering::SeqCst),
            ],
        }
    }

    pub fn restore(&self, core: SharedPicCoreSnapshot) {
        if core.has_pending_request {
            let mut req = self.pending_request.lock().unwrap();
            *req = Some(self.intr.request());
        }

        self.pending_interrupts[0].store(core.pending_interrupts[0], Ordering::SeqCst);
        self.pending_interrupts[1].store(core.pending_interrupts[1], Ordering::SeqCst);
        self.in_service[0].store(core.in_service[0], Ordering::SeqCst);
        self.in_service[1].store(core.in_service[1], Ordering::SeqCst);
        self.imr[0].store(core.imr[0], Ordering::SeqCst);
        self.imr[1].store(core.imr[1], Ordering::SeqCst);
        self.vector_offset[0].store(core.vector_offset[0], Ordering::SeqCst);
        self.vector_offset[1].store(core.vector_offset[1], Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub struct Pic {
    state: State,
    cascade_mode: Cascade,
    adi: Adi,
    trigger_mode: TriggerMode,
    register_select: u8,
    read_register: bool,
    poll_command: bool,
    controller_index: usize,
    shared_core: Arc<SharedPicCore>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct PicSnapshot {
    state: State,
    cascade_mode: Cascade,
    adi: Adi,
    trigger_mode: TriggerMode,
    register_select: u8,
    read_register: bool,
    poll_command: bool,
    controller_index: usize,
}

impl Pic {
    pub fn new(controller_index: usize, vector_offset: u8, shared_core: Arc<SharedPicCore>) -> Self {
        shared_core.vector_offset[controller_index].store(vector_offset, Ordering::SeqCst);
        Self {
            controller_index,
            state: State::Ready,
            cascade_mode: Cascade::Single {
                cascade_id: 0,
            },
            adi: Adi::Interval8,
            trigger_mode: TriggerMode::Edge,
            register_select: 0,
            read_register: false,
            poll_command: false,
            shared_core,
        }
    }

    pub fn read(&mut self, addr: u8) -> u8 {
        if addr == 0 {
            if self.read_register {
                match self.register_select {
                    0 => self.shared_core.pending_interrupts[self.controller_index].load(Ordering::SeqCst),
                    1 => self.shared_core.in_service[self.controller_index].load(Ordering::SeqCst),
                    _ => unreachable!(),
                }
            } else {
                0
            }
        } else {
            self.shared_core.imr[self.controller_index].load(Ordering::SeqCst)
        }
    }

    pub fn write(&mut self, addr: u8, val: u8) {
        self.state = if addr == 0 {
            match self.state {
                State::Ready => {
                    if val & 0x10 != 0 {
                        self.cascade_mode = if val & 0x02 != 0 {
                            Cascade::Single {
                                cascade_id: 0,
                            }
                        } else {
                            Cascade::Cascade {
                                cascade_map: 0,
                            }
                        };

                        self.adi = if val & 0x04 != 0 { Adi::Interval4 } else { Adi::Interval8 };

                        self.trigger_mode = if val & 0x08 != 0 {
                            TriggerMode::Level
                        } else {
                            TriggerMode::Edge
                        };

                        // Initialization command word
                        State::Icw2 {
                            need_icw4: val & 1 != 0,
                        }
                    } else if val & 0x18 == 0x00 {
                        let index = val & 0x7;
                        match val >> 5 {
                            // Non-specific EOI
                            0b001 => {
                                let highest_set = self.shared_core.in_service[self.controller_index]
                                    .load(Ordering::Relaxed)
                                    .trailing_zeros() as u8;
                                if highest_set < 8 {
                                    self.end_of_interrupt(highest_set);
                                }
                            },

                            // Specific EOI
                            0b011 => self.end_of_interrupt(index),

                            // Rotate on non-specific EOI command
                            0b101 => error!("TODO: PIC rotate on non-specific EOI"), // TODO

                            // Rotate in automatic EOI mode (set)
                            0b100 => error!("TODO: PIC rotate in automatic EOI mode (set)"), // TODO

                            // Rotate in automatic EOI mode (clear)
                            0b000 => error!("TODO: PIC rotate in automatic EOI mode (clear)"), // TODO

                            // Rotate on specific EOI command
                            0b111 => error!("TODO: PIC rotate on specific EOI command"), // TODO

                            // Set priority command
                            0b110 => error!("TODO: PIC set priority"), // TODO

                            // No operation
                            0b010 => (),
                            _ => unreachable!(),
                        }

                        State::Ready
                    } else if val & 0x18 == 0x08 {
                        // OCW3

                        self.register_select = val & 1;
                        self.read_register = val & 2 != 0;
                        self.poll_command = val & 4 != 0;
                        let _special_mask_mode = val & 0x20;
                        let _enable_special_mask_mode = val & 0x40;

                        State::Ready
                    } else {
                        // Unknown operation command word
                        State::Ready // TODO
                    }
                },
                other => other,
            }
        } else {
            match self.state {
                State::Ready => {
                    self.shared_core.imr[self.controller_index].store(val, Ordering::SeqCst);
                    self.shared_core.update_pending_interrupt();
                    State::Ready
                },
                State::Icw2 {
                    need_icw4,
                } => {
                    self.shared_core.vector_offset[self.controller_index].store(val & 0xf8, Ordering::SeqCst);
                    warn!("PIC vector offset changed: 0x{:X}", val & 0xf8);

                    match self.cascade_mode {
                        Cascade::Single {
                            ..
                        } => {
                            if need_icw4 {
                                State::Icw4
                            } else {
                                State::Ready
                            }
                        },
                        Cascade::Cascade {
                            ..
                        } => State::Icw3 {
                            need_icw4,
                        },
                    }
                },
                State::Icw3 {
                    need_icw4,
                } => {
                    match &mut self.cascade_mode {
                        Cascade::Single {
                            cascade_id,
                        } => *cascade_id = val & 0x07,
                        Cascade::Cascade {
                            cascade_map,
                        } => *cascade_map = val & 0x7f,
                    }

                    if need_icw4 { State::Icw4 } else { State::Ready }
                },
                State::Icw4 => {
                    // TODO: Ignore for now
                    State::Ready
                },
            }
        }
    }

    fn end_of_interrupt(&self, index: u8) {
        debug!("End-Of-Interrupt: {index}");
        self.shared_core.in_service[self.controller_index].fetch_and(!(1 << index), Ordering::Relaxed);
        self.shared_core.pending_interrupts[self.controller_index].fetch_and(!(1 << index), Ordering::Relaxed);

        self.shared_core.update_pending_interrupt();
    }

    pub fn get_line(&self, index: u8) -> IrqLine {
        IrqLine {
            lines: DynamicIrqLine {
                shared_core: self.shared_core.clone(),
            },
            index: index + self.controller_index as u8 * 8,
        }
    }

    pub fn vector_offset(&self) -> u8 {
        self.shared_core.vector_offset[self.controller_index].load(Ordering::SeqCst)
    }

    pub fn clear_pending(&self) {
        self.shared_core.pending_interrupts[self.controller_index].store(0, Ordering::Relaxed);
        self.shared_core.in_service[self.controller_index].store(0, Ordering::Relaxed);

        self.shared_core.update_pending_interrupt();
    }

    pub fn snapshot(&self) -> PicSnapshot {
        PicSnapshot {
            state: self.state,
            cascade_mode: self.cascade_mode,
            adi: self.adi,
            trigger_mode: self.trigger_mode,
            register_select: self.register_select,
            read_register: self.read_register,
            poll_command: self.poll_command,
            controller_index: self.controller_index,
        }
    }

    pub fn restore(&mut self, pic: PicSnapshot) {
        self.state = pic.state;
        self.cascade_mode = pic.cascade_mode;
        self.adi = pic.adi;
        self.trigger_mode = pic.trigger_mode;
        self.register_select = pic.register_select;
        self.read_register = pic.read_register;
        self.poll_command = pic.poll_command;
        self.controller_index = pic.controller_index;
    }
}

#[derive(Clone, Debug)]
pub struct IrqLine {
    lines: DynamicIrqLine,
    index: u8,
}

impl IrqLine {
    // TODO: We should keep track of the actual current values so we can deassert interrupts if the code issues an EOI without having actually received the interrupt.
    pub fn pulse(&self) {
        self.lines.pulse(self.index);
    }

    pub fn set_high(&mut self) {
        self.lines.set_high(self.index);
    }

    pub fn set_low(&mut self) {
        self.lines.set_low(self.index);
    }

    pub fn set(&mut self, val: bool) {
        self.lines.set(self.index, val);
    }
}

#[derive(Clone, Debug)]
pub struct DynamicIrqLine {
    shared_core: Arc<SharedPicCore>,
}

impl DynamicIrqLine {
    pub fn from_shared_core(shared_core: Arc<SharedPicCore>) -> Self {
        Self {
            shared_core,
        }
    }

    // TODO: We should keep track of the actual current values so we can deassert interrupts if the code issues an EOI without having actually received the interrupt.
    pub fn pulse(&self, index: u8) {
        let bit = index % 8;
        let n = index / 8;
        let pending_interrupts = &self.shared_core.pending_interrupts[n as usize];

        let val = 1u8 << bit;
        pending_interrupts.fetch_or(val, Ordering::Relaxed);

        self.shared_core.update_pending_interrupt();
    }

    pub fn set_high(&mut self, index: u8) {
        self.set(index, true);
    }

    pub fn set_low(&mut self, index: u8) {
        self.set(index, false);
    }

    pub fn set(&mut self, index: u8, val: bool) {
        if val {
            self.pulse(index);
        }
    }
}
