use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::trace;
use serde::{Deserialize, Serialize};

use crate::hw::pic::io::{IoApic, IoApicCore, RedirectionEntry, TriggerMode};
use crate::hw::pic::legacy::{DynamicIrqLine, IrqLine};
use crate::hw::pic::local::{LocalApic, LocalApicCore};

pub mod io;
pub mod legacy;
pub mod local;

#[bitsize(3)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum DeliveryMode {
    /// Deliver the signal on the INTR signal of all processor cores listed in the
    /// destination. Trigger Mode for "fixed" Delivery Mode can be edge or level.
    Normal = 0,

    /// Deliver the signal on the INTR signal of the processor core that is
    /// executing at the lowest priority among all the processors listed in the
    /// specified destination. Trigger Mode for "lowest priority". Delivery Mode
    /// can be edge or level.
    LowPriority = 1,

    /// System Management Interrupt. A delivery mode equal to SMI requires an
    /// edge trigger mode. The vector information is ignored but must be
    /// programmed to all zeroes for future compatibility.
    SMI = 2,
    Reserved3 = 3,

    /// Deliver the signal on the NMI signal of all processor cores listed in the
    /// destination. Vector information is ignored. NMI is treated as an edge
    /// triggered interrupt, even if it is programmed as a level triggered interrupt.
    /// For proper operation, this redirection table entry must be programmed to
    /// "edge" triggered interrupt.
    NMI = 4,

    /// Deliver the signal to all processor cores listed in the destination by
    /// asserting the INIT signal. All addressed local APICs will assume their
    /// INIT state. INIT is always treated as an edge triggered interrupt, even if
    /// programmed otherwise. For proper operation, this redirection table entry
    /// must be programmed to “edge” triggered interrupt.
    INIT = 5,
    Reserved6 = 6,

    /// Deliver the signal to the INTR signal of all processor cores listed in the
    /// destination as an interrupt that originated in an externally connected
    /// (8259A-compatible) interrupt controller. The INTA cycle that corresponds
    /// to this ExtINT delivery is routed to the external controller that is expected
    /// to supply the vector. A Delivery Mode of "ExtINT" requires an edge
    /// trigger mode.
    External = 7,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits)]
pub enum DestinationMode {
    Physical = 0,
    Logical = 1,
}

#[derive(Debug)]
struct Bitset {
    values: [AtomicU32; 8],
}

impl Bitset {
    pub fn new() -> Self {
        Self {
            values: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    /// Returns true if the bit is set.
    pub fn get(&self, index: impl Into<usize>) -> bool {
        let index = index.into();
        let bit = 1 << (index % 32);
        self.values[index / 32].load(Ordering::SeqCst) & bit != 0
    }

    /// Returns true if the value changed.
    pub fn set(&self, index: impl Into<usize>) -> bool {
        let index = index.into();
        let bit = 1 << (index % 32);
        self.values[index / 32].fetch_or(bit, Ordering::SeqCst) & bit == 0
    }

    /// Returns true if the value changed.
    pub fn reset(&self, index: impl Into<usize>) -> bool {
        let index = index.into();
        let bit = 1 << (index % 32);
        self.values[index / 32].fetch_and(!bit, Ordering::SeqCst) & bit != 0
    }

    pub fn highest_index_set(&self) -> Option<usize> {
        for (index, v) in self.values.iter().enumerate().rev() {
            let v = v.load(Ordering::SeqCst);
            if v != 0 {
                return Some(index * 32 + (31 - v.leading_zeros() as usize))
            }
        }

        None
    }

    pub fn snapshot(&self) -> BitsetSnapshot {
        BitsetSnapshot {
            values: std::array::from_fn(|n| self.values[n].load(Ordering::SeqCst)),
        }
    }

    pub fn restore(&self, snapshot: BitsetSnapshot) {
        for (dst, src) in self.values.iter().zip(snapshot.values) {
            dst.store(src, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct BitsetSnapshot {
    values: [u32; 8],
}

#[derive(Clone)]
pub struct DynamicApicIrqLine {
    io: Arc<IoApicCore>,
    local: Arc<LocalApicCore>,
}

impl Debug for DynamicApicIrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicApicIrqLine").finish()
    }
}

impl DynamicApicIrqLine {
    pub fn new(io: &IoApic, local: &LocalApic) -> Self {
        Self {
            io: io.core.clone(),
            local: local.core.clone(),
        }
    }

    pub fn pulse(&self, index: u32) {
        self.set(index, true);
        self.set(index, false);
    }

    pub fn set(&self, index: u32, val: bool) {
        let changed = if val {
            self.io.irq_lines.set(index as usize)
        } else {
            self.io.irq_lines.reset(index as usize)
        };

        let entry = self.io.redirection_entry(index);
        trace!("INTIN{:02X} = {val} (changed: {changed}, entry: {entry:X?})", index);
        let val = match entry.polarity() {
            io::Polarity::ActiveHigh => val,
            io::Polarity::ActiveLow => !val,
        };

        if entry.masked() {
            trace!("Ignoring interrupt INTIN{:02X}, because it is masked", index);
            if entry.trigger_mode() == TriggerMode::LevelSensitive && val {
                let mut e = entry;
                e.set_pending(true);
                self.io.set_redirection_entry(index, e);
            }

            return;
        }

        match entry.trigger_mode() {
            TriggerMode::EdgeSensitive => {
                if changed && val {
                    self.send_interrupt(index, entry);
                    let mut e = entry;
                    e.set_pending(false);
                    self.io.set_redirection_entry(index, e);
                }
            },
            TriggerMode::LevelSensitive => {
                if val && !entry.pending() {
                    self.send_interrupt(index, entry);
                    let mut e = entry;
                    e.set_pending(true);
                    self.io.set_redirection_entry(index, e);
                } else if !val {
                    let mut e = entry;
                    e.set_pending(false);
                    e.set_was_received(false);
                    self.io.set_redirection_entry(index, e);
                }
            },
        }
    }

    fn send_interrupt(&self, index: u32, entry: RedirectionEntry) {
        // TODO: Check APIC ID
        // TODO: Destination mode?
        trace!(
            "Delivering interrupt 0x{:02X} (INTIN{:02X}): {entry:X?}",
            entry.vector(),
            index
        );

        match entry.delivery_mode() {
            DeliveryMode::Normal | DeliveryMode::LowPriority => {
                if entry.trigger_mode() == TriggerMode::LevelSensitive {
                    let mut e = entry;
                    e.set_was_received(true);
                    self.io.set_redirection_entry(index, e);
                }

                self.local.raise_interrupt(entry.vector(), entry.trigger_mode());
            },
            DeliveryMode::SMI => todo!("SMI"),
            DeliveryMode::NMI => todo!("NMI"),
            DeliveryMode::INIT => todo!("INIT"),
            DeliveryMode::External => todo!("EXTERNAL"),
            DeliveryMode::Reserved3 | DeliveryMode::Reserved6 => panic!("Invalid delivery mode: {:?}", entry.delivery_mode()),
        }
    }
}

#[derive(Clone)]
pub struct ApicIrqLine {
    index: u32,
    inner: DynamicApicIrqLine,
}

impl Debug for ApicIrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApicIrqLine").field("index", &self.index).finish()
    }
}

impl ApicIrqLine {
    pub fn new(io: &IoApic, local: &LocalApic, index: u32) -> Self {
        Self {
            index,
            inner: DynamicApicIrqLine::new(io, local),
        }
    }

    pub fn pulse(&self) {
        self.set(true);
        self.set(false);
    }

    pub fn set(&self, val: bool) {
        self.inner.set(self.index, val)
    }
}

#[derive(Clone)]
pub struct DualIrqLine {
    pub pic: IrqLine,
    pub apic: ApicIrqLine,
}

impl std::fmt::Debug for DualIrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualIrqLine").finish()
    }
}

impl DualIrqLine {
    pub fn pulse(&self) {
        self.pic.pulse();
        self.apic.pulse();
    }

    // TODO
    pub fn set(&self, val: bool) {
        if val {
            self.pulse();
        }
    }
}

#[derive(Clone)]
pub struct DualDynamicIrqLine {
    pub pic: DynamicIrqLine,
    pub apic: DynamicApicIrqLine,
}

impl std::fmt::Debug for DualDynamicIrqLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualDynamicIrqLine").finish()
    }
}

impl DualDynamicIrqLine {
    pub fn pulse(&self, index: u8) {
        if index < 16 {
            self.pic.pulse(index);
        }

        self.apic.pulse(index as u32);
    }

    // TODO
    pub fn set(&self, index: u8, val: bool) {
        if val {
            self.pulse(index);
        }
    }
}
