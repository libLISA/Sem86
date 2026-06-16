use std::fmt::{Debug, Formatter};
use std::io::{BufRead, ErrorKind, Read, Seek};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use bytemuck::checked::cast_slice;
use bytemuck::{Pod, Zeroable, cast_ref};
use lz4_flex::frame::{FrameDecoder, FrameDecoderSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub enum TraceEntry<'a> {
    Instr(&'a TraceInstrExecuted),
    MemAssert(TraceAssertMem),
    Int(TraceInt),
    In(TracePortIo),
    Out(TracePortIo),
}

impl TraceEntry<'_> {
    pub fn into_owned(self) -> OwnedTraceEntry {
        match self {
            TraceEntry::Instr(trace_instr_executed) => OwnedTraceEntry::Instr(*trace_instr_executed),
            TraceEntry::MemAssert(trace_assert_mem) => OwnedTraceEntry::MemAssert(trace_assert_mem),
            TraceEntry::Int(trace_int) => OwnedTraceEntry::Int(trace_int),
            TraceEntry::In(trace_port_io) => OwnedTraceEntry::In(trace_port_io),
            TraceEntry::Out(trace_port_io) => OwnedTraceEntry::Out(trace_port_io),
        }
    }
}

#[derive(Clone, Debug)]
pub enum OwnedTraceEntry {
    Instr(TraceInstrExecuted),
    MemAssert(TraceAssertMem),
    Int(TraceInt),
    In(TracePortIo),
    Out(TracePortIo),
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct SegmentDescriptor {
    pub base: u32,
    pub limit: u16,
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct SegmentRef {
    pub value: u16,
    pub cached_base: u32,
    pub limit: u32,
}

#[derive(Copy, Clone, Pod, Zeroable)]
#[repr(C)]
pub struct MmVal {
    fraction: u64,
    exp: u16,
    padding: [u16; 3],
}

impl Debug for MmVal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.as_u128(), f)
    }
}

impl MmVal {
    pub fn as_u128(&self) -> u128 {
        self.fraction as u128 | ((self.exp as u128) << 64)
    }
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct TraceInstrExecuted {
    pub len: u8,
    pub bytes: [u8; 15],
    pub eax: u32,
    pub ecx: u32,
    pub edx: u32,
    pub ebx: u32,
    pub esp: u32,
    pub ebp: u32,
    pub esi: u32,
    pub edi: u32,
    pub eip: u32,
    pub eflags: u32,
    pub gdtr: SegmentDescriptor,
    pub idtr: SegmentDescriptor,
    pub ldtr: SegmentRef,
    pub tr: SegmentRef,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub es: SegmentRef,
    pub cs: SegmentRef,
    pub ss: SegmentRef,
    pub ds: SegmentRef,
    pub fs: SegmentRef,
    pub gs: SegmentRef,
    pub fcw: u16,
    pub fsw: u16,
    pub mm: [MmVal; 8],
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct TraceAssertMem {
    pub paddr: u32,
    pub laddr: u32,
    pub len: u8,
    pub data: [u8; 31],
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct TraceInt {
    pub vector: u8,
}

#[derive(Copy, Clone, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct TracePortIo {
    pub port: u16,
    pub len: u8,
    pub value: u32,
}

pub struct TraceEntryReader<R: Read> {
    file: FrameDecoder<R>,
    buf: [u32; 74],
    byte_buf: [u8; 1024],
    cached_next: Option<u8>,
    bytes_read: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceSnapshot {
    bytes_read: usize,
    cached_next: Option<u8>,
    #[serde(default)]
    fast_resume: Option<FastResume>,
    #[serde(with = "serde_big_array::BigArray")]
    buf: [u32; 74],
}

#[derive(Clone, Serialize, Deserialize)]
struct FastResume {
    decoder: FrameDecoderSnapshot,
    seek_pos: u64,
}

impl std::fmt::Debug for FastResume {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastResume").field("seek_pos", &self.seek_pos).finish()
    }
}

impl<R: Read + Seek> TraceEntryReader<R> {
    pub fn next(&mut self, abort_on_instr: bool) -> Option<TraceEntry<'_>> {
        let next_item = *self.cached_next.get_or_insert_with(|| {
            let mut buf = [0; 1];
            match self.file.read_exact(&mut buf) {
                Ok(_) => (),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => buf[0] = 0xff,
                Err(e) => panic!("{e}"),
            };
            self.bytes_read += 1;
            buf[0]
        });

        if abort_on_instr && [0, 3, 4].contains(&next_item) {
            return None
        }

        self.cached_next = None;

        Some(match next_item {
            0 => TraceEntry::Instr(self.read_instr()),
            1 => TraceEntry::MemAssert(self.read()),
            2 => TraceEntry::Int(self.read()),
            3 => TraceEntry::Out(self.read()),
            4 => TraceEntry::In(self.read()),
            0xff => return None,
            n => panic!("unknown trace entry type {n}"),
        })
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    pub fn gb_read(&self) -> f64 {
        self.bytes_read() as f64 / 1024.0 / 1024.0 / 1024.0
    }

    pub fn new(stream: FrameDecoder<R>) -> Self {
        Self {
            file: stream,
            buf: [0; _],
            byte_buf: [0; _],
            cached_next: None,
            bytes_read: 0,
        }
    }

    fn read<T: Pod>(&mut self) -> T {
        self.file.read_exact(&mut self.byte_buf[..size_of::<T>()]).unwrap();
        self.bytes_read += size_of::<T>();

        *bytemuck::from_bytes(&self.byte_buf[..size_of::<T>()])
    }

    fn read_instr(&mut self) -> &TraceInstrExecuted {
        self.file.read_exact(&mut self.byte_buf[..12]).unwrap();
        let mask: [u32; 3] = cast_slice::<_, u32>(&self.byte_buf[..12]).try_into().unwrap();
        let num = mask.iter().map(|v| v.count_ones() as usize).sum::<usize>();
        self.file.read_exact(&mut self.byte_buf[..num * 4]).unwrap();

        let items: &[u32] = cast_slice(&self.byte_buf[..num * 4]);
        let mut k = 0;
        for (i, &m) in mask.iter().enumerate() {
            let mut bits = m;
            while bits != 0 {
                let tz = bits.trailing_zeros() as usize;
                self.buf[i * 32 + tz] = items[k];
                k += 1;
                bits &= bits - 1;
            }
        }

        self.bytes_read += 12 + num * 4;

        cast_ref(&self.buf)
    }

    pub fn snapshot(&mut self) -> TraceSnapshot {
        TraceSnapshot {
            fast_resume: Some(FastResume {
                decoder: self.file.snapshot(),
                seek_pos: self.file.get_mut().stream_position().unwrap(),
            }),
            bytes_read: self.bytes_read,
            cached_next: self.cached_next,
            buf: self.buf,
        }
    }

    pub fn restore(mut self, snapshot: TraceSnapshot, mut progress: impl FnMut(usize, usize)) -> Self {
        assert!(self.bytes_read <= snapshot.bytes_read);
        self.cached_next = snapshot.cached_next;
        self.buf = snapshot.buf;

        if let Some(f) = snapshot.fast_resume {
            self.file = f.decoder.into_decoder({
                let mut inner = self.file.into_inner();
                inner.seek(std::io::SeekFrom::Start(f.seek_pos)).unwrap();
                inner
            });
            self.bytes_read = snapshot.bytes_read;
        } else {
            progress(0, snapshot.bytes_read);
            let mut last_print = 0usize;
            while self.bytes_read < snapshot.bytes_read {
                // Avoid an unnecessary copy by not reading bytes out of the internal buffer.
                let data = self.file.fill_buf().unwrap();
                let remaining = snapshot.bytes_read - self.bytes_read;
                let to_read = remaining.min(data.len());
                self.file.consume(to_read);
                self.bytes_read += to_read;

                if self.bytes_read - last_print > (64 << 20) {
                    progress(self.bytes_read, snapshot.bytes_read);
                    last_print = self.bytes_read;
                }
            }
        }

        assert_eq!(self.bytes_read, snapshot.bytes_read);
        self
    }
}

pub struct ChannelReader<T> {
    recv: Receiver<T>,
}

impl<T> Iterator for ChannelReader<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv.recv().ok()
    }
}

pub struct ChannelIter<I: Iterator> {
    iter: I,
    recv: Option<Receiver<I::Item>>,
    send: SyncSender<I::Item>,
}

impl<I: Iterator> ChannelIter<I> {
    pub fn new(iter: I) -> Self {
        let (send, recv) = sync_channel(1_000_000);
        Self {
            iter,
            recv: Some(recv),
            send,
        }
    }

    pub fn get_reader(&mut self) -> ChannelReader<I::Item> {
        ChannelReader {
            recv: self.recv.take().unwrap(),
        }
    }

    pub fn run(&mut self) {
        for item in &mut self.iter {
            self.send.send(item).unwrap();
        }
    }
}
