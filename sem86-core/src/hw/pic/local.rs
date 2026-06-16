use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::{debug, error, info, trace, warn};
use sem86_arch::mem::Mem32;
use serde::{Deserialize, Serialize};

use crate::hw::MMIO_ID_LAPIC;
use crate::hw::intr::{IntrHandle, PendingRequest};
use crate::hw::pic::io::TriggerMode;
use crate::hw::pic::{Bitset, BitsetSnapshot, DeliveryMode, DestinationMode};
use crate::time::EmulatorClock;

#[bitsize(64)]
#[derive(Copy, Clone, FromBits, DebugBits, Serialize, Deserialize, Encode, Decode)]
pub struct ApicBase {
    reserved: u8,
    bsp: bool,
    reserved: u2,
    enabled_globally: bool,
    apic_base_addr: u52,
}

#[bitsize(32)]
#[derive(Copy, Clone, FromBits, DebugBits)]
pub struct Icr {
    vector: u8,
    delivery_mode: DeliveryMode,
    destination_mode: DestinationMode,
    delivery_status: bool,
    reserved: u1,

    /// Clear for init-level deassert
    init_level_assert: bool,

    /// Set for init-level deassert
    /// TODO: Figure out if this is correct
    trigger_mode: TriggerMode,
    reserved: u2,
    destination_type: DestinationType,
    reserved: u12,
}

#[bitsize(2)]
#[derive(Copy, Clone, FromBits, Debug)]
pub enum DestinationType {
    UseDestinationField,
    CurrentCpu,
    AllProcessors,
    AllProcessorsExceptCurrent,
}

const DEFAULT_MMIO_ADDR: u64 = 0xFEE00000;

#[derive(Debug)]
pub struct LocalApicCore {
    task_priority: AtomicU8,
    in_service: Bitset,
    trigger_mode: Bitset,
    interrupt_requests: Bitset,
    intr: IntrHandle,
    pending_intr: Mutex<Option<PendingRequest>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
struct LocalApicCoreSnapshot {
    task_priority: u8,
    in_service: BitsetSnapshot,
    trigger_mode: BitsetSnapshot,
    interrupt_requests: BitsetSnapshot,
    has_pending: bool,
}

impl LocalApicCore {
    pub fn new(intr: IntrHandle) -> Self {
        Self {
            in_service: Bitset::new(),
            trigger_mode: Bitset::new(),
            interrupt_requests: Bitset::new(),
            intr,
            task_priority: AtomicU8::new(0),
            pending_intr: Mutex::new(None),
        }
    }

    fn priority(vector: u8) -> u8 {
        vector >> 4
    }

    pub fn raise_interrupt(&self, vector: u8, trigger_mode: TriggerMode) {
        // TODO: How should we handle level-mode triggers that reassert while still in service? Should we allow interrupt requests to become true again?
        if !self.interrupt_requests.set(vector) || self.in_service.get(vector) {
            // Interrupts will not be checked when running in trace mode, so seeing this log message can be normal.
            // When following a trace, interrupts are triggered directly from the trace instead.
            trace!(
                "Interrupt 0x{vector:02X} is already pending or in service (={})",
                self.in_service.get(vector)
            );
            return;
        }

        match trigger_mode {
            TriggerMode::EdgeSensitive => self.trigger_mode.reset(vector),
            TriggerMode::LevelSensitive => self.trigger_mode.set(vector),
        };

        trace!(
            "Priority: {}, current priority: {}",
            Self::priority(vector),
            self.current_priority()
        );
        if Self::priority(vector) > self.current_priority() {
            trace!("INTR raised");
            *self.pending_intr.lock().unwrap() = Some(self.intr.request());
        }
    }

    pub fn current_priority(&self) -> u8 {
        Self::priority(
            self.in_service
                .highest_index_set()
                .map(|index| index as u8)
                .unwrap_or_else(|| self.task_priority.load(Ordering::SeqCst)),
        )
    }

    /// Returns the interrupt vector, if an interrupt can be triggered.
    /// If some vector is returned, the request is cleared and the interrupt
    /// is marked as in-service.
    pub fn next_pending_interrupt(&self) -> Option<u8> {
        if let Some(vector) = self.interrupt_requests.highest_index_set() {
            let vector = vector as u8;
            if Self::priority(vector) > self.current_priority() {
                trace!("Servicing interrupt 0x{vector:02X}");
                self.interrupt_requests.reset(vector);
                self.in_service.set(vector);

                // TODO: There are all sorts of concurrency issues if we ever access this from another thread. (e.g., what if interrupt_requests was updated while we set in_service? -- in that case our mutex should not be reset, as we might want to immediately interrupt with a higher priority interrupt.)
                *self.pending_intr.lock().unwrap() = None;

                return Some(vector)
            }
        }

        *self.pending_intr.lock().unwrap() = None;

        None
    }

    pub fn write_eoi(&self) {
        if let Some(vector) = self.in_service.highest_index_set() {
            trace!("Writing EOI for vector 0x{vector:02X}");
            self.in_service.reset(vector);

            self.check_pending();

            if self.trigger_mode.get(vector) {
                // TODO: Notify IoApic EOI
            }
        } else {
            trace!("Unable to write EOI, no interrupts in service: {:04X?}", self.in_service);
        }
    }

    fn check_pending(&self) {
        trace!("Checking if any interrupts are pending...");
        if let Some(pending) = self.interrupt_requests.highest_index_set() {
            trace!(
                "Next pending interrupt: {pending:02X} with priority {}, current priority: {}",
                Self::priority(pending as u8),
                self.current_priority()
            );
            *self.pending_intr.lock().unwrap() = if Self::priority(pending as u8) > self.current_priority() {
                trace!("INTR raised");
                Some(self.intr.request())
            } else {
                trace!(
                    "Not raising INTR, because current task has higher priority ({:02X?} / 0x{:02X})",
                    self.in_service.highest_index_set(),
                    self.task_priority.load(Ordering::SeqCst)
                );
                None
            };
        }
    }

    fn snapshot(&self) -> LocalApicCoreSnapshot {
        LocalApicCoreSnapshot {
            task_priority: self.task_priority.load(Ordering::SeqCst),
            in_service: self.in_service.snapshot(),
            trigger_mode: self.trigger_mode.snapshot(),
            interrupt_requests: self.interrupt_requests.snapshot(),
            has_pending: self.pending_intr.lock().unwrap().is_some(),
        }
    }

    fn restore(&self, snapshot: LocalApicCoreSnapshot) {
        self.task_priority.store(snapshot.task_priority, Ordering::SeqCst);
        self.in_service.restore(snapshot.in_service);
        self.trigger_mode.restore(snapshot.trigger_mode);
        self.interrupt_requests.restore(snapshot.interrupt_requests);

        if snapshot.has_pending {
            *self.pending_intr.lock().unwrap() = Some(self.intr.request());
        }
    }
}

#[allow(unused)]
#[derive(Debug)]
pub struct LocalApic {
    id: u32,
    logical_apic_destination_id: u8,
    destination_format: u4,
    spurious_interrupt_vector: u32,
    error_status: u32,
    cmci: u32,
    interrupt_command_register: [u32; 2],
    lvt: [u32; 6],
    initial_count: u32,
    current_count: u32,
    divide_configuration: u32,
    pub(super) core: Arc<LocalApicCore>,
    apic_base: ApicBase,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct LocalApicSnapshot {
    id: u32,
    logical_apic_destination_id: u8,
    destination_format: u8,
    spurious_interrupt_vector: u32,
    error_status: u32,
    cmci: u32,
    interrupt_command_register: [u32; 2],
    lvt: [u32; 6],
    initial_count: u32,
    current_count: u32,
    divide_configuration: u32,
    core: LocalApicCoreSnapshot,
    apic_base: ApicBase,
}

impl LocalApic {
    pub fn new(mem: &Mem32, intr: IntrHandle) -> Self {
        mem.map_physical_memory_to_mmio(DEFAULT_MMIO_ADDR..DEFAULT_MMIO_ADDR + 0x1000, MMIO_ID_LAPIC);

        Self {
            id: 0,
            logical_apic_destination_id: 0,
            destination_format: u4::new(0),
            spurious_interrupt_vector: 0xff,
            error_status: 0,
            cmci: 0,
            interrupt_command_register: [0; 2],
            lvt: [0; 6],
            initial_count: 0,
            current_count: 0,
            divide_configuration: 0,
            core: Arc::new(LocalApicCore::new(intr)),
            apic_base: ApicBase::new(true, true, u52::new(DEFAULT_MMIO_ADDR >> 12)),
        }
    }

    pub fn write(&mut self, index: u32, val: u32) {
        match index {
            0x02 => self.id = val,
            0x08 => {
                self.core.task_priority.store(val as u8, Ordering::SeqCst);
                trace!("Task priority now set to: 0x{val:02X}");
                self.core.check_pending();
            },
            0x0B => {
                if val == 0 {
                    self.core.write_eoi();
                }
            },
            0x0D => self.logical_apic_destination_id = (val >> 24) as u8,
            0x0E => self.destination_format = u4::new((val >> 28) as u8),
            0x0F => self.spurious_interrupt_vector = val,
            0x30 => {
                self.interrupt_command_register[0] = val;
                let id = (self.interrupt_command_register[1] >> 24) & 0xf;
                let icr = Icr::from(self.interrupt_command_register[0]);

                // 0 | 2 | 3 => error!("TODO: Send inter-processor interrupt: {icr:X?} with id=0x{id:X}"),
                // 1 => {
                //     // TODO: When implementing multiple CPUs, we need to handle this.
                //     trace!("Ignoring ICR for other CPUs");
                // }
                // _ => unreachable!(),
                debug!("Sending interrupt: {icr:X?}");
                match icr.destination_type() {
                    DestinationType::AllProcessorsExceptCurrent => {
                        warn!("Ignoring ICR for other CPUs (id=0x{id:X}, icr={icr:X?})")
                    },
                    DestinationType::UseDestinationField => {
                        warn!(
                            "parse destination field: {icr:?} icr[1]={:?}, lapic id: {:?}",
                            self.interrupt_command_register[1], self.id
                        );

                        let destination = self.interrupt_command_register[1] >> 24;
                        match icr.destination_mode() {
                            DestinationMode::Physical => {
                                assert_eq!(
                                    self.id,
                                    self.interrupt_command_register[1] >> 24,
                                    "Sending IRQs to CPUs other than the current CPU is not implemented -- {icr:?}"
                                );
                                self.core.raise_interrupt(icr.vector(), icr.trigger_mode());
                            },
                            DestinationMode::Logical => {
                                assert!(
                                    self.logical_apic_destination_id & destination as u8 != 0,
                                    "Sending IRQs to CPUs other than the current CPU is not implemented -- {icr:?}, destination: 0x{destination:X}, id: 0x{:X}, logical destination id: 0x{:X}",
                                    self.id,
                                    self.logical_apic_destination_id
                                );
                                self.core.raise_interrupt(icr.vector(), icr.trigger_mode());
                            },
                        }
                    },
                    DestinationType::AllProcessors | DestinationType::CurrentCpu => match icr.delivery_mode() {
                        DeliveryMode::Normal => {
                            self.core.raise_interrupt(icr.vector(), icr.trigger_mode());
                        },
                        DeliveryMode::LowPriority => todo!(),
                        DeliveryMode::SMI => todo!(),
                        DeliveryMode::Reserved3 => todo!(),
                        DeliveryMode::NMI => todo!(),
                        DeliveryMode::INIT => error!("Ignoring INIT IPI {icr:X?}"),
                        DeliveryMode::Reserved6 => todo!(),
                        DeliveryMode::External => todo!(),
                    },
                }
            },
            0x31 => self.interrupt_command_register[1] = val,
            0x32..=0x37 => self.lvt[index as usize - 0x32] = val,
            0x38 => self.initial_count = val,
            0x39 => error!("Write to LAPIC current count register"),
            0x3E => self.divide_configuration = val,
            _ => error!("TODO: Write LAPIC[0x{index:X} / 0x{:X}] = 0x{val:X}", index << 4),
        }
    }

    pub fn read(&self, index: u32, time: &EmulatorClock) -> u32 {
        match index {
            // Version
            0x02 => self.id,
            0x03 => 0x00030010,
            0x08 => self.core.task_priority.load(Ordering::SeqCst) as u32,
            // We're not supporting arbitration
            0x09 => 0,
            // TODO: Processor priority register
            0x0A => 0,
            // EOI is write-only
            0x0B => 0,
            0x0D => (self.logical_apic_destination_id as u32) << 24,
            0x0E => (self.destination_format.as_u32() << 28) | 0x0fff_ffff,
            0x0F => self.spurious_interrupt_vector,
            0x30 => self.interrupt_command_register[0],
            0x31 => self.interrupt_command_register[1],
            0x32..=0x37 => self.lvt[index as usize - 0x32],
            0x38 => self.initial_count,
            0x39 => {
                let ticks = time.get_ticks_in_hz(33_000_000);
                let divider = (self.divide_configuration & 0b11) | ((self.divide_configuration & 0b1000) >> 1);
                let divider = 1 << ((divider + 1) & 0b111);
                let ticks = ticks / divider;

                let current_count = self.initial_count as u64 - 1 - (ticks % self.initial_count as u64);

                warn!("Read LAPIC current timer count register = 0x{:X}", current_count);
                current_count as u32
            },
            0x3E => self.divide_configuration,
            _ => {
                error!("TODO: Read LAPIC[0x{index:X} / 0x{:X}]", index << 4);
                u32::MAX
            },
        }
    }

    pub fn next_pending_interrupt(&self) -> Option<u8> {
        if self.apic_base.enabled_globally() {
            self.core.next_pending_interrupt()
        } else {
            None
        }
    }

    pub fn clear_pending(&self) {
        for n in 0..256usize {
            self.core.interrupt_requests.reset(n);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.apic_base.enabled_globally()
    }

    pub fn write_apic_base(&mut self, new_value: u64, mem: &Mem32) {
        let old = self.apic_base;
        self.apic_base = ApicBase::from(new_value);

        // BSP cannot be modified
        self.apic_base.set_bsp(old.bsp());
        info!("Wrote MSR_APIC_BASE = {:X?}", self.apic_base);

        if self.apic_base.enabled_globally() && self.apic_base.apic_base_addr().as_u64() != DEFAULT_MMIO_ADDR {
            panic!("TODO: Move APIC")
        }

        if !self.apic_base.enabled_globally() {
            let start = old.apic_base_addr().as_u64() << 12;
            mem.map_physical_memory_to_default(start..start + 0x1000);
        }
    }

    pub fn read_apic_base(&self) -> u64 {
        self.apic_base.value
    }

    pub fn snapshot(&self) -> LocalApicSnapshot {
        LocalApicSnapshot {
            id: self.id,
            logical_apic_destination_id: self.logical_apic_destination_id,
            destination_format: self.destination_format.as_u8(),
            spurious_interrupt_vector: self.spurious_interrupt_vector,
            error_status: self.error_status,
            cmci: self.cmci,
            interrupt_command_register: self.interrupt_command_register,
            lvt: self.lvt,
            initial_count: self.initial_count,
            current_count: self.current_count,
            divide_configuration: self.divide_configuration,
            core: self.core.snapshot(),
            apic_base: self.apic_base,
        }
    }

    pub fn restore(&mut self, lapic: LocalApicSnapshot) {
        self.id = lapic.id;
        self.logical_apic_destination_id = lapic.logical_apic_destination_id;
        self.destination_format = u4::new(lapic.destination_format);
        self.spurious_interrupt_vector = lapic.spurious_interrupt_vector;
        self.error_status = lapic.error_status;
        self.cmci = lapic.cmci;
        self.interrupt_command_register = lapic.interrupt_command_register;
        self.lvt = lapic.lvt;
        self.initial_count = lapic.initial_count;
        self.current_count = lapic.current_count;
        self.divide_configuration = lapic.divide_configuration;
        self.core.restore(lapic.core);
        self.apic_base = lapic.apic_base;

        // TODO: Map APIC_BASE
    }
}
