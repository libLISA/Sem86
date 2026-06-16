use sem86_arch::mem::snapshot::MemorySnapshot;
use serde::{Deserialize, Serialize};

use crate::arch::intel386::State;
use crate::hw::snapshot::HwSnapshot;
use crate::system::ExpandedDb;
use crate::tracefile::TraceSnapshot;

#[derive(Clone, Serialize, Deserialize)]
pub struct EmulatorSnapshot {
    pub(super) cpu: State,
    pub(super) op_size: ExpandedDb,
    pub(super) next_expected_interrupt: Option<u8>,
    pub(super) is_halted: bool,
    pub(super) trace_triggered_interrupt: Option<u8>,
    pub(super) hw: HwSnapshot,
    pub(super) memory: MemorySnapshot,
    pub(super) trace: Option<TraceSnapshot>,
    pub(super) k: u64,
}
