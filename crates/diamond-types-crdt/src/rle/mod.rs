use std::fmt::Debug;

use rle::{HasLength, MergableSpan, Searchable, SplitableSpan, SplitableSpanHelpers};
pub use simple_rle::RleVec;

pub type RleKey = u32;

pub mod simple_rle;

pub trait RleKeyed {
    fn get_rle_key(&self) -> RleKey;
}

pub trait RleSpanHelpers: RleKeyed + HasLength {
    fn end(&self) -> u32 {
        self.get_rle_key() + self.len() as u32
    }
}

impl<V: RleKeyed + HasLength> RleSpanHelpers for V {}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KVPair<V>(pub RleKey, pub V);

impl<V> RleKeyed for KVPair<V> {
    fn get_rle_key(&self) -> u32 {
        self.0
    }
}

impl<V: HasLength> HasLength for KVPair<V> {
    fn len(&self) -> usize { self.1.len() }
}
impl<V: HasLength + SplitableSpan> SplitableSpanHelpers for KVPair<V> {
    fn truncate_h(&mut self, at: usize) -> Self {
        debug_assert!(at > 0);
        debug_assert!(at < self.1.len());

        let remainder = self.1.truncate(at);
        KVPair(self.0 + at as u32, remainder)
    }

    fn truncate_keeping_right_h(&mut self, at: usize) -> Self {
        let old_key = self.0;
        self.0 += at as u32;
        let trimmed = self.1.truncate_keeping_right(at);
        KVPair(old_key, trimmed)
    }
}
impl<V: HasLength + MergableSpan> MergableSpan for KVPair<V> {
    fn can_append(&self, other: &Self) -> bool {
        other.0 == self.end() && self.1.can_append(&other.1)
    }

    fn append(&mut self, other: Self) {
        self.1.append(other.1);
    }

    fn prepend(&mut self, other: Self) {
        self.1.prepend(other.1);
        self.0 = other.0;
    }
}

impl<V: Searchable> Searchable for KVPair<V> {
    type Item = V::Item;

    fn get_offset(&self, loc: Self::Item) -> Option<usize> { self.1.get_offset(loc) }
    fn at_offset(&self, offset: usize) -> Self::Item { self.1.at_offset(offset) }
}

impl<V: Default> Default for KVPair<V> {
    fn default() -> Self {
        KVPair(0, V::default())
    }
}

#[cfg(test)]
mod test {
    use rle::test_splitable_methods_valid;

    use crate::order::TimeSpan;
    use crate::rle::KVPair;

    #[test]
    fn kvpair_valid() {
        test_splitable_methods_valid(KVPair(10, TimeSpan {
            start: 10,
            len: 5
        }));
    }
}