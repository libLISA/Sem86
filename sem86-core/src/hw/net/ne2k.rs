use std::ops::{Index, Range};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bilge::prelude::*;
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use smoltcp::wire::{EthernetAddress, EthernetFrame};

use crate::hw::net::unat::UserspaceNat;
use crate::hw::pci::{CommonPciHeader, DeviceWriteEvent, GeneralDeviceHeader, PciCommandRegister, PciDevice};
use crate::hw::pic::DualDynamicIrqLine;
use crate::hw::ports::{PortError, PortIoData, WithIoSpace};
use crate::util::ByteSubstitutions;

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct ControlRegister {
    stop: bool,
    start: bool,
    transmit_packet: bool,
    remote_dma_command: u3,
    page_select: u2,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct InterruptStatusReg {
    packet_received: bool,
    packet_transmitted: bool,
    receive_error: bool,
    transmit_error: bool,
    overwrite_warning: bool,
    counter_overflow: bool,
    remote_dma_complete: bool,
    reset_status: bool,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, Serialize, Deserialize)]
pub enum WordTransferSize {
    ByteWide,
    WordWide,
}

impl WordTransferSize {
    fn num_bytes(&self) -> u16 {
        match self {
            WordTransferSize::ByteWide => 1,
            WordTransferSize::WordWide => 2,
        }
    }
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, Serialize, Deserialize)]
pub enum ByteOrder {
    LittleEndian,
    BigEndian,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, Serialize, Deserialize)]
pub enum DmaMode {
    Dual16,
    Single32,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct DataConfigurationReg {
    word_transfer_size: WordTransferSize,
    byte_order: ByteOrder,
    dma_mode: DmaMode,
    disable_loopback: bool,
    auto_init_remote: bool,
    /// 0 = 1 * wts
    /// 1 = 2 * wts
    /// 2 = 4 * wts
    /// 3 = 6 * wts
    fifo_threshold: u2,
    reserved: u1,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct TransmitConfigurationReg {
    inhibit_crc: bool,
    encoded_loopback_control: u2,
    disable_auto_transmit: bool,
    enable_collision_offset: bool,
    reserved: u3,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct TransmitStatusReg {
    packed_transmitted: bool,
    reserved: bool,
    transmit_collided: bool,
    transmit_aborted: bool,
    carrier_sense_lost: bool,
    fifo_underrun: bool,
    cd_heartbeat: bool,
    out_of_window_collision: bool,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct ReceiveConfigurationReg {
    save_errored_packets: bool,
    accept_runt_packets: bool,
    accept_broadcast: bool,
    accept_multicast: bool,
    enable_promiscuous_physical: bool,
    monitor_mode: bool,
    reserved: u2,
}

#[bitsize(1)]
#[derive(Copy, Clone, Debug, FromBits, Serialize, Deserialize)]
pub enum AddressMatch {
    Physical,
    MulticastOrBroadcast,
}

#[bitsize(8)]
#[derive(Copy, Clone, DebugBits, FromBits, Serialize, Deserialize)]
pub struct ReceiveStatusReg {
    packed_received_intact: bool,
    crc_error: bool,
    frame_alignment_error: bool,
    fifo_overrun: bool,
    missed_packet: bool,
    address_match: AddressMatch,
    receiver_disabled: bool,
    deferring: bool,
}

/// 64KiB of private RAM that can be read/written via PIO
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PrivateRam {
    mac_addr: [u8; 32],
    data: Vec<u8>,
}

impl Index<Range<u16>> for PrivateRam {
    type Output = [u8];

    fn index(&self, index: Range<u16>) -> &Self::Output {
        let start = index.start - (16 << 10);
        let end = index.end - (16 << 10);
        &self.data[start as usize..end as usize]
    }
}

impl PrivateRam {
    pub fn new() -> Self {
        Self {
            mac_addr: [
                0xB0, 0xB0, 0xC4, 0xC4, 0x20, 0x20, 0, 0, 0, 0, 0, 0, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57,
                0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57, 0x57,
            ],
            data: vec![0; 32 << 10],
        }
    }

    pub fn read(&self, pos: u16) -> u8 {
        if let Some(val) = self.mac_addr.get(pos as usize) {
            *val
        } else {
            pos.checked_sub(16 << 10)
                .map(|pos| self.data.get(pos as usize).copied().unwrap_or(0))
                .unwrap_or(0xff)
        }
    }

    pub fn write(&mut self, pos: u16, val: u8) {
        if let Some(pos) = pos.checked_sub(16 << 10)
            && let Some(current_byte) = self.data.get_mut(pos as usize)
        {
            *current_byte = val
        }
    }

    fn copy_from_slice(&mut self, start_offset: u16, bytes: &[u8]) {
        if let Some(pos) = start_offset.checked_sub(16 << 10) {
            self.data[pos as usize..pos as usize + bytes.len()].copy_from_slice(bytes);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ne2kState {
    pci_header: GeneralDeviceHeader,

    control_reg: ControlRegister,
    isr: InterruptStatusReg,
    imr: InterruptStatusReg,
    dcr: DataConfigurationReg,
    tcr: TransmitConfigurationReg,
    tsr: TransmitStatusReg,
    rcr: ReceiveConfigurationReg,
    rsr: ReceiveStatusReg,

    // Page 0
    local_dma: u16,
    page_start: u8,
    page_stop: u8,
    bound_ptr: u8,
    tx_page_start: u8,
    num_coll: u8,
    tx_bytes: u16,
    fifo: u8,
    remote_dma: u16,
    remote_start: u16,
    remote_bytes: u16,
    tallycnt: [u8; 3],

    // Page 1
    phys_addr: [u8; 6],
    curr_page: u8,
    multicast_hash: [u8; 8],

    // Page 2
    remote_next_packet_pointer: u8,
    local_packet_pointer: u8,
    address_count: u16,

    private_ram: PrivateRam,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ne2kSnapshot {
    state: Ne2kState,
}

#[derive(Debug)]
pub struct Ne2k {
    state: Ne2kState,
    irq: DualDynamicIrqLine,
    nat: UserspaceNat,
    irq_line: Arc<AtomicU8>,
    rx_irq_enabled: Arc<AtomicBool>,
}

impl Ne2k {
    pub fn new(irq: DualDynamicIrqLine) -> Self {
        let rx_irq_enabled = Arc::new(AtomicBool::new(false));
        let enabled = rx_irq_enabled.clone();
        let irq_copy = irq.clone();
        let irq_line = Arc::new(AtomicU8::new(1));
        let irq_line_copy = irq_line.clone();
        let private_ram = PrivateRam::new();
        let nat = UserspaceNat::new(move || {
            if enabled.load(Ordering::SeqCst) {
                irq_copy.set(irq_line_copy.load(Ordering::SeqCst), true);
            }
        });

        let mut control_reg = ControlRegister::from(0);
        control_reg.set_stop(true);
        control_reg.set_remote_dma_command(u3::new(4));

        let mut dcr = DataConfigurationReg::from(0);
        dcr.set_dma_mode(DmaMode::Single32);

        let mut isr = InterruptStatusReg::from(0);
        isr.set_reset_status(true);
        Self {
            state: Ne2kState {
                pci_header: GeneralDeviceHeader {
                    common: CommonPciHeader {
                        vendor_id: 0x10ec,
                        device_id: 0x8029,
                        command: PciCommandRegister::from(0x01),
                        status: 0x0200,
                        revision_id: 0,
                        prog_if: 0,
                        subclass: 0,
                        class_code: 0x02,
                        cache_line_size: 0,
                        latency_timer: 0,
                        header_type: 0,
                        bist: 0,
                    },
                    bar: [0; 6],
                    cardbus_cis_pointer: 0,
                    subsystem_vendor_id: 0,
                    subsystem_id: 0,
                    expansion_rom_base_address: 0,
                    capabilities_pointer: 0,
                    reserved1: [0; 3],
                    reserved2: 0,
                    interrupt_line: 1,
                    interrupt_pin: 1,
                    min_grant: 0,
                    max_latency: 0,
                },
                control_reg,
                isr,
                imr: InterruptStatusReg::from(0),
                dcr,
                tcr: TransmitConfigurationReg::from(0),
                tsr: TransmitStatusReg::from(0),
                rcr: ReceiveConfigurationReg::from(0),
                rsr: ReceiveStatusReg::from(0),
                local_dma: 0,
                page_start: 0,
                page_stop: 0,
                bound_ptr: 0,
                tx_page_start: 0,
                num_coll: 0,
                tx_bytes: 0,
                fifo: 0,
                remote_dma: 0,
                remote_start: 0,
                remote_bytes: 0,
                tallycnt: [0; _],
                phys_addr: [0xB0, 0xC4, 0x20, 0x00, 0x00, 0x00],
                curr_page: 0,
                multicast_hash: [0; _],
                remote_next_packet_pointer: 0,
                local_packet_pointer: 0,
                address_count: 0,
                private_ram,
            },
            irq,
            nat,
            rx_irq_enabled,
            irq_line,
        }
    }

    fn base_addresses(&self) -> BaseAddresses {
        BaseAddresses {
            io_base: (self.state.pci_header.bar[0] & BAR_MASK) as u16,
        }
    }

    fn read<S: PortIoData>(&mut self, addr: u16) -> Result<S, PortError> {
        self.check_recv_packet();

        if addr == 0x10 {
            S::from_u32(0u16, || {
                let mut result = 0u32;
                // Split reads above WORD size into multiple WORD reads.
                for index in 0..(S::SIZE / 2).max(1) {
                    let val = match self.state.dcr.word_transfer_size() {
                        WordTransferSize::ByteWide => self.state.private_ram.read(self.state.remote_dma) as u16,
                        WordTransferSize::WordWide => {
                            self.state.private_ram.read(self.state.remote_dma) as u16
                                | ((self.state.private_ram.read(self.state.remote_dma + 1) as u16) << 8)
                        },
                    };

                    debug!(
                        "Reading private ram ({:?}) at 0x{:04X} = 0x{val:X}",
                        self.state.dcr.word_transfer_size(),
                        self.state.remote_dma
                    );
                    self.state.remote_dma += self.state.dcr.word_transfer_size().num_bytes();

                    if self.state.remote_dma == (self.state.page_stop as u16) << 8 {
                        self.state.remote_dma = (self.state.page_start as u16) << 8;
                    }

                    self.state.remote_bytes = self
                        .state
                        .remote_bytes
                        .saturating_sub(self.state.dcr.word_transfer_size().num_bytes());

                    if self.state.remote_bytes == 0 {
                        info!("All bytes read");
                        self.state.isr.set_remote_dma_complete(true);
                        self.check_irq();
                    }

                    result |= (val as u32) << (index * 16);
                }

                debug!("Final private RAM value read: 0x{result:X}");

                result
            })
        } else {
            S::from_u8(|| {
                let result = if addr == 0x1f {
                    self.reset();
                    0
                } else if addr >= 0x10 {
                    warn!("read from unimplemented asic register at 0x{addr:X}");
                    0
                } else if addr == 0 {
                    self.state.control_reg.value
                } else {
                    match self.state.control_reg.page_select().as_u8() {
                        0 => match addr {
                            0x1 => self.state.local_dma.get_byte(0),
                            0x2 => self.state.local_dma.get_byte(1),
                            0x3 => self.state.bound_ptr,
                            0x4 => self.state.tsr.value,
                            0x5 => self.state.num_coll,
                            0x6 => self.state.fifo,
                            0x7 => self.state.isr.value,
                            0x8 => self.state.remote_dma.get_byte(0),
                            0x9 => self.state.remote_dma.get_byte(1),
                            0xa => 0x50, // ???
                            0xb => 0x43, // ???
                            0xc => self.state.rsr.value,
                            0xd..=0xf => self.state.tallycnt[addr as usize - 0xd],
                            _ => unreachable!(),
                        },
                        1 => match addr {
                            0x1..=0x6 => self.state.phys_addr[addr as usize - 0x1],
                            0x7 => self.state.curr_page,
                            0x8..=0xf => self.state.multicast_hash[addr as usize - 0x8],
                            _ => unreachable!(),
                        },
                        2 => match addr {
                            0x1 => self.state.page_start,
                            0x2 => self.state.page_stop,
                            0x3 => self.state.remote_next_packet_pointer,
                            0x4 => self.state.tx_page_start,
                            0x5 => self.state.local_packet_pointer,
                            0x6 => self.state.address_count.get_byte(0),
                            0x7 => self.state.address_count.get_byte(1),
                            0x8..=0xb => unimplemented!(),
                            0xc => self.state.rcr.value,
                            0xd => self.state.tcr.value,
                            0xe => self.state.dcr.value,
                            0xf => self.state.imr.value,
                            _ => unreachable!(),
                        },
                        3 => match addr {
                            0x3 => 0,
                            0x5 | 0x6 => 0x40,
                            _ => 0,
                        },
                        _ => unreachable!("page_select is u2"),
                    }
                };

                debug!(
                    "Read from address {}:0x{addr:X} = 0x{result:02X}",
                    self.state.control_reg.page_select()
                );
                result
            })
        }
    }

    fn write<S: PortIoData>(&mut self, addr: u16, val: S) -> Result<(), PortError> {
        self.check_recv_packet();

        if addr == 0x10 {
            let u32_val = val.u32();
            // Split DWORD writes into two WORD writes
            let vals = if S::SIZE >= 4 {
                &[u32_val as u16, (u32_val >> 16) as u16] as &[_]
            } else {
                &[u32_val as u16]
            };

            for &val in vals {
                debug!("Write private RAM at 0x{:04X} = 0x{val:X}", self.state.remote_dma);
                match self.state.dcr.word_transfer_size() {
                    WordTransferSize::ByteWide => {
                        self.state.private_ram.write(self.state.remote_dma, val as u8);
                    },
                    WordTransferSize::WordWide => {
                        self.state.private_ram.write(self.state.remote_dma, val.get_byte(0));
                        self.state.private_ram.write(self.state.remote_dma + 1, val.get_byte(1));
                    },
                };

                self.state.remote_dma += self.state.dcr.word_transfer_size().num_bytes();

                if self.state.remote_dma == (self.state.page_stop as u16) << 8 {
                    self.state.remote_dma = (self.state.page_start as u16) << 8;
                }

                self.state.remote_bytes = self
                    .state
                    .remote_bytes
                    .saturating_sub(self.state.dcr.word_transfer_size().num_bytes());

                if self.state.remote_bytes == 0 {
                    info!("All bytes written");
                    self.state.isr.set_remote_dma_complete(true);
                    self.check_irq();
                }
            }
        } else if addr == 0x1f {
            // Ignore write to reset register
        } else if addr >= 0x10 {
            unimplemented!("reserved asic registers")
        } else if addr == 0 {
            let val = val.require_u8()?;
            let cr = ControlRegister::from(val);
            debug!("Write control register = {val:02X} = {cr:?}");
            let remote_dma_command = if cr.remote_dma_command().as_u8() == 0 {
                4
            } else {
                cr.remote_dma_command().as_u8()
            };

            self.state.control_reg.set_remote_dma_command(u3::new(remote_dma_command));
            self.state.control_reg.set_stop(cr.stop());
            self.state.control_reg.set_start(cr.start());
            self.state.control_reg.set_page_select(cr.page_select());
            if cr.stop() {
                self.state.isr.set_reset_status(true);
            }

            if cr.start() {
                self.state.isr.set_reset_status(false);
            }

            if remote_dma_command == 3 {
                let start = (self.state.bound_ptr as u16) << 8;
                self.state.remote_start = start;
                self.state.remote_dma = start;

                let size_addr = self.state.bound_ptr as u16 * 256 + 2;
                self.state.remote_bytes.set_byte(0, self.state.private_ram.read(size_addr));
                self.state
                    .remote_bytes
                    .set_byte(1, self.state.private_ram.read(size_addr + 1));

                info!(
                    "Starting private RAM read at 0x{:04X} of 0x{:X} bytes",
                    self.state.remote_start, self.state.remote_bytes
                );
            }

            if cr.transmit_packet() {
                if self.state.tcr.encoded_loopback_control().as_u8() != 0 {
                    if self.state.tcr.encoded_loopback_control().as_u8() != 1 {
                        warn!("Loop mode unsupported")
                    } else {
                        warn!("TODO: Some kind of loopback")
                    }
                } else {
                    if (self.state.control_reg.stop() || !self.state.control_reg.start()) && self.state.tx_bytes == 0 {
                        return Ok(());
                    }

                    self.state.control_reg.set_transmit_packet(true);

                    assert!(self.state.tx_bytes > 0);

                    let start_offset = (self.state.tx_page_start as u16) << 8;
                    let start_offset = if start_offset >= 48 << 10 {
                        (start_offset - 48) << 10
                    } else {
                        start_offset
                    };

                    let range = start_offset..start_offset + self.state.tx_bytes;
                    debug!("Sending bytes in range: {range:04X?}");
                    let data = self.state.private_ram[range].to_vec();
                    self.send_packet(data);
                }
            }

            if cr.remote_dma_command().as_u8() == 1 && cr.start() && self.state.remote_bytes == 0 {
                self.state.isr.set_remote_dma_complete(true);
                self.check_irq();
            }
        } else {
            let val = val.require_u8()?;
            trace!(
                "Write address {}:0x{addr:X} = {val:02X}",
                self.state.control_reg.page_select()
            );
            match self.state.control_reg.page_select().as_u8() {
                0 => match addr {
                    1 => self.state.page_start = val,
                    2 => self.state.page_stop = val,
                    3 => self.state.bound_ptr = val,
                    4 => self.state.tx_page_start = val,
                    5 => self.state.tx_bytes.set_byte(0, val),
                    6 => self.state.tx_bytes.set_byte(1, val),
                    7 => {
                        self.state.isr.value = self.state.isr.value & (!val) & 0x7f;
                        info!("Lowered interrupts: {:?}", self.state.isr);
                        self.check_irq();
                        // TODO: Lower interrupt pin if (isr & !imr) == 0
                    },
                    8 => {
                        self.state.remote_start.set_byte(0, val);
                        self.state.remote_dma = self.state.remote_start;
                    },
                    9 => {
                        self.state.remote_start.set_byte(1, val);
                        self.state.remote_dma = self.state.remote_start;
                    },
                    0xa => self.state.remote_bytes.set_byte(0, val),
                    0xb => self.state.remote_bytes.set_byte(1, val),
                    0xc => self.state.rcr = ReceiveConfigurationReg::from(val),
                    0xd => {
                        self.state.tcr = TransmitConfigurationReg::from(val);

                        assert!(!self.state.tcr.inhibit_crc());
                        assert!(!self.state.tcr.disable_auto_transmit());
                    },
                    0xe => self.state.dcr = DataConfigurationReg::from(val),
                    0xf => {
                        self.state.imr = InterruptStatusReg::from(val);
                        self.rx_irq_enabled.store(self.state.imr.packet_received(), Ordering::SeqCst);
                        self.check_irq();
                    },
                    _ => unreachable!(),
                },
                1 => match addr {
                    0x1..=0x6 => self.state.phys_addr[addr as usize - 1] = val,
                    0x7 => self.state.curr_page = val,
                    0x8..=0xf => self.state.multicast_hash[addr as usize - 8] = val,
                    _ => unreachable!(),
                },
                2 => match addr {
                    0x1 => self.state.local_dma.set_byte(0, val),
                    0x2 => self.state.local_dma.set_byte(1, val),
                    0x3 => self.state.remote_next_packet_pointer = val,
                    0x4 => unimplemented!(),
                    0x5 => self.state.local_packet_pointer = val,
                    0x6 => self.state.address_count.set_byte(0, val),
                    0x7 => self.state.address_count.set_byte(1, val),
                    0x8..=0xf => unimplemented!(),
                    _ => unreachable!(),
                },
                3 => error!("not implememented: page 3 writes"),
                _ => unreachable!("page_select is u2"),
            }
        }

        Ok(())
    }

    fn reset(&mut self) {
        self.state.control_reg = ControlRegister::from(0);
        self.state.control_reg.set_stop(true);
        self.state.control_reg.set_remote_dma_command(u3::new(4));
        self.state.isr = InterruptStatusReg::from(0);
        self.state.isr.set_reset_status(true);
        self.state.imr = InterruptStatusReg::from(0);
        self.state.dcr = DataConfigurationReg::from(0);
        self.state.dcr.set_dma_mode(DmaMode::Single32);
        self.state.tcr = TransmitConfigurationReg::from(0);
        self.state.tsr = TransmitStatusReg::from(0);
        self.state.rcr = ReceiveConfigurationReg::from(0);
        self.state.rsr = ReceiveStatusReg::from(0);
        self.state.local_dma = 0;
        self.state.page_start = 0;
        self.state.page_stop = 0;
        self.state.bound_ptr = 0;
        self.state.tx_page_start = 0;
        self.state.num_coll = 0;
        self.state.tx_bytes = 0;
        self.state.fifo = 0;
        self.state.remote_dma = 0;
        self.state.remote_start = 0;
        self.state.remote_bytes = 0;
        self.state.tallycnt = [0; _];
        self.state.phys_addr = [0; _];
        self.state.curr_page = 0;
        self.state.multicast_hash = [0; _];
        self.state.remote_next_packet_pointer = 0;
        self.state.local_packet_pointer = 0;
        self.state.address_count = 0;

        self.lower_irq();
    }

    fn raise_irq(&self) {
        self.irq.set(self.state.pci_header.interrupt_line, true);
    }

    fn lower_irq(&self) {
        self.irq.set(self.state.pci_header.interrupt_line, false);
    }

    fn check_irq(&self) {
        if self.state.isr.value & self.state.imr.value != 0 {
            info!("Raising IRQ");
            self.raise_irq();
        } else {
            self.lower_irq();
        }
    }

    fn send_packet(&mut self, data: Vec<u8>) {
        info!("Sending network bytes: {data:02X?}");

        self.nat.send_packet(&data);

        self.state.control_reg.set_transmit_packet(false);
        self.state.tsr.set_packed_transmitted(true);
        self.state.isr.set_packet_transmitted(true);
        self.check_irq();
    }

    fn check_recv_packet(&mut self) {
        if let Some(mut packet) = self.nat.recv_packet() {
            packet.pad_to_60();
            self.recv_frame(packet.data());
        }
    }

    fn recv_frame(&mut self, data: &[u8]) {
        if self.state.control_reg.stop()
            || self.state.page_start == 0
            || (self.state.dcr.disable_loopback() && self.state.tcr.encoded_loopback_control().as_u8() != 0)
        {
            trace!("Ignoring packet: not receiving");
            return
        }

        let pages = (data.len() + 4 + 4).div_ceil(256);
        let available = if self.state.curr_page < self.state.bound_ptr {
            self.state.bound_ptr as usize - self.state.curr_page as usize
        } else {
            (self.state.page_stop as usize - self.state.page_start as usize)
                - (self.state.curr_page as usize - self.state.bound_ptr as usize)
        };

        if available <= pages {
            error!("TODO: Buffer partial receives and receive the full packet later");
            return
        }

        let frame = EthernetFrame::new_checked(data).unwrap();
        if !self.state.rcr.enable_promiscuous_physical() {
            if !self.state.rcr.accept_broadcast() && frame.dst_addr().is_broadcast() {
                trace!("Ignoring broadcast packet");
                return
            }

            if frame.dst_addr().is_multicast() {
                if !self.state.rcr.accept_multicast() {
                    trace!("Ignoring multicast packet");
                    return
                }

                error!(
                    "TODO: properly handle multicast (compute mcast_index, check mchash, reject packets for which bit is not set)"
                )
            } else if frame.dst_addr() != EthernetAddress::from_bytes(&self.state.phys_addr) {
                return
            }
        }

        let next_page = self.state.curr_page + pages as u8;
        let next_page = if next_page >= self.state.page_stop {
            next_page - (self.state.page_stop - self.state.page_start)
        } else {
            next_page
        };

        let header = [
            1 | if frame.dst_addr().is_multicast() { 0x20 } else { 0x00 },
            next_page,
            (data.len() + 4).get_byte(0),
            (data.len() + 4).get_byte(1),
        ];

        let start_offset = self.state.curr_page as u16 * 256;
        trace!("Reading packet: copying into private RAM at 0x{start_offset:04X}: {data:02X?}");
        if next_page > self.state.curr_page {
            self.state.private_ram.copy_from_slice(start_offset, &header);
            self.state.private_ram.copy_from_slice(start_offset + 4, data);
        } else {
            let remaining = (self.state.page_stop - self.state.curr_page) as usize * 256;
            self.state.private_ram.copy_from_slice(start_offset, &header);
            self.state
                .private_ram
                .copy_from_slice(start_offset + 4, &data[..(remaining - 4).min(data.len())]);

            if data.len() > remaining - 4 {
                self.state
                    .private_ram
                    .copy_from_slice(self.state.page_start as u16 * 256, &data[remaining - 4..]);
            }
        }

        self.state.curr_page = next_page;
        self.state
            .rsr
            .set_address_match(if frame.dst_addr().is_broadcast() || frame.dst_addr().is_multicast() {
                AddressMatch::MulticastOrBroadcast
            } else {
                AddressMatch::Physical
            });
        self.state.rsr.set_packed_received_intact(true);
        self.state.isr.set_packet_received(true);
        self.check_irq();
    }

    pub fn snapshot(&self) -> Ne2kSnapshot {
        Ne2kSnapshot {
            state: self.state.clone(),
        }
    }

    pub fn restore(&mut self, snapshot: Ne2kSnapshot) {
        self.state = snapshot.state;
        self.irq_line.store(self.state.pci_header.interrupt_line, Ordering::SeqCst);
        self.rx_irq_enabled.store(self.state.imr.packet_received(), Ordering::SeqCst);
    }
}

struct BaseAddresses {
    io_base: u16,
}

impl WithIoSpace for Ne2k {
    fn try_read<S: PortIoData>(&mut self, addr: u16, _mmio: &mut crate::hw::HwMmio) -> Option<Result<S, PortError>> {
        if !self.state.pci_header.common.command.enable_io_space() {
            return None
        }

        let a = self.base_addresses();
        if addr & BAR_MASK as u16 == a.io_base {
            Some(self.read::<S>(addr & !BAR_MASK as u16))
        } else {
            None
        }
    }

    fn try_write<S: PortIoData>(&mut self, addr: u16, val: S, _mmio: &mut crate::hw::HwMmio) -> Option<Result<(), PortError>> {
        if !self.state.pci_header.common.command.enable_io_space() {
            return None
        }

        let a = self.base_addresses();
        if addr & BAR_MASK as u16 == a.io_base {
            Some(self.write(addr & !BAR_MASK as u16, val))
        } else {
            None
        }
    }
}

const BAR_MASK: u32 = !0x1f;
impl PciDevice for Ne2k {
    fn write_configuration_space(&mut self, index: usize, val: u32) {
        debug!("Write PCI register 0x{index:X} = 0x{val:X}");
        match self.state.pci_header.write(index, val) {
            Some(DeviceWriteEvent::Common(_)) => {
                self.irq_line.store(self.state.pci_header.interrupt_line, Ordering::SeqCst);
            },
            Some(DeviceWriteEvent::Bar(0)) => {
                self.state.pci_header.bar[0] = (self.state.pci_header.bar[0] & BAR_MASK) | 1;
                info!("BAR0 = {:X}", self.state.pci_header.bar[0]);
            },
            Some(DeviceWriteEvent::Bar(n)) => self.state.pci_header.bar[n] = 0,
            Some(DeviceWriteEvent::CardbusCisPointer) => (),
            Some(DeviceWriteEvent::ExpansionRom) => (),
            Some(DeviceWriteEvent::CapabilitiesPointer) => (),
            Some(DeviceWriteEvent::Reserved2) => (),
            Some(DeviceWriteEvent::InterruptConfig) => (),
            None => (),
        }
    }

    fn read_configuration_space(&mut self, index: usize) -> u32 {
        let result = self.state.pci_header.read(index).unwrap_or(0);
        debug!("Read PCI register 0x{index:X} = 0x{result:X}");
        result
    }
}
