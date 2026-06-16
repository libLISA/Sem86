use bilge::prelude::*;

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct PaeAddr32 {
    pub page_offset: u12,
    pub pte_offset: u9,
    pub pde_offset: u9,
    pub pdpe_offset: u2,
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct PaePdpe {
    pub present: bool,
    reserved: u2,
    pub pwt: bool,
    pub pcd: bool,
    reserved: u4,
    pub avl: u3,
    phys_base: u40,
    reserved: u12,
}

impl PaePdpe {
    pub fn pd_base_addr(&self) -> u64 {
        self.phys_base().as_u64() << 12
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct PaePde {
    pub present: bool,
    pub writeable: bool,
    pub user_accessible: bool,
    pub pwt: bool,
    pub pcd: bool,
    pub accessed: bool,
    reserved: bool,
    /// Page size = 2MiB when true, 4KiB when false.
    pub big_page: bool,
    reserved: bool,
    pub avl: u3,
    phys_base: u40,
    reserved: u12,
}

impl PaePde {
    pub fn pt_base_addr(&self) -> u64 {
        self.phys_base().as_u64() << 12
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct BigPaePde {
    pub present: bool,
    pub writeable: bool,
    pub user_accessible: bool,
    pub pwt: bool,
    pub pcd: bool,
    pub accessed: bool,
    pub dirty: bool,
    /// Page size = 2MiB when true, 4KiB when false.
    pub big_page: bool,
    pub global: bool,
    pub avl: u3,
    pub pat: bool,
    reserved: u8,
    phys_base: u31,
    reserved: u11,
    pub no_execute: bool,
}

impl BigPaePde {
    pub fn phys_base_addr(&self) -> u64 {
        self.phys_base().as_u64() << 21
    }
}

#[bitsize(64)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct PaePte {
    pub present: bool,
    pub writeable: bool,
    pub user_accessible: bool,
    pub pwt: bool,
    pub pcd: bool,
    pub accessed: bool,
    pub dirty: bool,
    pub page_attribute_table: bool,
    pub global: bool,
    pub avl: u3,
    phys_base: u40,
    reserved: u11,
    pub no_execute: bool,
}

impl PaePte {
    pub fn phys_base_addr(&self) -> u64 {
        self.phys_base().as_u64() << 12
    }
}
