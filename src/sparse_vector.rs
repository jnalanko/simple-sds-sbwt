//! An Elias-Fano encoded array supporting rank, select, and related queries.
//!
//! This structure is equivalent to `sd_vector` in [SDSL](https://github.com/simongog/sdsl-lite).
//! It is also known in the literature as sdarray:
//!
//! > Okanohara, Sadakane: Practical Entropy-Compressed Rank/Select Dictionary.  
//! > Proc. ALENEX 2007.  
//! > DOI: [10.1137/1.9781611972870.6](https://doi.org/10.1137/1.9781611972870.6)
//!
//! Assume that we have a bitvector of length `n` with `m` set bits, with `m` much smaller than `n`.
//! Let `w = log(n) - log(m)`.
//! In the integer array interpretation (see [`BitVec`]), we split each value into the low `w` bits and the high `log(m)` bits.
//! The low bits are stored explicitly in an [`IntVector`].
//! The high bits are encoded in unary in a [`BitVector`].
//! If there are `k >= 0` values with the same high part, the bitvector will contain `k` set bits followed by an unset bit.
//! Then
//!
//! > `select(i) = low[i] + ((high.select(i) - i) << w)`.
//!
//! Rank, predecessor, and successor queries use `select_zero` on `high` followed by a linear scan.

use crate::bit_vector::BitVector;
use crate::int_vector::IntVector;
use crate::ops::{Element, Access, BitVec, Rank, Select, PredSucc, SelectZero};
use crate::raw_vector::{RawVector, AccessRaw};
use crate::serialize::Serialize;
use crate::bits;

use std::convert::TryFrom;
use std::iter::{DoubleEndedIterator, ExactSizeIterator, FusedIterator, Extend};
use std::{cmp, io};

// FIXME tests in a separate module
//#[cfg(test)]
//mod tests;

//-----------------------------------------------------------------------------

// FIXME example
/// An immutable Elias-Fano encoded bitvector supporting, rank, select, and related queries.
///
/// This structure should be used for sparse bitvectors, where frequency of set bits is low.
/// For dense bitvectors or when [`SelectZero`] is needed, [`BitVector`] is a better choice.
/// Because most queries require support structures for one of the components, the bitvector itself is immutable.
/// Conversions between `SparseVector` and [`BitVector`] are possible using the [`From`] trait.
/// The maximum length of the vector is approximately `usize::MAX` bits.
///
/// `SparseVector` implements the following `simple_sds` traits:
/// * Basic functionality: [`BitVec`]
/// * Queries and operations: [`Rank`], [`Select`], [`PredSucc`]
/// * Serialization: [`Serialize`]
///
/// # Notes
///
/// * `SparseVector` never panics from I/O errors.
/// * [`Select::one_iter`] for `SparseVector` does not need select support.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SparseVector {
    len: usize,
    width: usize,
    high: BitVector,
    low: IntVector,
}

// Bitvector index encoded as offsets in `high` and `low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pos {
    high: usize,
    low: usize,
}

// Bitvector index encoded as low and high parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parts {
    high: usize,
    low: usize,
}

impl SparseVector {
    // Split a bitvector index into high and low parts.
    fn split(&self, index: usize) -> Parts {
        Parts {
            high: index >> self.width,
            low: index & unsafe { bits::low_set_unchecked(self.width) as usize },
        }
    }

    // Get (rank, bitvector index) from the offsets in `high` and `low`.
    fn combine(&self, pos: Pos) -> (usize, usize) {
        (pos.low, ((pos.high - pos.low) << self.width) + (self.low.get(pos.low) as usize))
    }

    // Get the offsets in `high` and `low` for the set bit of the given rank.
    fn pos(&self, rank: usize) -> Pos {
        Pos {
            high: self.high.select(rank).unwrap(),
            low: rank,
        }
    }

    // Get a `Pos` that points one past the unset bit preceding the values with this high part.
    fn lower_bound(&self, high_part: usize) -> Pos {
        if high_part == 0 {
            Pos { high: 0, low: 0, }
        } else {
            let high_offset = self.high.select_zero(high_part - 1).unwrap();
            Pos { high: high_offset, low: high_offset - high_part, }
        }
    }

    // Get a `Pos` that points to the unset bit after the values with the this high part.
    fn upper_bound(&self, high_part: usize) -> Pos {
        let high_offset = self.high.select_zero(high_part).unwrap();
        Pos { high: high_offset, low: high_offset - high_part, }
    }
}

//-----------------------------------------------------------------------------

// FIXME document, example, note that Extend may fail
#[derive(Clone, Debug)]
pub struct SparseBuilder {
    data: SparseVector,
    // We need a mutable bitvector during construction.
    high: RawVector,
    // Number of bits already set.
    len: usize,
    // The first index that can be set.
    next: usize,
}

impl SparseBuilder {
    /// Returns an empty SparseBuilder.
    ///
    /// Returns `None` if `ones > universe`.
    ///
    /// # Arguments
    ///
    /// * `universe`: Universe size or length of the bitvector.
    /// * `ones`: Number of bits that will be set in the bitvector.
    pub fn new(universe: usize, ones: usize) -> Option<SparseBuilder> {
        if ones > universe {
            return None;
        }

        let log_n = bits::bit_len(universe as u64);
        let mut log_m = bits::bit_len(ones as u64);
        if log_m == log_n {
            log_m -= 1;
        }
        let width: usize = log_n - log_m;
        let low = IntVector::with_len(ones, width, 0).unwrap();
        let data = SparseVector {
            len: universe,
            width: width,
            high: BitVector::from(RawVector::new()),
            low: low,
        };

        let high = RawVector::with_len(ones + (1usize << log_m), false);
        Some(SparseBuilder {
            data: data,
            high: high,
            len: 0,
            next: 0,
        })
    }

    /// Returns the number of bits that have already been set.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the number of bits that can be set.
    pub fn capacity(&self) -> usize {
        self.data.count_ones()
    }

    /// Returns the universe size or the length of the bitvector.
    pub fn universe(&self) -> usize {
        self.data.len()
    }

    // FIXME name
    /// Returns the smallest index in the bitvector that can still be set.
    pub fn next(&self) -> usize {
        self.next
    }

    /// Returns `true` if all bits that can be set have been set.
    pub fn full(&self) -> bool {
        self.len() == self.capacity()
    }

    /// Sets the specified bit in the bitvector.
    ///
    /// # Panics
    ///
    /// Panics if the builder is full, if `index < self.next()`, or if `index >= self.universe()`.
    pub fn set(&mut self, index: usize) {
        self.try_set(index).unwrap();
    }

    /// Unsafe version of [`SparseBuilder::set`] without sanity checks.
    ///
    /// Behavior is undefined if the builder is full, if `index < self.next()`, or if `index >= self.universe()`.
    pub unsafe fn set_unchecked(&mut self, index: usize) {
        let parts = self.data.split(index);
        self.high.set_bit(parts.high + self.len, true);
        self.data.low.set(self.len, parts.low as u64);
        self.len += 1; self.next = index + 1;
    }

    /// Tries to set the specified bit in the bitvector.
    ///
    /// Returns an error if the builder is full, if `index < self.next()`, or if `index >= self.universe()`.
    pub fn try_set(&mut self, index: usize) -> Result<(), String> {
        if self.full() {
            return Err("The builder is full".to_string());
        }
        if index < self.next() {
            return Err(format!("Cannot set bit {}; the lowest possible index is {}", index, self.next()));
        }
        if index >= self.universe() {
            return Err(format!("Cannot set bit {}; universe size is {}", index, self.universe()));
        }
        unsafe { self.set_unchecked(index); }
        Ok(())
    }
}

impl Extend<usize> for SparseBuilder {
    fn extend<T: IntoIterator<Item = usize>>(&mut self, iter: T) {
        for index in iter {
            self.set(index);
        }
    }
}

//-----------------------------------------------------------------------------

// FIXME example
/// A read-only iterator over [`SparseVector`].
///
/// The type of `Item` is `bool`.
#[derive(Clone, Debug)]
pub struct Iter<'a> {
    parent: OneIter<'a>,
    // The first index we have not visited.
    next: usize,
    // The first set bit we have not visited.
    next_set: Option<usize>,
    // The first index we should not visit.
    limit: usize,
    // The last set bit we have not visited.
    last_set: Option<usize>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = bool;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.limit {
            return None;
        }
        match self.next_set {
            Some(value) => {
                if value == self.next {
                    self.next_set = if let Some((_, index)) = self.parent.next() {
                        Some(index)
                    } else {
                        // If `next_set == last_set` already, we cannot reach the same index again.
                        self.last_set
                    };
                    self.next += 1;
                    Some(true)
                } else {
                    self.next += 1;
                    Some(false)
                }
            },
            None => {
                self.next += 1;
                Some(false)
            },
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.limit - self.next;
        (remaining, Some(remaining))
    }
}

impl<'a> DoubleEndedIterator for Iter<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.next >= self.limit {
            return None;
        }
        self.limit -= 1;
        match self.last_set {
            Some(value) => {
                if value == self.limit {
                    self.last_set = if let Some((_, index)) = self.parent.next_back() {
                        Some(index)
                    } else {
                        // If `next_set == last_set` already, we cannot reach the same index again.
                        self.next_set
                    };
                    Some(true)
                } else {
                    Some(false)
                }
            },
            None => Some(false),
        }
    }
}

impl<'a> ExactSizeIterator for Iter<'a> {}

impl<'a> FusedIterator for Iter<'a> {}

//-----------------------------------------------------------------------------

impl<'a> BitVec<'a> for SparseVector {
    type Iter = Iter<'a>;

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn count_ones(&self) -> usize {
        self.low.len()
    }

    fn get(&self, index: usize) -> bool {
        // Find the first value with the same high part, if it exists.
        let parts = self.split(index);
        let mut pos = self.lower_bound(parts.high);

        // Iterate forward over the values with the same high part until we find
        // a value no less than `value` or we run out of such values.
        while pos.high < self.high.len() && self.high.get(pos.high) {
            let low = self.low.get(pos.low) as usize;
            if low >= parts.low {
                return low == parts.low;
            }
            pos.high += 1; pos.low += 1;
        }

        return false;
    }

    fn iter(&'a self) -> Self::Iter {
        let mut one_iter = self.one_iter();
        let next_set = if let Some((_, index)) = one_iter.next() {
            Some(index)
        } else {
            None
        };
        let last_set = if let Some((_, index)) = one_iter.next_back() {
            Some(index)
        } else {
            next_set
        };
        Self::Iter {
            parent: one_iter,
            next: 0,
            next_set: next_set,
            limit: self.len(),
            last_set: last_set,
        }
    }
}

//-----------------------------------------------------------------------------

impl<'a> Rank<'a> for SparseVector {
    fn supports_rank(&self) -> bool {
        true
    }

    fn enable_rank(&mut self) {}

    fn rank(&self, index: usize) -> usize {
        if index >= self.len() {
            return self.count_ones();
        }

        // Find the last value with the same high part, if it exists.
        let parts = self.split(index);
        let mut pos = self.upper_bound(parts.high);
        if pos.low == 0 {
            return 0;
        }
        pos.high -= 1; pos.low -= 1;

        // Iterate backward over the values with the same high part until we find
        // as value lower than `index` or we run out of such values.
        while self.high.get(pos.high) && (self.low.get(pos.low) as usize) >= parts.low {
            if pos.low == 0 {
                return 0;
            }
            pos.high -= 1; pos.low -= 1;
        }

        pos.low + 1
    }
}

//-----------------------------------------------------------------------------

// FIXME example
/// An iterator over the set bits in [`SparseVector`].
///
/// The type of `Item` is `(usize, usize)`.
/// This can be interpreted as:
///
/// * `(index, value)` or `(i, select(i))` in the integer array; or
/// * `(rank(j), j)` in the bit array with `j` such that `self.get(j) == true`.
///
/// Note that `index` is not always the index provided by [`Iterator::enumerate`].
/// Queries may create iterators in the middle of the bitvector.
#[derive(Clone, Debug)]
pub struct OneIter<'a> {
    parent: &'a SparseVector,
    // The first position we have not visited.
    next: Pos,
    // The first position we should not visit.
    limit: Pos,
}

impl<'a> OneIter<'a> {
    // Build an empty iterator for the parent bitvector.
    fn empty_iter(parent: &'a SparseVector) -> OneIter<'a> {
        OneIter {
            parent: parent,
            next: Pos { high: parent.high.len(), low: parent.low.len(), },
            limit: Pos { high: parent.high.len(), low: parent.low.len(), },
        }
    }
}

impl<'a> Iterator for OneIter<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.next.low >= self.limit.low {
            None
        } else {
            while !self.parent.high.get(self.next.high) {
                self.next.high += 1;
            }
            let result = self.parent.combine(self.next);
            self.next.high += 1; self.next.low += 1;
            Some(result)
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.limit.low - self.next.low;
        (remaining, Some(remaining))
    }
}

impl<'a> DoubleEndedIterator for OneIter<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.next.low >= self.limit.low {
            None
        } else {
            self.limit.high -= 1; self.limit.low -= 1;
            while !self.parent.high.get(self.limit.high) {
                self.limit.high -= 1;
            }
            Some(self.parent.combine(self.limit))
        }
    }
}

impl<'a> ExactSizeIterator for OneIter<'a> {}

impl<'a> FusedIterator for OneIter<'a> {}

//-----------------------------------------------------------------------------

impl<'a> Select<'a> for SparseVector {
    type OneIter = OneIter<'a>;

    fn supports_select(&self) -> bool {
        true
    }

    fn enable_select(&mut self) {}

    fn one_iter(&'a self) -> Self::OneIter {
        Self::OneIter {
            parent: self,
            next: Pos { high: 0, low: 0, },
            limit: Pos { high: self.high.len(), low: self.low.len(), },
        }
    }

    fn select(&'a self, rank: usize) -> Option<usize> {
         if rank >= self.count_ones() {
             None
        } else {
            Some(self.combine(self.pos(rank)).1)
        }
    }

    fn select_iter(&'a self, rank: usize) -> Self::OneIter {
         if rank >= self.count_ones() {
             Self::OneIter::empty_iter(self)
        } else {
            Self::OneIter {
                parent: self,
                next: self.pos(rank),
                limit: Pos { high: self.high.len(), low: self.low.len(), },
            }
        }
    }
}

//-----------------------------------------------------------------------------

impl<'a> PredSucc<'a> for SparseVector {
    type OneIter = OneIter<'a>;

    fn supports_pred_succ(&self) -> bool {
        true
    }

    fn enable_pred_succ(&mut self) {}

    fn predecessor(&'a self, value: usize) -> Self::OneIter {
        if self.is_empty() {
            return Self::OneIter::empty_iter(self);
        }

        // Find the last value with the same high part, if it exists.
        let parts = self.split(cmp::min(value, self.len() - 1));
        let mut pos = self.upper_bound(parts.high);
        if pos.low == 0 {
            return Self::OneIter::empty_iter(self);
        }
        pos.high -= 1; pos.low -= 1;

        // Iterate backward over the values with the same high part until we find
        // a value no greater than `value` or we run out of such values.
        while self.high.get(pos.high) && (self.low.get(pos.low) as usize) > parts.low {
            if pos.low == 0 {
                return Self::OneIter::empty_iter(self);
            }
            pos.high -= 1; pos.low -= 1;
        }

        // The predecessor has a lower high part, so we continue iterating until we find it.
        while !self.high.get(pos.high) {
            pos.high -= 1;
        }

        Self::OneIter {
            parent: self,
            next: pos,
            limit: Pos { high: self.high.len(), low: self.low.len(), },
        }
    }

    fn successor(&'a self, value: usize) -> Self::OneIter {
        if self.is_empty() {
            return Self::OneIter::empty_iter(self);
        }

        // Find the first value with the same high part, if it exists.
        let parts = self.split(cmp::min(value, self.len() - 1));
        let mut pos = self.lower_bound(parts.high);

        // Iterate forward over the values with the same high part until we find
        // a value no less than `value` or we run out of such values.
        while pos.high < self.high.len() && self.high.get(pos.high) {
            if (self.low.get(pos.low) as usize) >= parts.low {
                return Self::OneIter {
                    parent: self,
                    next: pos,
                    limit: Pos { high: self.high.len(), low: self.low.len(), },
                };
            }
            pos.high += 1; pos.low += 1;
        }

        // The successor has a greater high part, so we continue iterating until we find it.
        while pos.high < self.high.len() {
            if self.high.get(pos.high) {
                return Self::OneIter {
                    parent: self,
                    next: pos,
                    limit: Pos { high: self.high.len(), low: self.low.len(), },
                };
            }
            pos.high += 1;
        }

        Self::OneIter::empty_iter(self)
    }
}

//-----------------------------------------------------------------------------

impl Serialize for SparseVector {
    fn serialize_header<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.len.serialize(writer)?;
        self.width.serialize(writer)?;
        Ok(())
    }

    fn serialize_body<T: io::Write>(&self, writer: &mut T) -> io::Result<()> {
        self.high.serialize(writer)?;
        self.low.serialize(writer)?;
        Ok(())
    }

    fn load<T: io::Read>(reader: &mut T) -> io::Result<Self> {
        let len = usize::load(reader)?;
        let width = usize::load(reader)?;
        let high = BitVector::load(reader)?;
        let low = IntVector::load(reader)?;
        let result = SparseVector {
            len: len,
            width: width,
            high: high,
            low: low,
        };
        Ok(result)
    }

    fn size_in_bytes(&self) -> usize {
        self.len.size_in_bytes() +
        self.width.size_in_bytes() +
        self.high.size_in_bytes() +
        self.low.size_in_bytes()
    }
}

//-----------------------------------------------------------------------------

impl From<BitVector> for SparseVector {
    fn from(value: BitVector) -> Self {
        let mut builder = SparseBuilder::new(value.len(), value.count_ones()).unwrap();
        for (_, index) in value.one_iter() {
            unsafe { builder.set_unchecked(index); }
        }
        SparseVector::try_from(builder).unwrap()
    }
}

impl From<SparseVector> for BitVector {
    fn from(value: SparseVector) -> Self {
        let mut data = RawVector::with_len(value.len(), false);
        for (_, index) in value.one_iter() {
            data.set_bit(index, true);
        }
        BitVector::from(data)
    }
}

impl TryFrom<SparseBuilder> for SparseVector {
    type Error = &'static str;

    fn try_from(value: SparseBuilder) -> Result<Self, Self::Error> {
        let mut value = value;
        if !value.full() {
            return Err("The builder is not full");
        }
        value.data.high = BitVector::from(value.high);
        value.data.high.enable_select();
        value.data.high.enable_select_zero();
        Ok(value.data)
    }
}

//-----------------------------------------------------------------------------