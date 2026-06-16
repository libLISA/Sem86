use std::fs::File;
use std::io::Write;
use std::iter::repeat_with;
use std::path::Path;

use bitcode::{Decode, Encode};
use lz4_flex::frame::{AutoFinishEncoder, FrameDecoder, FrameEncoder};
use sem86_arch::exceptions::Exception;

use crate::SegmentSizes;
use crate::arch::intel386::State;
use crate::il::ExecResult;

#[derive(Clone, Debug, Encode, Decode)]
pub struct MemInfo {
    pub addr: u64,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Clone, Debug, Encode, Decode)]
pub struct ChainTest {
    pub entry_point_ip: u32,
    pub instrs: Vec<()>,
    pub state_before: State,
    pub state_after: State,
    pub mem: Vec<MemInfo>,
    pub segment_sizes: SegmentSizes,
    pub expected_result: Result<ExecResult, Exception>,
}

pub struct TestDbWriter {
    writer: AutoFinishEncoder<File>,
}

impl TestDbWriter {
    pub fn create(path: impl AsRef<Path>) -> Self {
        Self {
            writer: FrameEncoder::new(File::create(path.as_ref()).unwrap()).auto_finish(),
        }
    }

    pub fn add(&mut self, entry: &ChainTest) -> Result<(), ()> {
        let data = bitcode::encode(entry);
        self.writer.write_all(&u32::to_le_bytes(data.len() as u32)).unwrap();
        self.writer.write_all(&data).unwrap();

        Ok(())
    }
}

pub struct TestDbReader {
    reader: FrameDecoder<File>,
}

impl TestDbReader {
    pub fn load(path: impl AsRef<Path>) -> Self {
        Self {
            reader: FrameDecoder::new(File::open(path.as_ref()).unwrap()),
        }
    }

    pub fn into_iter(mut self) -> impl Iterator<Item = ChainTest> {
        use std::io::Read;
        repeat_with(move || {
            let mut buf = [0; 4];
            self.reader.read_exact(&mut buf).ok()?;
            let len = u32::from_le_bytes(buf);

            let mut data = vec![0; len as usize];
            self.reader.read_exact(&mut data).ok()?;

            Some(bitcode::decode(&data).unwrap())
        })
        .take_while(|item| item.is_some())
        .flatten()
    }
}
