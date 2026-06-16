use std::fmt::Display;

use itertools::Itertools;

use super::{Dataflows, ParameterizedComputation};
use crate::arch::Arch;
use crate::encoding::Encoding;
use crate::encoding::bitpattern::{MappingOrBitOrder, PART_NAMES, Part, PartMapping, PartValue};
use crate::encoding::dataflows::AccessKind;
use crate::semantics::{ARG_NAMES, Computation};

impl<A: Arch, C: Computation> Display for Dataflows<A, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, access) in self.addresses.iter().enumerate() {
            write!(
                f,
                "{:10} = ",
                format!(
                    "Addr{}(m{}; {} bytes)",
                    match access.kind {
                        AccessKind::Executable => "X ",
                        AccessKind::Input => "R ",
                        AccessKind::InputOutput => "RW",
                    },
                    index,
                    access.size.end,
                )
            )?;

            let names = access.inputs.iter().map(|input| format!("{input}")).collect::<Vec<_>>();

            match &access.calculation {
                ParameterizedComputation::FromPart(index) => {
                    writeln!(f, "Part[{index}] with inputs {}", names.iter().format(", "))?
                },
                ParameterizedComputation::Calculation(c) => writeln!(f, "{}", c.display(&names))?,
            }
        }

        for output in self.output_dataflows() {
            match &output.computation {
                Some(computation) => {
                    let names = output.inputs.iter().map(|input| input.to_string()).collect::<Vec<_>>();

                    writeln!(
                        f,
                        "{:10} := {}",
                        format!("{}", output.target.clone()),
                        computation.display(&names)
                    )?;
                },
                None => {
                    if output.unobservable_external_inputs {
                        writeln!(
                            f,
                            "{:10} := <unobservable external inputs>",
                            format!("{}", output.target.clone())
                        )?;
                    } else {
                        writeln!(f, "{:10} := {}", format!("{}", output.target.clone()), output.inputs)?;
                    }
                },
            }
        }

        if !self.write_ordering.is_empty() {
            writeln!(f, "write ordering: {:?}", self.write_ordering)?;
        }

        Ok(())
    }
}

impl<A: Arch, S: Display, M> Display for Encoding<A, S, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self
            .bits
            .chunks(8)
            .rev()
            .map(|byte| byte.iter().rev().map(|s| format!("{s:?}")).join(""));
        writeln!(
            f,
            "[{}] {} = {}",
            bytes
                .clone()
                .take(self.equivalent_prefixes.num_bytes_to_replace())
                .format(" "),
            bytes
                .clone()
                .skip(self.equivalent_prefixes.num_bytes_to_replace())
                .format(" "),
            self.instr().bytes().iter().map(|b| format!("{b:02X}")).format("")
        )?;

        write!(f, "{}", self.semantics)?;

        writeln!(f, "---")?;

        for (index, part) in self.parts.iter().enumerate() {
            writeln!(f, "<{}>{}", PART_NAMES[index], part)?;
        }

        Ok(())
    }
}

struct DisplayPartMapping<'a, A: Arch> {
    mapping: &'a PartMapping<A>,
    value: u64,
    size: usize,
}

impl<A: Arch> Display for DisplayPartMapping<'_, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.mapping {
            PartMapping::Register {
                mapping, ..
            } => {
                for (key, value) in mapping.iter().enumerate() {
                    if key != 0 {
                        write!(f, ", ")?;
                    }

                    write!(f, "{:0pad$b} => ", key, pad = self.size)?;
                    if let Some(reg) = value {
                        if key as u64 == self.value {
                            write!(f, "[ {reg} ]")?;
                        } else {
                            write!(f, "{reg}")?;
                        }
                    } else {
                        write!(f, "-")?;
                    }
                }
            },
            PartMapping::Imm {
                mapping,
                bits,
                ..
            } => {
                write!(f, "immediate = 0x{:X}", self.value)?;

                if let Some(MappingOrBitOrder::Mapping(mapping)) = mapping
                    && mapping.iter().any(PartValue::is_invalid)
                {
                    for (value, item) in mapping.iter().enumerate() {
                        if item.is_invalid() {
                            write!(f, " exclude {value}")?;
                        }
                    }
                }

                if let Some(bits) = bits {
                    write!(f, " [bits: {bits:?}]")?;
                }
            },
            PartMapping::MemoryComputation {
                mapping, ..
            } => {
                for (key, value) in mapping.iter().enumerate() {
                    if key != 0 {
                        write!(f, ", ")?;
                    }

                    write!(f, "{:0pad$b} => ", key, pad = self.size)?;
                    if let Some(computation) = value {
                        if key as u64 == self.value {
                            write!(f, "[ {} ]", computation.display(ARG_NAMES))?;
                        } else {
                            write!(f, "{}", computation.display(ARG_NAMES))?;
                        }
                    } else {
                        write!(f, "-")?;
                    }
                }
            },
        }

        Ok(())
    }
}

impl<A: Arch> Display for Part<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{} bits]: {}",
            self.size,
            DisplayPartMapping {
                mapping: &self.mapping,
                value: self.value,
                size: self.size,
            }
        )
    }
}
