use std::ops::{Deref, DerefMut};

use bitcode::{Decode, Encode};
use serde::{Deserialize, Serialize};

pub trait BankData {
    type Item;

    fn new() -> Self;
    fn get(&self, index: u8) -> Self::Item;
    fn set(&mut self, index: u8, val: Self::Item);
}

#[macro_export]
macro_rules! impl_bank {
    ($(#[$attr:meta])* $vis:vis struct $struct:ident -> $item_ty:ty { $($field_vis:vis $name:ident ($n:literal): $ty:ty),* $(,)* }) => {
        $(#[$attr])*
        $vis struct $struct {
            $($field_vis $name: $ty,)*
        }

        impl $crate::hw::bank::BankData for $struct {
            type Item = $item_ty;

            fn new() -> Self {
                Self {
                    $($name: <$ty>::default(),)*
                }
            }

            fn get(&self, index: u8) -> Self::Item {
                match index {
                    $($n => {
                        ::log::trace!("Read register: {}.{} ({}) = {:#X?}", stringify!($struct), stringify!($name), $n, self.$name);

                        self.$name.try_into().unwrap()
                    },)*
                    n => {
                        ::log::error!("Tried to read missing register: {}.{}", stringify!($struct), n);

                        0xff
                    },
                }
            }

            fn set(&mut self, index: u8, val: Self::Item) {
                match index {
                    $($n => {
                        self.$name = <$ty>::try_from(val).unwrap();
                        ::log::trace!("Write register: {}.{} ({}) = {:#X?}", stringify!($struct), stringify!($name), $n, self.$name);
                    },)*
                    n => ::log::error!("Tried to write missing register: {}.{}", stringify!($struct), n),
                }
            }
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct RegisterBank<T, const ADDR_BITS: u8> {
    regs: T,
    current_index: u8,
}

impl<T: BankData, const ADDR_BITS: u8> RegisterBank<T, ADDR_BITS> {
    pub fn new() -> Self {
        Self {
            regs: T::new(),
            current_index: 0,
        }
    }

    pub fn read_addr(&self) -> u8 {
        self.current_index
    }

    pub fn write_addr(&mut self, addr: u8) {
        self.current_index = addr;
    }

    fn addr_mask(&self) -> u8 {
        !(u16::MAX << ADDR_BITS) as u8
    }

    pub fn read(&self) -> T::Item {
        self.regs.get(self.current_index & self.addr_mask())
    }

    pub fn write(&mut self, val: T::Item) {
        self.regs.set(self.current_index & self.addr_mask(), val)
    }

    pub fn current_addr(&self) -> u8 {
        self.current_index
    }
}

impl<T: BankData, const ADDR_BITS: u8> Default for RegisterBank<T, ADDR_BITS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: BankData, const ADDR_BITS: u8> Deref for RegisterBank<T, ADDR_BITS> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.regs
    }
}

impl<T: BankData, const ADDR_BITS: u8> DerefMut for RegisterBank<T, ADDR_BITS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.regs
    }
}
