// Copyright 2014-2016 bluss and ndarray developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use std::cmp::Ordering;
use std::fmt::Debug;
use std::slice;
use itertools::{enumerate, zip};

use super::{Si, Ix, Ixs};
use super::{zipsl, zipsl_mut};
use error::{from_kind, ErrorKind, ShapeError};
use ZipExt;
use {Ix0, Ix1, Ix2, Ix3, Ix4, Ix5, IxDyn};
use {ArrayView1, ArrayViewMut1};

/// Calculate offset from `Ix` stride converting sign properly
#[inline(always)]
pub fn stride_offset(n: Ix, stride: Ix) -> isize {
    (n as isize) * ((stride as Ixs) as isize)
}

/// Check whether the given `dim` and `stride` lead to overlapping indices
///
/// There is overlap if, when iterating through the dimensions in the order
/// of maximum variation, the current stride is inferior to the sum of all
/// preceding strides multiplied by their corresponding dimensions.
///
/// The current implementation assumes strides to be positive
pub fn dim_stride_overlap<D: Dimension>(dim: &D, strides: &D) -> bool {
    let order = strides._fastest_varying_stride_order();

    let dim = dim.slice();
    let strides = strides.slice();
    let mut prev_offset = 1;
    for &index in order.slice() {
        let d = dim[index];
        let s = strides[index];
        // any stride is ok if dimension is 1
        if d != 1 && (s as isize) < prev_offset {
            return true;
        }
        prev_offset = stride_offset(d, s);
    }
    false
}

/// Check whether the given dimension and strides are memory safe
/// to index the provided slice.
///
/// To be safe, no stride may be negative, and the offset corresponding
/// to the last element of each dimension should be smaller than the length
/// of the slice. Also, the strides should not allow a same element to be
/// referenced by two different index.
pub fn can_index_slice<A, D: Dimension>(data: &[A], dim: &D, strides: &D)
    -> Result<(), ShapeError>
{
    // check lengths of axes.
    let len = match dim.size_checked() {
        Some(l) => l,
        None => return Err(from_kind(ErrorKind::OutOfBounds)),
    };
    // check if strides are strictly positive (zero ok for len 0)
    for &s in strides.slice() {
        let s = s as Ixs;
        if s < 1 && (len != 0 || s < 0) {
            return Err(from_kind(ErrorKind::Unsupported));
        }
    }
    if len == 0 {
        return Ok(());
    }
    // check that the maximum index is in bounds
    let mut last_index = dim.clone();
    for mut index in last_index.slice_mut().iter_mut() {
        *index -= 1;
    }
    if let Some(offset) = stride_offset_checked_arithmetic(dim,
                                                           strides,
                                                           &last_index)
    {
        // offset is guaranteed to be positive so no issue converting
        // to usize here
        if (offset as usize) >= data.len() {
            return Err(from_kind(ErrorKind::OutOfBounds));
        }
        if dim_stride_overlap(dim, strides) {
            return Err(from_kind(ErrorKind::Unsupported));
        }
    } else {
        return Err(from_kind(ErrorKind::OutOfBounds));
    }
    Ok(())
}

/// Return stride offset for this dimension and index.
///
/// Return None if the indices are out of bounds, or the calculation would wrap
/// around.
fn stride_offset_checked_arithmetic<D>(dim: &D, strides: &D, index: &D)
    -> Option<isize>
    where D: Dimension
{
    let mut offset = 0;
    for (&d, &i, &s) in zipsl(dim.slice(), index.slice()).zip_cons(strides.slice()) {
        if i >= d {
            return None;
        }

        if let Some(offset_) = (i as isize)
                                   .checked_mul((s as Ixs) as isize)
                                   .and_then(|x| x.checked_add(offset)) {
            offset = offset_;
        } else {
            return None;
        }
    }
    Some(offset)
}

/// Array shape and index trait.
///
/// `unsafe` because of the assumptions in the default methods.
///
/// ***Don't implement or call methods in this trait, its interface is internal
/// to the crate and will evolve at will.***
pub unsafe trait Dimension : Clone + Eq + Debug + Send + Sync + Default {
    /// `SliceArg` is the type which is used to specify slicing for this
    /// dimension.
    ///
    /// For the fixed size dimensions it is a fixed size array of the correct
    /// size, which you pass by reference. For the `Vec` dimension it is
    /// a slice.
    ///
    /// - For `Ix1`: `[Si; 1]`
    /// - For `Ix2`: `[Si; 2]`
    /// - and so on..
    /// - For `Vec<Ix>`: `[Si]`
    ///
    /// The easiest way to create a `&SliceArg` is using the macro
    /// [`s![]`](macro.s!.html).
    type SliceArg: ?Sized + AsRef<[Si]>;
    /// Pattern matching friendly form of the dimension value.
    ///
    /// Usually a tuple.
    type Pattern: IntoDimension<Dim=Self>;
    #[doc(hidden)]
    fn ndim(&self) -> usize;
    #[doc(hidden)]
    fn slice(&self) -> &[Ix] {
        unsafe {
            slice::from_raw_parts(self as *const _ as *const Ix, self.ndim())
        }
    }

    fn equal(&self, rhs: &Self) -> bool {
        self.slice() == rhs.slice()
    }

    fn into_pattern(self) -> Self::Pattern;

    #[doc(hidden)]
    fn slice_mut(&mut self) -> &mut [Ix] {
        unsafe {
            slice::from_raw_parts_mut(self as *mut _ as *mut Ix, self.ndim())
        }
    }

    fn array_view(&self) -> ArrayView1<Ix> {
        ArrayView1::from(self.slice())
    }
    fn array_view_mut(&mut self) -> ArrayViewMut1<Ix> {
        ArrayViewMut1::from(self.slice_mut())
    }

    #[doc(hidden)]
    fn size(&self) -> usize {
        self.slice().iter().fold(1, |s, &a| s * a as usize)
    }

    #[doc(hidden)]
    /// Compute the size while checking for overflow
    fn size_checked(&self) -> Option<usize> {
        self.slice().iter().fold(Some(1), |s, &a| s.and_then(|s_| s_.checked_mul(a)))
    }

    #[doc(hidden)]
    fn default_strides(&self) -> Self {
        // Compute default array strides
        // Shape (a, b, c) => Give strides (b * c, c, 1)
        let mut strides = self.clone();
        {
            let mut it = strides.slice_mut().iter_mut().rev();
            // Set first element to 1
            for rs in it.by_ref() {
                *rs = 1;
                break;
            }
            let mut cum_prod = 1;
            for (rs, dim) in it.zip(self.slice().iter().rev()) {
                cum_prod *= *dim;
                *rs = cum_prod;
            }
        }
        strides
    }

    #[doc(hidden)]
    fn fortran_strides(&self) -> Self {
        // Compute fortran array strides
        // Shape (a, b, c) => Give strides (1, a, a * b)
        let mut strides = self.clone();
        {
            let mut it = strides.slice_mut().iter_mut();
            // Set first element to 1
            for rs in it.by_ref() {
                *rs = 1;
                break;
            }
            let mut cum_prod = 1;
            for (rs, dim) in it.zip(self.slice().iter()) {
                cum_prod *= *dim;
                *rs = cum_prod;
            }
        }
        strides
    }

    #[doc(hidden)]
    #[inline]
    fn first_index(&self) -> Option<Self> {
        for ax in self.slice().iter() {
            if *ax == 0 {
                return None;
            }
        }
        let mut index = self.clone();
        for rr in index.slice_mut().iter_mut() {
            *rr = 0;
        }
        Some(index)
    }

    #[doc(hidden)]
    /// Iteration -- Use self as size, and return next index after `index`
    /// or None if there are no more.
    // FIXME: use &Self for index or even &mut?
    #[inline]
    fn next_for(&self, index: Self) -> Option<Self> {
        let mut index = index;
        let mut done = false;
        for (&dim, ix) in zip(self.slice(), index.slice_mut()).rev() {
            *ix += 1;
            if *ix == dim {
                *ix = 0;
            } else {
                done = true;
                break;
            }
        }
        if done {
            Some(index)
        } else {
            None
        }
    }

    #[doc(hidden)]
    /// Return stride offset for index.
    fn stride_offset(index: &Self, strides: &Self) -> isize {
        let mut offset = 0;
        for (&i, &s) in zipsl(index.slice(), strides.slice()) {
            offset += stride_offset(i, s);
        }
        offset
    }

    #[doc(hidden)]
    /// Return stride offset for this dimension and index.
    fn stride_offset_checked(&self, strides: &Self, index: &Self) -> Option<isize> {
        let mut offset = 0;
        for (&d, &i, &s) in zipsl(self.slice(), index.slice()).zip_cons(strides.slice())
        {
            if i >= d {
                return None;
            }
            offset += stride_offset(i, s);
        }
        Some(offset)
    }

    #[doc(hidden)]
    fn last_elem(&self) -> usize {
        if self.ndim() == 0 { 0 } else { self.slice()[self.ndim() - 1] }
    }

    #[doc(hidden)]
    fn set_last_elem(&mut self, i: usize) {
        let nd = self.ndim();
        self.slice_mut()[nd - 1] = i;
    }

    #[doc(hidden)]
    /// Modify dimension, strides and return data pointer offset
    ///
    /// **Panics** if `slices` does not correspond to the number of axes,
    /// if any stride is 0, or if any index is out of bounds.
    fn do_slices(dim: &mut Self, strides: &mut Self, slices: &Self::SliceArg) -> isize {
        let slices = slices.as_ref();
        let mut offset = 0;
        assert!(slices.len() == dim.slice().len());
        for (dr, sr, &slc) in zipsl_mut(dim.slice_mut(), strides.slice_mut()).zip_cons(slices)
        {
            let m = *dr;
            let mi = m as Ixs;
            let Si(b1, opt_e1, s1) = slc;
            let e1 = opt_e1.unwrap_or(mi);

            let b1 = abs_index(mi, b1);
            let mut e1 = abs_index(mi, e1);
            if e1 < b1 { e1 = b1; }

            assert!(b1 <= m);
            assert!(e1 <= m);

            let m = e1 - b1;
            // stride
            let s = (*sr) as Ixs;

            // Data pointer offset
            offset += stride_offset(b1, *sr);
            // Adjust for strides
            assert!(s1 != 0);
            // How to implement negative strides:
            //
            // Increase start pointer by
            // old stride * (old dim - 1)
            // to put the pointer completely in the other end
            if s1 < 0 {
                offset += stride_offset(m - 1, *sr);
            }

            let s_prim = s * s1;

            let d = m / s1.abs() as Ix;
            let r = m % s1.abs() as Ix;
            let m_prim = d + if r > 0 { 1 } else { 0 };

            // Update dimension and stride coordinate
            *dr = m_prim;
            *sr = s_prim as Ix;
        }
        offset
    }

    #[doc(hidden)]
    fn is_contiguous(dim: &Self, strides: &Self) -> bool {
        let defaults = dim.default_strides();
        if strides.equal(&defaults) {
            return true;
        }
        if dim.ndim() == 1 { return false; }
        let order = strides._fastest_varying_stride_order();
        let strides = strides.slice();

        // FIXME: Negative strides
        let dim_slice = dim.slice();
        let mut cstride = 1;
        for &i in order.slice() {
            // a dimension of length 1 can have unequal strides
            if dim_slice[i] != 1 && strides[i] != cstride {
                return false;
            }
            cstride *= dim_slice[i];
        }
        true
    }

    /// Return the axis ordering corresponding to the fastest variation
    /// (in ascending order).
    ///
    /// Assumes that no stride value appears twice. This cannot yield the correct
    /// result the strides are not positive.
    #[doc(hidden)]
    fn _fastest_varying_stride_order(&self) -> Self {
        let mut indices = self.clone();
        for (i, elt) in enumerate(indices.slice_mut()) {
            *elt = i;
        }
        let strides = self.slice();
        indices.slice_mut().sort_by_key(|&i| strides[i]);
        indices
    }
}

/// Implementation-specific extensions to `Dimension`
pub trait DimensionExt {
// note: many extensions go in the main trait if they need to be special-
// cased per dimension
    /// Get the dimension at `axis`.
    ///
    /// *Panics* if `axis` is out of bounds.
    #[inline]
    fn axis(&self, axis: Axis) -> Ix;

    /// Set the dimension at `axis`.
    ///
    /// *Panics* if `axis` is out of bounds.
    #[inline]
    fn set_axis(&mut self, axis: Axis, value: Ix);
}

impl<D> DimensionExt for D
    where D: Dimension
{
    #[inline]
    fn axis(&self, axis: Axis) -> Ix {
        self.slice()[axis.axis()]
    }

    #[inline]
    fn set_axis(&mut self, axis: Axis, value: Ix) {
        self.slice_mut()[axis.axis()] = value;
    }
}

impl<'a> DimensionExt for [Ix]
{
    #[inline]
    fn axis(&self, axis: Axis) -> Ix {
        self[axis.axis()]
    }

    #[inline]
    fn set_axis(&mut self, axis: Axis, value: Ix) {
        self[axis.axis()] = value;
    }
}

#[inline]
fn abs_index(len: Ixs, index: Ixs) -> Ix {
    if index < 0 {
        (len + index) as Ix
    } else {
        index as Ix
    }
}

/// Collapse axis `axis` and shift so that only subarray `index` is
/// available.
///
/// **Panics** if `index` is larger than the size of the axis
// FIXME: Move to Dimension trait
pub fn do_sub<A, D: Dimension>(dims: &mut D, ptr: &mut *mut A, strides: &D,
                               axis: usize, index: Ix) {
    let dim = dims.slice()[axis];
    let stride = strides.slice()[axis];
    assert!(index < dim);
    dims.slice_mut()[axis] = 1;
    let off = stride_offset(index, stride);
    unsafe {
        *ptr = ptr.offset(off);
    }
}

// Tuple to array conversion

/// $m: macro callback
/// $m is called with $arg and then the indices corresponding to the size argument
macro_rules! index {
    ($m:ident $arg:tt 0) => ($m!($arg));
    ($m:ident $arg:tt 1) => ($m!($arg 0));
    ($m:ident $arg:tt 2) => ($m!($arg 0 1));
    ($m:ident $arg:tt 3) => ($m!($arg 0 1 2));
    ($m:ident $arg:tt 4) => ($m!($arg 0 1 2 3));
    ($m:ident $arg:tt 5) => ($m!($arg 0 1 2 3 4));
    ($m:ident $arg:tt 6) => ($m!($arg 0 1 2 3 4 5));
}

macro_rules! index_item {
    ($m:ident $arg:tt 0) => ();
    ($m:ident $arg:tt 1) => ($m!($arg 0););
    ($m:ident $arg:tt 2) => ($m!($arg 0 1););
    ($m:ident $arg:tt 3) => ($m!($arg 0 1 2););
    ($m:ident $arg:tt 4) => ($m!($arg 0 1 2 3););
    ($m:ident $arg:tt 5) => ($m!($arg 0 1 2 3 4););
    ($m:ident $arg:tt 6) => ($m!($arg 0 1 2 3 4 5););
}

pub trait IntoDimension {
    type Dim: Dimension;
    fn into_dimension(self) -> Self::Dim;
}

impl IntoDimension for Ix {
    type Dim = Ix1;
    #[inline(always)]
    fn into_dimension(self) -> Ix1 { Ix1(self) }
}

impl<D> IntoDimension for D where D: Dimension {
    type Dim = D;
    #[inline(always)]
    fn into_dimension(self) -> Self { self }
}

impl IntoDimension for Vec<usize> {
    type Dim = IxDyn;
    #[inline(always)]
    fn into_dimension(self) -> Self::Dim { Dim::new(self) }
}

trait Convert {
    type To;
    fn convert(self) -> Self::To;
}

impl Convert for Ix {
    type To = Ix1;
    fn convert(self) -> Self::To { Ix1(self) }
}
/*
*/

macro_rules! sub {
    ($_x:tt $y:tt) => ($y);
}

macro_rules! tuple_type {
    ([$T:ident] $($index:tt)*) => (
        ( $(sub!($index $T), )* )
    )
}

macro_rules! tuple_expr {
    ([$self_:expr] $($index:tt)*) => (
        ( $($self_[$index], )* )
    )
}

macro_rules! array_expr {
    ([$self_:expr] $($index:tt)*) => (
        [$($self_ . $index, )*]
    )
}

macro_rules! tuple_to_array {
    ([] $($n:tt)*) => {
        $(
        impl Convert for [Ix; $n] {
            type To = index!(tuple_type [Ix] $n);
            fn convert(self) -> Self::To {
                index!(tuple_expr [self] $n)
            }
        }
        
        impl Convert for index!(tuple_type [Ix] $n) {
            type To = Dim<[Ix; $n]>;
            fn convert(self) -> Self::To {
                Dim(index!(array_expr [self] $n))
            }
        }

        impl IntoDimension for [Ix; $n] {
            type Dim = Dim<[Ix; $n]>;
            #[inline(always)]
            fn into_dimension(self) -> Self::Dim {
                Dim::new(self)
            }
        }

        impl IntoDimension for index!(tuple_type [Ix] $n) {
            type Dim = Dim<[Ix; $n]>;
            #[inline(always)]
            fn into_dimension(self) -> Self::Dim {
                Dim::new(index!(array_expr [self] $n))
            }
        }

        )*
    }
}

index_item!(tuple_to_array [] 6);

unsafe impl Dimension for Ix0 {
    type SliceArg = [Si; 0];
    type Pattern = ();
    // empty product is 1 -> size is 1
    #[inline]
    fn ndim(&self) -> usize { 0 }
    #[inline]
    fn slice(&self) -> &[Ix] { &self[..] }
    #[inline]
    fn slice_mut(&mut self) -> &mut [Ix] { &mut self[..] }
    #[inline]
    fn _fastest_varying_stride_order(&self) -> Self { Ix0() }
    #[inline]
    fn into_pattern(self) -> Self::Pattern {
        self.convert()
    }
    #[inline]
    fn next_for(&self, _index: Self) -> Option<Self> {
        None
    }
}

unsafe impl Dimension for Ix1 {
    type SliceArg = [Si; 1];
    type Pattern = Ix;
    #[inline]
    fn ndim(&self) -> usize { 1 }
    #[inline]
    fn slice(&self) -> &[Ix] { &self[..] }
    #[inline]
    fn slice_mut(&mut self) -> &mut [Ix] { &mut self[..] }
    #[inline]
    fn into_pattern(self) -> Self::Pattern {
        self[0]
    }
    #[inline]
    fn next_for(&self, mut index: Self) -> Option<Self> {
        index[0] += 1;
        if index[0] < self[0] {
            Some(index)
        } else {
            None
        }
    }

    #[inline]
    fn equal(&self, rhs: &Self) -> bool {
        self[0] == rhs[0]
    }

    #[inline]
    fn size(&self) -> usize { self[0] }
    #[inline]
    fn size_checked(&self) -> Option<usize> { Some(self[0]) }

    #[inline]
    fn default_strides(&self) -> Self {
        Ix1(1)
    }

    #[inline]
    fn _fastest_varying_stride_order(&self) -> Self {
        Ix1(0)
    }

    #[inline]
    fn first_index(&self) -> Option<Self> {
        if self[0] != 0 {
            Some(Ix1(0))
        } else {
            None
        }
    }

    /// Self is an index, return the stride offset
    #[inline(always)]
    fn stride_offset(index: &Self, stride: &Self) -> isize {
        stride_offset(index[0], stride[0])
    }

    /// Return stride offset for this dimension and index.
    #[inline]
    fn stride_offset_checked(&self, stride: &Self, index: &Self) -> Option<isize> {
        if index[0] < self[0] {
            Some(stride_offset(index[0], stride[0]))
        } else {
            None
        }
    }
}

unsafe impl Dimension for Ix2 {
    type SliceArg = [Si; 2];
    type Pattern = (Ix, Ix);
    #[inline]
    fn ndim(&self) -> usize { 2 }
    #[inline]
    fn into_pattern(self) -> Self::Pattern {
        self.convert()
    }
    #[inline]
    fn slice(&self) -> &[Ix] { &self[..] }
    #[inline]
    fn slice_mut(&mut self) -> &mut [Ix] { &mut self[..] }
    #[inline]
    fn next_for(&self, index: Self) -> Option<Self> {
        let mut i = index[0];
        let mut j = index[1];
        let imax = self[0];
        let jmax = self[1];
        j += 1;
        if j >= jmax {
            j = 0;
            i += 1;
            if i >= imax {
                return None;
            }
        }
        Some(Ix2(i, j))
    }

    #[inline]
    fn equal(&self, rhs: &Self) -> bool {
        self[0] == rhs[0] && self[1] == rhs[1]
    }

    #[inline]
    fn size(&self) -> usize { self[0] * self[1] }

    #[inline]
    fn size_checked(&self) -> Option<usize> {
        let m = self[0];
        let n = self[1];
        (m as usize).checked_mul(n as usize)
    }

    #[inline]
    fn last_elem(&self) -> usize {
        self[1]
    }

    #[inline]
    fn set_last_elem(&mut self, i: usize) {
        self[1] = i;
    }

    #[inline]
    fn default_strides(&self) -> Self {
        // Compute default array strides
        // Shape (a, b, c) => Give strides (b * c, c, 1)
        Ix2(self[1], 1)
    }
    #[inline]
    fn fortran_strides(&self) -> Self {
        Ix2(1, self[0])
    }

    #[inline]
    fn _fastest_varying_stride_order(&self) -> Self {
        if self[0] as Ixs <= self[1] as Ixs { Ix2(0, 1) } else { Ix2(1, 0) }
    }

    #[inline]
    fn is_contiguous(dim: &Self, strides: &Self) -> bool {
        let defaults = dim.default_strides();
        if strides.equal(&defaults) {
            return true;
        }
        
        if dim.ndim() == 1 { return false; }
        let order = strides._fastest_varying_stride_order();
        let strides = strides.slice();

        // FIXME: Negative strides
        let dim_slice = dim.slice();
        let mut cstride = 1;
        for &i in order.slice() {
            // a dimension of length 1 can have unequal strides
            if dim_slice[i] != 1 && strides[i] != cstride {
                return false;
            }
            cstride *= dim_slice[i];
        }
        true
    }

    #[inline]
    fn first_index(&self) -> Option<Self> {
        let m = self[0];
        let n = self[1];
        if m != 0 && n != 0 {
            Some(Ix2(0, 0))
        } else {
            None
        }
    }

    /// Self is an index, return the stride offset
    #[inline(always)]
    fn stride_offset(index: &Self, strides: &Self) -> isize {
        let i = index[0];
        let j = index[1];
        let s = strides[0];
        let t = strides[1];
        stride_offset(i, s) + stride_offset(j, t)
    }

    /// Return stride offset for this dimension and index.
    #[inline]
    fn stride_offset_checked(&self, strides: &Self, index: &Self) -> Option<isize>
    {
        let m = self[0];
        let n = self[1];
        let i = index[0];
        let j = index[1];
        let s = strides[0];
        let t = strides[1];
        if i < m && j < n {
            Some(stride_offset(i, s) + stride_offset(j, t))
        } else {
            None
        }
    }
}

unsafe impl Dimension for Ix3 {
    type SliceArg = [Si; 3];
    type Pattern = (Ix, Ix, Ix);
    #[inline]
    fn ndim(&self) -> usize { 3 }
    #[inline]
    fn into_pattern(self) -> Self::Pattern {
        self.convert()
    }
    #[inline]
    fn slice(&self) -> &[Ix] { &self[..] }
    #[inline]
    fn slice_mut(&mut self) -> &mut [Ix] { &mut self[..] }

    #[inline]
    fn size(&self) -> usize {
        let m = self[0];
        let n = self[1];
        let o = self[2];
        m as usize * n as usize * o as usize
    }

    #[inline]
    fn next_for(&self, index: Self) -> Option<Self> {
        let mut i = index[0];
        let mut j = index[1];
        let mut k = index[2];
        let imax = self[0];
        let jmax = self[1];
        let kmax = self[2];
        k += 1;
        if k == kmax {
            k = 0;
            j += 1;
            if j == jmax {
                j = 0;
                i += 1;
                if i == imax {
                    return None;
                }
            }
        }
        Some(Ix3(i, j, k))
    }

    /// Self is an index, return the stride offset
    #[inline]
    fn stride_offset(index: &Self, strides: &Self) -> isize {
        let i = index[0];
        let j = index[1];
        let k = index[2];
        let s = strides[0];
        let t = strides[1];
        let u = strides[2];
        stride_offset(i, s) + stride_offset(j, t) + stride_offset(k, u)
    }

    #[inline]
    fn _fastest_varying_stride_order(&self) -> Self {
        let mut stride = *self;
        let mut order = Ix3(0, 1, 2);
        macro_rules! swap {
            ($stride:expr, $order:expr, $x:expr, $y:expr) => {
                if $stride[$x] > $stride[$y] {
                    $stride.swap($x, $y);
                    $order.swap($x, $y);
                }
            }
        }
        {
            // stable sorting network for 3 elements
            let strides = stride.slice_mut();
            swap![strides, order, 1, 2];
            swap![strides, order, 0, 1];
            swap![strides, order, 1, 2];
        }
        order
    }
}

macro_rules! large_dim {
    ($n:expr, $($ix:ident),+) => (
        unsafe impl Dimension for Dim<[Ix; $n]> {
            type SliceArg = [Si; $n];
            type Pattern = ($($ix,)*);
            #[inline]
            fn ndim(&self) -> usize { $n }
            #[inline]
            fn into_pattern(self) -> Self::Pattern {
                self.convert()
            }
            #[inline]
            fn slice(&self) -> &[Ix] { &self[..] }
            #[inline]
            fn slice_mut(&mut self) -> &mut [Ix] { &mut self[..] }
        }
    )
}

large_dim!(4, Ix, Ix, Ix, Ix);
large_dim!(5, Ix, Ix, Ix, Ix, Ix);
/*
large_dim!(6, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(7, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(8, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(9, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(10, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(11, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
large_dim!(12, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix, Ix);
*/

/// Vec<Ix> is a "dynamic" index, pretty hard to use when indexing,
/// and memory wasteful, but it allows an arbitrary and dynamic number of axes.
unsafe impl Dimension for Dim<Vec<Ix>>
{
    type SliceArg = [Si];
    type Pattern = Self;
    fn ndim(&self) -> usize { self.len() }
    fn slice(&self) -> &[Ix] { self }
    fn slice_mut(&mut self) -> &mut [Ix] { self }
    #[inline]
    fn into_pattern(self) -> Self::Pattern {
        self
    }
}

/// Array shape with a next smaller dimension.
///
/// `RemoveAxis` defines a larger-than relation for array shapes:
/// removing one axis from *Self* gives smaller dimension *Smaller*.
pub trait RemoveAxis : Dimension {
    type Smaller: Dimension;
    fn remove_axis(&self, axis: Axis) -> Self::Smaller;
}

impl RemoveAxis for Ix1 {
    type Smaller = Ix0;
    #[inline]
    fn remove_axis(&self, _: Axis) -> Ix0 { Ix0() }
}

impl RemoveAxis for Ix2 {
    type Smaller = Ix1;
    #[inline]
    fn remove_axis(&self, axis: Axis) -> Ix1 {
        let axis = axis.axis();
        debug_assert!(axis < self.ndim());
        if axis == 0 { Ix1(self[1]) } else { Ix1(self[0]) }
    }
}

macro_rules! impl_remove_axis_array(
    ($($n:expr),*) => (
    $(
        impl RemoveAxis for Dim<[Ix; $n]>
        {
            type Smaller = Dim<[Ix; $n - 1]>;
            #[inline]
            fn remove_axis(&self, axis: Axis) -> Self::Smaller {
                let mut tup = Dim([0; $n - 1]);
                {
                    let mut it = tup.slice_mut().iter_mut();
                    for (i, &d) in self.slice().iter().enumerate() {
                        if i == axis.axis() {
                            continue;
                        }
                        for rr in it.by_ref() {
                            *rr = d;
                            break
                        }
                    }
                }
                tup
            }
        }
    )*
    );
);

// 12 is the maximum number for having the Eq trait from libstd
impl_remove_axis_array!(3, 4, 5);


impl RemoveAxis for Dim<Vec<Ix>> {
    type Smaller = Self;
    fn remove_axis(&self, axis: Axis) -> Self {
        let mut res = self.clone();
        res.remove(axis.axis());
        res
    }
}

/// Tuple or fixed size arrays that can be used to index an array.
///
/// ```
/// use ndarray::arr2;
///
/// let mut a = arr2(&[[0, 1], [0, 0]]);
/// a[[1, 1]] = 1;
/// assert_eq!(a[[0, 1]], 1);
/// assert_eq!(a[[1, 1]], 1);
/// ```
///
/// **Note** that `NdIndex` is implemented for all `D where D: Dimension`.
pub unsafe trait NdIndex<E> : Debug {
    #[doc(hidden)]
    fn index_checked(&self, dim: &E, strides: &E) -> Option<isize>;
    fn index_unchecked(&self, strides: &E) -> isize;
}

unsafe impl<D> NdIndex<D> for D
    where D: Dimension
{
    fn index_checked(&self, dim: &D, strides: &D) -> Option<isize> {
        dim.stride_offset_checked(strides, self)
    }
    fn index_unchecked(&self, strides: &D) -> isize {
        D::stride_offset(self, strides)
    }
}

unsafe impl NdIndex<Ix0> for () {
    #[inline]
    fn index_checked(&self, dim: &Ix0, strides: &Ix0) -> Option<isize> {
        dim.stride_offset_checked(strides, &Ix0())
    }
    #[inline(always)]
    fn index_unchecked(&self, _strides: &Ix0) -> isize {
        0
    }
}

unsafe impl NdIndex<Ix1> for Ix {
    #[inline]
    fn index_checked(&self, dim: &Ix1, strides: &Ix1) -> Option<isize> {
        dim.stride_offset_checked(strides, &Ix1(*self))
    }
    #[inline(always)]
    fn index_unchecked(&self, strides: &Ix1) -> isize {
        stride_offset(*self, strides[0])
    }
}

unsafe impl NdIndex<Ix2> for (Ix, Ix) {
    #[inline]
    fn index_checked(&self, dim: &Ix2, strides: &Ix2) -> Option<isize> {
        dim.stride_offset_checked(strides, &Ix2(self.0, self.1))
    }
    #[inline]
    fn index_unchecked(&self, strides: &Ix2) -> isize {
        stride_offset(self.0, strides[0]) + 
        stride_offset(self.1, strides[1])
    }
}
unsafe impl NdIndex<Ix3> for (Ix, Ix, Ix) {
    #[inline]
    fn index_checked(&self, dim: &Ix3, strides: &Ix3) -> Option<isize> {
        dim.stride_offset_checked(strides, &self.convert())
    }

    #[inline]
    fn index_unchecked(&self, strides: &Ix3) -> isize {
        stride_offset(self.0, strides[0]) + 
        stride_offset(self.1, strides[1]) +
        stride_offset(self.2, strides[2])
    }
}

unsafe impl NdIndex<Ix4> for (Ix, Ix, Ix, Ix) {
    #[inline]
    fn index_checked(&self, dim: &Ix4, strides: &Ix4) -> Option<isize> {
        dim.stride_offset_checked(strides, &self.convert())
    }
    #[inline]
    fn index_unchecked(&self, strides: &Ix4) -> isize {
        zip(&**strides, &*self.convert()).map(|(&s, &i)| stride_offset(i, s)).sum()
    }
}
unsafe impl NdIndex<Ix5> for (Ix, Ix, Ix, Ix, Ix) {
    #[inline]
    fn index_checked(&self, dim: &Ix5, strides: &Ix5) -> Option<isize> {
        dim.stride_offset_checked(strides, &self.convert())
    }
    #[inline]
    fn index_unchecked(&self, strides: &Ix5) -> isize {
        zip(&**strides, &*self.convert()).map(|(&s, &i)| stride_offset(i, s)).sum()
    }
}

unsafe impl NdIndex<Ix2> for [Ix; 2] {
    #[inline]
    fn index_checked(&self, dim: &Ix2, strides: &Ix2) -> Option<isize> {
        dim.stride_offset_checked(strides, &Ix2(self[0], self[1]))
    }
    #[inline]
    fn index_unchecked(&self, strides: &Ix2) -> isize {
        stride_offset(self[0], strides[0]) + 
        stride_offset(self[1], strides[1])
    }
}

unsafe impl NdIndex<Ix3> for [Ix; 3] {
    #[inline]
    fn index_checked(&self, dim: &Ix3, strides: &Ix3) -> Option<isize> {
        dim.stride_offset_checked(strides, &Ix3(self[0], self[1], self[2]))
    }
    #[inline]
    fn index_unchecked(&self, strides: &Ix3) -> isize {
        stride_offset(self[0], strides[0]) + 
        stride_offset(self[1], strides[1]) +
        stride_offset(self[2], strides[2])
    }
}

impl<'a> IntoDimension for &'a [Ix] {
    type Dim = Dim<Vec<Ix>>;
    fn into_dimension(self) -> Self::Dim {
        Dim(self.to_vec())
    }
}

unsafe impl<'a> NdIndex<IxDyn> for &'a [Ix] {
    fn index_checked(&self, dim: &IxDyn, strides: &IxDyn) -> Option<isize> {
        let mut offset = 0;
        for (&d, &i, &s) in zipsl(&dim[..], &self[..]).zip_cons(strides.slice()) {
            if i >= d {
                return None;
            }
            offset += stride_offset(i, s);
        }
        Some(offset)
    }
    fn index_unchecked(&self, strides: &IxDyn) -> isize {
        zip(&**strides, *self).map(|(&s, &i)| stride_offset(i, s)).sum()
    }
}

unsafe impl<'a> NdIndex<IxDyn> for Vec<Ix> {
    fn index_checked(&self, dim: &IxDyn, strides: &IxDyn) -> Option<isize> {
        let mut offset = 0;
        for (&d, &i, &s) in zipsl(&dim[..], &self[..]).zip_cons(strides.slice()) {
            if i >= d {
                return None;
            }
            offset += stride_offset(i, s);
        }
        Some(offset)
    }
    fn index_unchecked(&self, strides: &IxDyn) -> isize {
        zip(&**strides, self).map(|(&s, &i)| stride_offset(i, s)).sum()
    }
}

// NOTE: These tests are not compiled & tested
#[cfg(test)]
mod test {
    use super::Dimension;
    use error::{from_kind, ErrorKind};

    #[test]
    fn slice_indexing_uncommon_strides() {
        let v: Vec<_> = (0..12).collect();
        let dim = (2, 3, 2);
        let strides = (1, 2, 6);
        assert!(super::can_index_slice(&v, &dim, &strides).is_ok());

        let strides = (2, 4, 12);
        assert_eq!(super::can_index_slice(&v, &dim, &strides),
                   Err(from_kind(ErrorKind::OutOfBounds)));
    }

    #[test]
    fn overlapping_strides_dim() {
        let dim = (2, 3, 2);
        let strides = (5, 2, 1);
        assert!(super::dim_stride_overlap(&dim, &strides));
        let strides = (6, 2, 1);
        assert!(!super::dim_stride_overlap(&dim, &strides));
        let strides = (6, 0, 1);
        assert!(super::dim_stride_overlap(&dim, &strides));
    }
}

/// An axis index.
///
/// An axis one of an array’s “dimensions”; an *n*-dimensional array has *n* axes.
/// Axis *0* is the array’s outermost axis and *n*-1 is the innermost.
///
/// All array axis arguments use this type to make the code easier to write
/// correctly and easier to understand.
#[derive(Copy, Eq, Ord, Hash, Debug)]
pub struct Axis(pub usize);

impl Axis {
    #[inline(always)]
    pub fn axis(&self) -> usize { self.0 }
}

macro_rules! clone_from_copy {
    ($typename:ident) => {
        impl Clone for $typename {
            #[inline]
            fn clone(&self) -> Self { *self }
        }
    }
}

macro_rules! derive_cmp {
    ($traitname:ident for $typename:ident, $method:ident -> $ret:ty) => {
        impl $traitname for $typename {
            #[inline(always)]
            fn $method(&self, rhs: &Self) -> $ret {
                (self.0).$method(&rhs.0)
            }
        }
    }
}

derive_cmp!{PartialEq for Axis, eq -> bool}
derive_cmp!{PartialOrd for Axis, partial_cmp -> Option<Ordering>}
clone_from_copy!{Axis}

trait DimNew<I> {
    fn new(index: I) -> Self;
}

pub use self::dim::*;
pub mod dim {
    use super::IntoDimension;
    use super::DimNew;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
    pub struct Dim<I: ?Sized> {
        index: I,
    }

    impl<I> DimNew<I> for Dim<I> {
        fn new(index: I) -> Dim<I> {
            Dim {
                index: index,
            }
        }
    }

    #[allow(non_snake_case)]
    pub fn Dim<T>(index: T) -> T::Dim
        where T: IntoDimension
    {
        index.into_dimension()
    }

    impl<I: ?Sized> PartialEq<I> for Dim<I>
        where I: PartialEq,
    {
        fn eq(&self, rhs: &I) -> bool {
            self.index == *rhs
        }
    }

    use std::ops::{Deref, DerefMut};

    impl<I: ?Sized> Deref for Dim<I> {
        type Target = I;
        fn deref(&self) -> &I { &self.index }
    }
    impl<I: ?Sized> DerefMut for Dim<I>
    {
        fn deref_mut(&mut self) -> &mut I { &mut self.index }
    }

}

