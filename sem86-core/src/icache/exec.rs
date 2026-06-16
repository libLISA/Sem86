use std::fmt::Display;

use arrayvec::ArrayVec;

use crate::arch::intel386::Intel386;
use crate::codegen::page::PageCode;
use crate::decoder::EncodingLookup;
use crate::il::part_values::PartValues;
use crate::il::{MakeEncoding, MiniSem};

#[derive(Copy, Clone)]
pub enum Executable<'r, 'tag, F> {
    Single {
        part_values: PartValues,
        instr_len: u8,
        execute: F,
    },
    JittedPage {
        page: &'r PageCode<'tag>,
    },
}

pub struct EncodingInfo {
    pub encoding_index: usize,
    pub part_values: PartValues,
    pub instr_len: u8,
}

impl EncodingInfo {
    pub fn instance(&self, encodings: &impl EncodingLookup) -> MiniSem<Intel386> {
        let e = &encodings.get(self.encoding_index);
        let part_values = self.part_values.unpack(e.semantics.part_packing).collect::<ArrayVec<_, 6>>();
        e.make_encoding().instantiate(&part_values).unwrap()
    }

    pub fn display_instance<'a>(&'a self, encodings: &'a impl EncodingLookup) -> impl Display + 'a {
        DisplayInstance {
            encodings,
            encoding_index: self.encoding_index,
            part_values: self.part_values,
        }
    }
}

struct DisplayInstance<'e, T> {
    part_values: PartValues,
    encoding_index: usize,
    encodings: &'e T,
}

impl<T: EncodingLookup> Display for DisplayInstance<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let e = &self.encodings.get(self.encoding_index);
        let part_values = self.part_values.unpack(e.semantics.part_packing).collect::<ArrayVec<_, 6>>();
        write!(f, "{}", e.make_encoding().instantiate(&part_values).unwrap())
    }
}
