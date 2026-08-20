use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use log::{debug, info, trace};

use crate::DisplayK;
use crate::decoder::EncodingLookup;
use crate::emulator::EmulatorContextInner;
use crate::emulator::stat::Stat;
use crate::il::MakeEncoding;
use crate::util::DisplayByteSize;

struct State {
    running: AtomicBool,
    should_print: AtomicBool,
}

pub struct PerformanceMonitor {
    last_k: u64,
    last_k_time: Instant,
    last_jit_k: u64,
    mips_history: VecDeque<f64>,
    stat_jits: Stat<usize>,
    stat_page_mmaps: Stat<u64>,
    stat_page_walks: Stat<u64>,
    stat_page_faults: Stat<u64>,
    stat_page_dirty_frame_checks: Stat<usize>,
    stat_page_altered_mappings: Stat<u64>,
    stat_phys_frames_marked_dirty: Stat<u64>,
    stat_num_unaligned_reads: Stat<u64>,
    stat_num_trapped_reads: Stat<u64>,
    stat_num_page_bounds_crossing_reads: Stat<u64>,
    stat_slow_writes: Stat<u64>,
    stat_link_cache_misses: Stat<u64>,
    stat_page_cr3_changes: Stat<u64>,
    stat_page_cr3_reloads: Stat<u64>,
    stat_chains_compiled: Stat<u64>,
    stat_chains_executed: Stat<usize>,
    stat_num_entries: Stat<u64>,
    stat_num_interrupts_entered: Stat<u64>,
    stat_num_port_outs: Stat<u64>,
    stat_num_port_ins: Stat<u64>,
    stat_page_metadata_clears: Stat<u64>,
    stat_num_descriptors_read: Stat<u64>,
    stat_num_cache_consistency_checks: Stat<u64>,
    state: Arc<State>,
}

/// Channel over which `Arc<State>` can be sent.
/// Once sent, `should_print` will be set to true every 5 seconds,
/// until `running` is set to false.
/// 
/// This single background thread is responsible for ticking all
/// running timers in the process.
/// This avoids the overhead of thread spawning in `PerformanceMonitor::new`.
static TIMER_CHANNEL: LazyLock<Arc<Sender<Arc<State>>>> = LazyLock::new(|| {
    let (send, recv) = channel();
    std::thread::Builder::new()
        .name("perf-timer".into())
        .spawn(move || {
            let mut states = Vec::new();
            loop {
                while let Ok(state) = recv.try_recv() {
                    states.push(state);
                }

                states.retain(|state: &Arc<State>| {
                    state.should_print.fetch_or(true, Ordering::Relaxed);
                    state.running.load(Ordering::Relaxed)
                });

                std::thread::sleep(Duration::from_secs(5));
            }
        })
        .unwrap();

    Arc::new(send)
});

impl PerformanceMonitor {
    pub fn new(k: u64) -> Self {
        let state = Arc::new(State {
            running: AtomicBool::new(true),
            should_print: AtomicBool::new(false),
        });

        TIMER_CHANNEL.send(state.clone());

        Self {
            last_k: k,
            last_k_time: Instant::now(),
            last_jit_k: 0,
            stat_jits: Stat::new("SEEJITs"),
            stat_page_mmaps: Stat::new("mmaps"),
            stat_page_walks: Stat::new("walks"),
            stat_page_faults: Stat::new("faults"),
            stat_page_dirty_frame_checks: Stat::new("imem reverified"),
            stat_page_altered_mappings: Stat::new("altered mappings"),
            stat_phys_frames_marked_dirty: Stat::new("marked dirty"),
            stat_num_unaligned_reads: Stat::new("unaligned reads"),
            stat_num_trapped_reads: Stat::new("trapped reads"),
            stat_num_page_bounds_crossing_reads: Stat::new("page-bounds-crossing reads"),
            stat_slow_writes: Stat::new("slow writes"),
            stat_link_cache_misses: Stat::new("link misses"),
            stat_page_cr3_changes: Stat::new("CR3 changes"),
            stat_page_cr3_reloads: Stat::new("CR3 reloads"),
            stat_chains_compiled: Stat::new("CC"),
            stat_chains_executed: Stat::new("CX"),
            stat_num_entries: Stat::new("EE"),
            stat_num_interrupts_entered: Stat::new("interrupts"),
            stat_num_port_outs: Stat::new("OUTs"),
            stat_num_port_ins: Stat::new("INs"),
            stat_page_metadata_clears: Stat::new("metadata clears"),
            stat_num_descriptors_read: Stat::new("descriptors read"),
            stat_num_cache_consistency_checks: Stat::new("consistency checks"),
            mips_history: VecDeque::new(),
            state,
        }
    }

    #[inline(never)]
    pub fn update(&mut self, halt_time: Duration, ctx: &mut EmulatorContextInner<'_, '_>) {
        let time = self.last_k_time.elapsed();
        self.last_k_time = Instant::now();

        let emulator = &ctx.emulator;
        let active_rate = 1. - halt_time.as_secs_f64() / time.as_secs_f64();
        let active_time = active_rate * 100.;
        let num = emulator.ctx.k - self.last_k;
        let num_jit = emulator.ctx.jit_k - self.last_jit_k;
        // We compute MIPS during the time the CPU was active by dividing by the active rate. So if the CPU was active 50% of the time, we double the MIPS we executed in that 50%.
        let mips = (num as f64 / time.as_secs_f64()) / 1_000_000f64 / active_rate;
        let percentage_jitted = (num_jit as f64 / num as f64) * 100.;

        self.last_jit_k = emulator.ctx.jit_k;
        self.last_k = emulator.ctx.k;

        self.mips_history.push_back(mips);
        if self.mips_history.len() > 15 {
            self.mips_history.pop_front();
        }

        let avg_mips = self.avg_mips();

        let stat_page_mmaps = self.stat_page_mmaps.update(time, emulator.ctx.memory.mmap_count());
        let stat_page_faults = self.stat_page_faults.update(time, emulator.ctx.memory.page_fault_count());
        let stat_phys_frames_marked_dirty = self
            .stat_phys_frames_marked_dirty
            .update(time, emulator.ctx.memory.phys_frames_marked_dirty());
        let stat_link_cache_misses = self
            .stat_link_cache_misses
            .update(time, emulator.ctx.mmio_ctx.icache.num_link_cache_misses());
        let stat_cache_consistency_checks = self
            .stat_num_cache_consistency_checks
            .update(time, emulator.ctx.mmio_ctx.icache.num_cache_consistency_checks());
        let stat_page_altered_mappings = self
            .stat_page_altered_mappings
            .update(time, emulator.ctx.memory.num_altered_mappings());
        let stat_page_walks = self.stat_page_walks.update(time, emulator.ctx.memory.num_page_walks());
        let stat_page_cr3_changes = self.stat_page_cr3_changes.update(time, emulator.num_cr3_changes);
        let stat_page_cr3_reloads = self.stat_page_cr3_reloads.update(time, emulator.num_cr3_reloads);
        let stat_jits = self.stat_jits.update(time, emulator.ctx.mmio_ctx.icache.num_see_jits());
        let stat_page_dirty_frame_checks = self
            .stat_page_dirty_frame_checks
            .update(time, emulator.ctx.mmio_ctx.icache.num_dirty_frame_checks());
        let stat_page_metadata_clears = self
            .stat_page_metadata_clears
            .update(time, emulator.ctx.memory.num_metadata_clears());
        let stat_num_descriptors_read = self.stat_num_descriptors_read.update(time, emulator.ctx.num_descriptors_read);
        let stat_num_unaligned_reads = self
            .stat_num_unaligned_reads
            .update(time, emulator.ctx.memory.num_unaligned_reads());
        let stat_num_trapped_reads = self
            .stat_num_trapped_reads
            .update(time, emulator.ctx.memory.num_trapped_reads());
        let stat_num_page_bounds_crossing_reads = self
            .stat_num_page_bounds_crossing_reads
            .update(time, emulator.ctx.memory.num_page_bounds_crossing_reads());
        let stat_slow_writes = self.stat_slow_writes.update(time, emulator.ctx.memory.num_slow_writes());

        let stat_chains_compiled = self
            .stat_chains_compiled
            .update(time, emulator.ctx.mmio_ctx.icache.num_pages_jitted());
        let stat_chains_executed = self.stat_chains_executed.update(time, ctx.num_chains_executed);
        let stat_num_entries = self.stat_num_entries.update(time, ctx.emulator_entry_count);
        let stat_num_interrupts_entered = self.stat_num_interrupts_entered.update(time, emulator.num_interrupts_entered);

        let stat_num_port_outs = self.stat_num_port_outs.update(time, emulator.ctx.num_port_outs);
        let stat_num_port_ins = self.stat_num_port_ins.update(time, emulator.ctx.num_port_ins);

        let k = DisplayK(emulator.ctx.k);
        info!("=== Performance report k={k} ===");
        // TODO: Track number of icache entries with certain vs speculative jumps.
        info!("{mips:.04} ~ {avg_mips:.04} MIPS ({percentage_jitted:.1}% JITed) | {stat_jits}");
        info!(
            "Memory usage: SEEJIT: {:.1} ({} encodings) | ICache: {} | Chains: {}",
            DisplayByteSize(emulator.ctx.mmio_ctx.icache.seejit_memory_usage()),
            emulator.ctx.mmio_ctx.icache.num_see_jits(),
            DisplayByteSize(emulator.ctx.mmio_ctx.icache.entry_memory_usage()),
            DisplayByteSize(emulator.ctx.mmio_ctx.icache.page_jit_memory_usage())
        );
        info!("Cache: {stat_link_cache_misses} | {stat_cache_consistency_checks}");
        info!(
            "Memory: {stat_page_mmaps} ({stat_page_altered_mappings}) | {stat_phys_frames_marked_dirty} | {} code frames dirty ({} total) / {} frames clean | {stat_page_dirty_frame_checks} | {stat_num_unaligned_reads} | {stat_num_trapped_reads} | {stat_num_page_bounds_crossing_reads} | {stat_slow_writes}",
            emulator.ctx.mmio_ctx.icache.code_frames_dirty(),
            emulator.ctx.mmio_ctx.icache.total_frames_dirty(),
            emulator.ctx.mmio_ctx.icache.total_frames_clean()
        );
        info!(
            "Paging: {stat_page_walks} | {stat_page_faults} | {stat_page_cr3_changes} | {stat_page_cr3_reloads} | {stat_page_metadata_clears}"
        );
        info!(
            "Chains: {stat_chains_compiled} | {stat_chains_executed} | {:.1} instrs/CX",
            num_jit as f64 / stat_chains_executed.delta()
        );
        info!("Port IO: {stat_num_port_outs} | {stat_num_port_ins}");
        info!("{stat_num_descriptors_read}");

        if halt_time.as_millis() > 0 {
            info!(
                "Active {active_time:.1}% (halted for {}ms) | {stat_num_entries} | {stat_num_interrupts_entered}",
                halt_time.as_millis()
            );
        } else {
            info!("Active {active_time:.1}% | {stat_num_entries} | {stat_num_interrupts_entered}");
        }

        if emulator.profiling_enabled {
            info!("Profiler snapshot:\n{:?}", ctx.profiler.snapshot());
        }

        if emulator.measure_single_encoding_execution {
            for (pos, (index, num)) in ctx.most_executed_encodings().take(15).enumerate() {
                let encoding = ctx.semantics.get(index).make_encoding();
                debug!(target: extend_path_with!("sejit"), "Most executed encoding #{pos}: encoding #{index} ({num}x ~ {:.2}%) - {:X} ({:?})", (num as f64 / emulator.ctx.k as f64) * 100., encoding.instr(), encoding.semantics.name);
                trace!(target: extend_path_with!("sejit"), "{encoding}");
            }

            let total_cycles = emulator.execution_duration.iter().sum::<u64>();
            debug!(target: extend_path_with!("sejit"), "Total execution cycles: {:.1}B", total_cycles as f64 / 1_000_000_000.);
            for (pos, (index, num)) in ctx.encodings_most_time_taken().take(15).enumerate() {
                let encoding = ctx.semantics.get(index).make_encoding();
                debug!(target: extend_path_with!("sejit"), "Most time taken encoding #{pos}: encoding #{index} ({:.1}B cycles - {:.1}cycles/exec) - {:X} ({:?})", num as f64 / 1_000_000_000., num as f64 / emulator.execution_counts[index] as f64, encoding.instr(), encoding.semantics.name);
                trace!(target: extend_path_with!("sejit"), "{encoding}");
            }

            for (index, (count, info)) in emulator.ctx.mmio_ctx.icache.most_executed_blocks().take(10).enumerate() {
                debug!(target: extend_path_with!("perf::blockjit"), "Most executed block #{index}: {count}x - segment_sizes={:?} entry-point={:X} protected-mode-memory-accesses={} {}", info.segment_sizes, info.entry_point, info.protected_mode_accesses, serde_json::to_string(&info.instrs).unwrap());
            }
        }

        debug!(target: extend_path_with!("debug::icache"), "Number of instructions crossing page bounds: {}", DisplayNumEntriesCrossingPageBounds(ctx));
        trace!(target: extend_path_with!("debug::icache"), "Instruction cache snapshot:\n{}", emulator.ctx.mmio_ctx.icache.display_debug_snapshot(&emulator.ctx.memory.physical_memory()));

        info!("Generating this report took {}ms", self.last_k_time.elapsed().as_millis());
    }

    pub fn avg_mips(&self) -> f64 {
        self.mips_history.iter().sum::<f64>() / self.mips_history.len() as f64
    }

    #[inline(always)]
    pub fn should_print(&self) -> bool {
        self.state.should_print.fetch_and(false, Ordering::Relaxed)
    }
}

impl Drop for PerformanceMonitor {
    fn drop(&mut self) {
        self.state.running.store(false, Ordering::Relaxed);
    }
}

/// Struct that only invokes `num_instrs_crossing_page_bounds` if `Display`ed.
/// This ensures we do not needlessly run the function when logging is disabled.
struct DisplayNumEntriesCrossingPageBounds<'r, 'mem, 'tag>(&'r EmulatorContextInner<'mem, 'tag>);

impl Display for DisplayNumEntriesCrossingPageBounds<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.emulator.ctx.mmio_ctx.icache.num_instrs_crossing_page_bounds().fmt(f)
    }
}
