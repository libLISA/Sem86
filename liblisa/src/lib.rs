#![allow(incomplete_features)]
#![deny(rustdoc::missing_crate_level_docs, rustdoc::invalid_codeblock_attributes)]
#![feature(generic_const_exprs)]
#![doc(html_no_source)]

pub mod arch;
pub mod encoding;
pub mod instr;
pub mod semantics;
pub mod state;
pub mod utils;
pub mod value;

pub use instr::Instruction;
