use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arbitrary_int::u4;
use bilge::prelude::*;
use bitcode::{Decode, Encode};
use bytemuck::{Pod, Zeroable};
use log::{debug, error, info, trace};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

use crate::hw::MMIO_ID_IOAPIC;
use crate::hw::pic::{Bitset, BitsetSnapshot, DeliveryMode, DestinationMode};

const MMIO_ADDR: u64 = 0xFEC00000;
const NUM_REDIRECTION_ENTRIES: usize = 24;

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Polarity {
    ActiveHigh = 0,
    ActiveLow = 1,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TriggerMode {
    EdgeSensitive = 0,
    LevelSensitive = 1,
}

#[derive(Debug)]
pub struct IoApicCore {
    redirection_entries: [AtomicU64; NUM_REDIRECTION_ENTRIES],
    pub(super) irq_lines: Bitset,
}

impl Default for IoApicCore {
    fn default() -> Self {
        Self::new()
    }
}

impl IoApicCore {
    pub fn new() -> Self {
        Self {
            redirection_entries: std::array::from_fn(|_| {
                AtomicU64::new(
                    RedirectionEntry::new(
                        0,
                        DeliveryMode::Normal,
                        DestinationMode::Physical,
                        false,
                        Polarity::ActiveHigh,
                        false,
                        TriggerMode::LevelSensitive,
                        true,
                        0,
                    )
                    .value,
                )
            }),
            irq_lines: Bitset::new(),
        }
    }

    pub fn redirection_entry(&self, index: u32) -> RedirectionEntry {
        RedirectionEntry::from(self.redirection_entries[index as usize].load(Ordering::SeqCst))
    }

    pub fn set_redirection_entry(&self, index: u32, value: RedirectionEntry) {
        self.redirection_entries[index as usize].store(value.value, Ordering::SeqCst)
    }

    pub fn iter_redirection_entries(&self) -> impl Iterator<Item = RedirectionEntry> + use<'_> {
        self.redirection_entries
            .iter()
            .map(|v| RedirectionEntry::from(v.load(Ordering::SeqCst)))
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits, Zeroable, Pod)]
#[repr(transparent)]
pub struct RedirectionEntry {
    pub vector: u8,
    pub delivery_mode: DeliveryMode,
    pub destination_mode: DestinationMode,
    /// Set if this interrupt is going to be sent, but the APIC is busy. Read only.
    pub pending: bool,
    pub polarity: Polarity,
    /// Used for level triggered interrupts only to show if a local APIC has received the interrupt (= 1), or has sent an EOI (= 0). Read only.
    pub was_received: bool,
    pub trigger_mode: TriggerMode,
    /// When set, interrupt is masked.
    pub masked: bool,
    reserved: u39,
    /// Destination field. If the destination mode bit was clear, then the lower 4 bits contain the bit APIC ID to sent the interrupt to. If the bit was set, the upper 4 bits also contain a set of processors.
    pub destination: u8,
}

#[derive(Debug)]
pub struct IoApic {
    register_select: u32,
    id: u4,
    pub(super) core: Arc<IoApicCore>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct IoApicSnapshot {
    register_select: u32,
    id: u8,
    redirection_entries: [u64; NUM_REDIRECTION_ENTRIES],
    irq_lines: BitsetSnapshot,
}

impl IoApic {
    pub fn new(mem: &Mem32) -> Self {
        // TODO: This only needs to be 32 bytes
        mem.map_physical_memory_to_mmio(MMIO_ADDR..MMIO_ADDR + 0x1000, MMIO_ID_IOAPIC);

        Self {
            register_select: 0,
            id: u4::new(0),
            core: Arc::new(IoApicCore::new()),
        }
    }

    pub fn write(&mut self, index: u32, val: u32) {
        match index {
            0 => self.register_select = val,
            1 => {
                if self.register_select == 0 {
                    self.id = u4::new((val >> 24) as u8 & 0xf);
                    info!("ID set to 0x{:X}", self.id);
                } else if let Some(entry_index) = self.entry_index() {
                    let entry = self.core.redirection_entry(entry_index);
                    let new_val = if self.register_select & 1 != 0 {
                        (val as u64) << 32 | (entry.value & 0xffff_ffff)
                    } else {
                        (val as u64) | (entry.value & !0xffff_ffff)
                    };

                    trace!("Original entry: 0x{:04X}", entry.value);
                    trace!("Updated entry : 0x{new_val:04X}");

                    let entry = RedirectionEntry::from(new_val);
                    self.core.set_redirection_entry(entry_index, entry);

                    info!("Redirection entry {entry_index} modified: {entry:X?}");
                } else {
                    error!("Invalid IOAPIC write to register 0x{:X} = 0x{val:X}", self.register_select)
                }
            },
            _ => error!("Invalid IOAPIC write to index 0x{index:X}"),
        }
    }

    fn entry_index(&self) -> Option<u32> {
        if self.register_select >= 0x10 && self.register_select < 0x10 + NUM_REDIRECTION_ENTRIES as u32 * 2 {
            Some((self.register_select - 0x10) / 2)
        } else {
            None
        }
    }

    pub fn read(&self, index: u32) -> u32 {
        match index {
            0 => self.register_select,
            1 => match self.register_select {
                0 => self.id.as_u32() << 24,
                1 => (((NUM_REDIRECTION_ENTRIES - 1) << 16) | 0x11) as u32,
                // Arbitration priority: unsupported
                2 => 0,
                n => {
                    if let Some(entry_index) = self.entry_index() {
                        let v = self.core.redirection_entry(entry_index);
                        debug!("Read from register 0x{:X} = {v:X?}", self.register_select);
                        if n & 1 != 0 { (v.value >> 32) as u32 } else { v.value as u32 }
                    } else {
                        error!("Invalid IOAPIC write to register 0x{:X}", n);
                        0
                    }
                },
            },
            _ => {
                error!("Invalid IOAPIC read from register 0x{index:X}");
                u32::MAX
            },
        }
    }

    pub fn clear_pending(&self) {
        for n in 0..NUM_REDIRECTION_ENTRIES as u32 {
            let mut entry = self.core.redirection_entry(n);
            entry.set_pending(false);
            self.core.set_redirection_entry(n, entry);
        }
    }

    pub fn find_redirection_entry_from_vector(&self, vector: u8) -> Option<(usize, RedirectionEntry)> {
        self.core
            .iter_redirection_entries()
            .enumerate()
            .find(|(_, e)| e.vector() == vector)
    }

    pub fn redirection_entry_vector(&self, index: usize) -> u8 {
        self.core.redirection_entry(index as u32).vector()
    }

    pub fn snapshot(&self) -> IoApicSnapshot {
        IoApicSnapshot {
            register_select: self.register_select,
            id: self.id.as_u8(),
            redirection_entries: std::array::from_fn(|n| self.core.redirection_entry(n as u32).value),
            irq_lines: self.core.irq_lines.snapshot(),
        }
    }

    pub fn restore(&mut self, ioapic: IoApicSnapshot) {
        self.register_select = ioapic.register_select;
        self.id = u4::new(ioapic.id);
        for (n, e) in ioapic.redirection_entries.into_iter().enumerate() {
            self.core.set_redirection_entry(n as u32, RedirectionEntry::from(e));
        }

        self.core.irq_lines.restore(ioapic.irq_lines);
    }
}
