use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, Ordering};

use bilge::prelude::*;
use bitcode::{Decode, Encode};
use log::warn;
use serde::{Deserialize, Serialize};

use super::reg::{Reg8, Reg16};

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Mask {
    selected_channel: u2,
    mask: bool,
    reserved: u5,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Mode {
    selected_channel: u2,
    operation: u2,
    reserved: u1,
    decrement: bool,
    mode: u2,
}

#[derive(Clone, Debug)]
pub struct Dma {
    addresses: [Reg16; 8],
    counts: [Reg16; 8],
    page_addrs: [Reg8; 8],
    mask: [bool; 8],

    fdd_addr: Arc<AtomicU32>,
    fdd_len: Arc<AtomicU16>,
    fdd_mode: Arc<AtomicU8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct DmaSnapshot {
    addresses: [Reg16; 8],
    counts: [Reg16; 8],
    page_addrs: [Reg8; 8],
    mask: [bool; 8],

    fdd_addr: u32,
    fdd_len: u16,
    fdd_mode: u8,
}

impl Dma {
    pub fn new(fdd_addr: Arc<AtomicU32>, fdd_len: Arc<AtomicU16>, fdd_mode: Arc<AtomicU8>) -> Self {
        Self {
            addresses: [(); 8].map(|_| Reg16::new(0)),
            counts: [(); 8].map(|_| Reg16::new(0)),
            page_addrs: [(); 8].map(|_| Reg8::new(0)),
            mask: [true; 8],
            fdd_addr,
            fdd_len,
            fdd_mode,
        }
    }

    fn dma_range(dma_index: u8) -> Range<usize> {
        dma_index as usize * 4..(dma_index as usize + 1) * 4
    }

    pub fn read_address(&mut self, channel: u8) -> u8 {
        self.addresses[channel as usize].read()
    }

    pub fn write_address(&mut self, channel: u8, val: u8) {
        self.addresses[channel as usize].write(val);

        if channel == 2 {
            self.update_fdd_addr();
        }
    }

    pub fn read_count(&mut self, channel: u8) -> u8 {
        self.counts[channel as usize].read()
    }

    pub fn write_count(&mut self, channel: u8, val: u8) {
        self.counts[channel as usize].write(val);
    }

    pub fn read_status(&self, dma_index: u8) -> u8 {
        warn!("TODO: read DMA #{dma_index} status");
        1
    }

    pub fn write_mask(&mut self, dma_index: usize, val: u8) {
        let val = Mask::from(val);
        log::error!("Unable to handle DMA mask: {val:?} for DMA #{dma_index}");
    }

    pub fn write_command(&self, _dma_index: u8, _val: u8) {
        todo!()
    }

    pub fn write_request(&self, _dma_index: u8, _val: u8) {
        todo!()
    }

    pub fn write_mode(&self, dma_index: u8, val: u8) {
        let mode = Mode::from(val);
        assert!(!mode.decrement());
        // assert_eq!(mode.mode(), u2::new(1)); // single mode

        if dma_index == 0 && mode.selected_channel() == u2::new(2) {
            self.fdd_mode.store(mode.operation().as_u8(), Ordering::Relaxed);
        }
    }

    pub fn clear_byte_flip_flop(&mut self, dma_index: u8) {
        for reg in self.addresses[Self::dma_range(dma_index)].iter_mut() {
            reg.make_next_byte_low();
        }

        for reg in self.counts[Self::dma_range(dma_index)].iter_mut() {
            reg.make_next_byte_low();
        }
    }

    pub fn master_clear(&mut self, dma_index: u8) {
        for item in self.mask[Self::dma_range(dma_index)].iter_mut() {
            *item = true;
        }

        // TODO: Reset command register
        // TODO: Reset status register
        self.clear_byte_flip_flop(dma_index);
    }

    pub fn clear_mask(&self, _dma_index: u8, _val: u8) {
        todo!()
    }

    pub fn write_all_mask_bits(&self, _dma_index: u8, _val: u8) {
        todo!()
    }

    pub fn read_page_addr_reg(&mut self, addr: u8) -> u8 {
        self.page_addrs[addr as usize].read()
    }

    pub fn write_page_addr_reg(&mut self, addr: u8, val: u8) {
        self.page_addrs[addr as usize].write(val);
        if addr == 2 {
            self.update_fdd_addr();
        }
    }

    fn update_fdd_addr(&mut self) {
        self.fdd_addr.store(
            self.addresses[2].value() as u32 | ((self.page_addrs[2].value() as u32) << 16),
            Ordering::Relaxed,
        );
        self.fdd_len.store(self.counts[2].value(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DmaSnapshot {
        DmaSnapshot {
            addresses: self.addresses.clone(),
            counts: self.counts.clone(),
            page_addrs: self.page_addrs.clone(),
            mask: self.mask,
            fdd_addr: self.fdd_addr.load(Ordering::SeqCst),
            fdd_len: self.fdd_len.load(Ordering::SeqCst),
            fdd_mode: self.fdd_mode.load(Ordering::SeqCst),
        }
    }

    pub fn restore(&mut self, dma: DmaSnapshot) {
        self.addresses = dma.addresses;
        self.counts = dma.counts;
        self.page_addrs = dma.page_addrs;
        self.mask = dma.mask;
        self.fdd_addr.store(dma.fdd_addr, Ordering::SeqCst);
        self.fdd_len.store(dma.fdd_len, Ordering::SeqCst);
        self.fdd_mode.store(dma.fdd_mode, Ordering::SeqCst);
    }
}
