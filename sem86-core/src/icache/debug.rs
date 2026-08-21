use std::fmt::Display;
use std::u32;

use itertools::Itertools;
use liblisa::Instruction;
use sem86_arch::addr::{LinAddr, PhysAddr, PhysFrameIndex};
use serde::{Deserialize, Serialize};

use super::entry::EntryFlags;
use super::inner::CheckingFlags;
use crate::icache::entry::{CacheEntryId, EntryPoint};
use crate::il::part_values::PartValues;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntryIndex(usize);

impl From<CacheEntryId<'_>> for EntryIndex {
    fn from(value: CacheEntryId<'_>) -> Self {
        Self(value.as_u32() as usize)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinksSnapshot {
    Conditional([Option<EntryIndex>; 2]),
    Speculative([LinAddr; 2]),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ExecutionKind {
    Single,
    JittedPage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntrySnapshot {
    pub phys_addr: PhysAddr,
    pub encoding_index: u32,
    pub part_values: PartValues,
    pub instr_len: u8,
    pub links: LinksSnapshot,
    pub execution_kind: ExecutionKind,
    pub flags: EntryFlags,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameSnapshot {
    pub is_dirty: bool,
    pub(crate) page_jit_pending: bool,
    pub(crate) any_checking_flags: bool,
    pub(crate) checks_needed: CheckingFlags,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheSnapshot {
    pub phys_memory: Vec<u8>,
    pub phys_frames: Vec<FrameSnapshot>,
    pub entries: Vec<EntrySnapshot>,
}

impl Display for CacheSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut current_frame = PhysFrameIndex::from(PhysAddr::new(u32::MAX));
        let mut num_with_pending_checks = 0usize;
        let mut num_code_frames = 0usize;
        for entry in self.entries.iter().sorted_by_key(|e| e.phys_addr) {
            if current_frame != entry.phys_addr.into() {
                current_frame = entry.phys_addr.into();

                let frame = &self.phys_frames[current_frame.index()];
                writeln!(
                    f,
                    "FRAME {current_frame} | dirty={}, page JIT pending={}, checks needed={}: {:?}",
                    frame.is_dirty, frame.page_jit_pending,
                    frame.any_checking_flags, frame.checks_needed
                )?;

                num_code_frames += 1;
                if frame.any_checking_flags {
                    num_with_pending_checks += 1;
                }
            }

            let addr = &entry.phys_addr;
            let instr = Instruction::new(&self.phys_memory[addr.as_u32() as usize..][..entry.instr_len as usize]);

            let padding = "            ";
            let entry_point = match entry.flags.entry_kind() {
                EntryPoint::None => " ",
                EntryPoint::Local => "e",
                EntryPoint::External => "E",
                EntryPoint::Global => "S",
            };

            let jit_kind = match entry.execution_kind {
                ExecutionKind::Single => " ",
                ExecutionKind::JittedPage => "P",
            };

            let next = match &entry.links {
                LinksSnapshot::Conditional(links) => {
                    if let [Some(link), None] = links
                        && self.entries[link.0].phys_addr == *addr + entry.instr_len as u32
                    {
                        "seq".to_string()
                    } else {
                        format!(
                            "[ {} ]",
                            links
                                .iter()
                                .map(|link| match link {
                                    Some(link) => {
                                        let addr = self.entries[link.0].phys_addr;
                                        if PhysFrameIndex::from(addr) == PhysFrameIndex::from(entry.phys_addr) {
                                            format!("page:{:X}", addr.frame_offset())
                                        } else {
                                            format!("{}", self.entries[link.0].phys_addr)
                                        }
                                    },
                                    None => "..".to_string(),
                                })
                                .format(", ")
                        )
                    }
                },
                LinksSnapshot::Speculative(v) => {
                    format!("speculative {}", v.iter().format(", "))
                },
            };

            let offset = addr.frame_offset();
            write!(f, "{padding}{offset:03X} {entry_point}{jit_kind} {instr:<32X} {next}")?;

            writeln!(f)?;
        }

        writeln!(f, "Frames with pending checks: {num_with_pending_checks} / {num_code_frames}")?;

        Ok(())
    }
}
