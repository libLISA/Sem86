use std::fmt::{Debug, Display};
use std::ops::{Index, IndexMut};

use generativity::{Guard, Id};

type Inner = std::pat::pattern_type!(u32 is 0..0xffff_ffff);

#[derive(Copy, Clone)]
struct NonMaxU32(Inner);

impl NonMaxU32 {
    pub fn new(val: u32) -> Self {
        assert!(val < 0xffff_fffe);
        unsafe { Self(std::mem::transmute(val)) }
    }

    fn as_u32(&self) -> u32 {
        unsafe { std::mem::transmute(self.0) }
    }
}

impl std::hash::Hash for NonMaxU32 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_u32().hash(state)
    }
}

impl PartialEq for NonMaxU32 {
    fn eq(&self, other: &Self) -> bool {
        self.as_u32() == other.as_u32()
    }
}

impl Eq for NonMaxU32 {}

#[derive(Copy, Clone, Hash, PartialEq, Eq)]
pub struct ZocIndex<'tag>(NonMaxU32, Id<'tag>);

impl Debug for ZocIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0.as_u32(), f)
    }
}

impl Display for ZocIndex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0.as_u32(), f)
    }
}

impl ZocIndex<'_> {
    pub fn as_u32(&self) -> u32 {
        self.0.as_u32()
    }

    #[inline(always)]
    pub unsafe fn new_unchecked(index: u32) -> Self {
        unsafe { Self(NonMaxU32::new(index), Id::new()) }
    }
}

impl PartialEq<usize> for ZocIndex<'_> {
    fn eq(&self, other: &usize) -> bool {
        self.as_u32() as usize == *other
    }
}

impl PartialEq<ZocIndex<'_>> for usize {
    fn eq(&self, other: &ZocIndex<'_>) -> bool {
        *self == other.0.as_u32() as usize
    }
}

/// Zero-overhead collection.
/// Guarantees indices to be in-bounds at compile time, eliminating any runtime bounds checks.
pub struct Zoc<'tag, Item> {
    data: Vec<Item>,
    tag: Id<'tag>,
}

impl<'tag, Item> Zoc<'tag, Item> {
    pub fn new(tag: Guard<'tag>) -> Self {
        Self {
            data: Vec::new(),
            tag: tag.into(),
        }
    }

    pub fn push(&mut self, item: Item) -> ZocIndex<'tag> {
        let index = ZocIndex(
            NonMaxU32::new(self.data.len().try_into().expect("Zoc cannot contain more than 4B elements")),
            self.tag,
        );
        self.data.push(item);
        index
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Item> {
        self.data.iter()
    }

    pub fn iter_with_indices(&self) -> impl Iterator<Item = (ZocIndex<'tag>, &Item)> {
        self.data
            .iter()
            .enumerate()
            .map(|(index, item)| (ZocIndex(NonMaxU32::new(index as u32), self.tag), item))
    }
}

impl<'tag, Item> Index<ZocIndex<'tag>> for Zoc<'tag, Item> {
    type Output = Item;

    fn index(&self, index: ZocIndex<'tag>) -> &Self::Output {
        // SAFETY: Indices are only handed out for existing items.
        //         The vector can never shrink, so all indices will remain valid forever.
        //         The tag ensures that the index corresponds to this specific container.
        unsafe { self.data.get_unchecked(index.0.as_u32() as usize) }
    }
}

impl<'tag, Item> IndexMut<ZocIndex<'tag>> for Zoc<'tag, Item> {
    fn index_mut(&mut self, index: ZocIndex<'tag>) -> &mut Self::Output {
        // SAFETY: See `Index::index` above.
        unsafe { self.data.get_unchecked_mut(index.0.as_u32() as usize) }
    }
}
