use bilge::prelude::*;

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Pte {
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
    phys_base: u20,
}

impl Pte {
    pub fn phys_base_addr(&self) -> u32 {
        self.phys_base().as_u32() << 12
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct Pde {
    pub present: bool,

    /// When set to false, all of the PTEs below this PDE are read-only.
    pub writeable: bool,
    pub user_accessible: bool,
    pub pwt: bool,
    pub pcd: bool,
    pub accessed: bool,
    reserved: u1,

    // Page size = 4MiB when true, 4KiB when false.
    pub big_page: bool,

    reserved: u1,
    pub avl: u3,
    pt_base: u20,
}

impl Pde {
    pub fn pt_base_addr(&self) -> u32 {
        self.pt_base().as_u32() << 12
    }
}

#[bitsize(32)]
#[derive(Copy, Clone, DebugBits, FromBits)]
pub struct BigPde {
    pub present: bool,

    /// When set to false, all of the PTEs below this PDE are read-only.
    pub writeable: bool,
    pub user_accessible: bool,
    pub pwt: bool,
    pub pcd: bool,
    pub accessed: bool,
    pub dirty: bool,

    // Page size = 4MiB when true, 4KiB when false.
    pub big_page: bool,

    pub global: bool,
    pub avl: u3,
    pub pat: bool,
    pub high_bits: u8,
    reserved: u1,
    pub low_bits: u10,
}

impl BigPde {
    pub fn phys_base_addr(&self) -> u64 {
        (self.low_bits().as_u64() << 22) | (self.high_bits().as_u64() << 32)
    }
}
