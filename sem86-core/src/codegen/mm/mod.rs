use elf::ElfBytes;
use elf::endian::LittleEndian;
use mem_dbg::MemSize;

pub mod bump;

#[derive(Clone, MemSize)]
pub struct Object(Vec<u8>);

impl Object {
    pub fn new(data: Vec<u8>) -> Self {
        Self(data)
    }

    pub fn parse(&self) -> ElfBytes<'_, LittleEndian> {
        ElfBytes::<LittleEndian>::minimal_parse(&self.0).unwrap()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn size(&self) -> usize {
        self.0.len()
    }
}
