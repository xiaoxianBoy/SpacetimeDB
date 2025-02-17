//! This module implements a fast path for serializing certain types from BFLATN to BSATN.
//!
//! The key insight is that a majority of row types will have a known fixed length,
//! with no variable-length members.
//! BFLATN is designed with this in mind, storing fixed-length portions of rows inline,
//! at the expense of an indirection to reach var-length columns like strings.
//! A majority of these types will also have a fixed BSATN length,
//! but note that BSATN stores sum values (enums) without padding,
//! so row types which contain sums may not have a fixed BSATN length
//! if the sum's variants have different "live" unpadded lengths.
//!
//! For row types with fixed BSATN lengths, we can reduce the BFLATN -> BSATN conversion
//! to a series of `memcpy`s, skipping over padding sequences.
//! This is potentially much faster than the more general  [`crate::bflatn_from::serialize_row_from_page`],
//! which traverses a [`RowTypeLayout`] and dispatches on the type of each column.
//!
//! For example, to serialize a row of type `(u64, u64, u32, u64)`,
//! [`bflatn_from`] will do four dispatches, three calls to `serialize_u64` and one to `serialize_u32`.
//! This module will make 2 `memcpy`s (or actually, `<[u8]>::copy_from_slice`s):
//! one of 20 bytes to copy the leading `(u64, u64, u32)`, which contains no padding,
//! and then one of 8 bytes to copy the trailing `u64`, skipping over 4 bytes of padding in between.

use super::{
    indexes::{Byte, Bytes},
    layout::{
        AlgebraicTypeLayout, HasLayout, PrimitiveType, ProductTypeElementLayout, ProductTypeLayout, RowTypeLayout,
        SumTypeLayout, SumTypeVariantLayout,
    },
    util::range_move,
};
use core::mem::MaybeUninit;
use core::ptr;

/// A precomputed BSATN layout for a type whose encoded length is a known constant,
/// enabling fast BFLATN -> BSATN conversion.
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct StaticBsatnLayout {
    /// The length of the encoded BSATN representation of a row of this type,
    /// in bytes.
    ///
    /// Storing this allows us to pre-allocate correctly-sized buffers,
    /// avoiding potentially-expensive `realloc`s.
    pub(crate) bsatn_length: u16,

    /// A series of `memcpy` invocations from a BFLATN row into a BSATN buffer
    /// which are sufficient to BSATN serialize the row.
    fields: Box<[MemcpyField]>,
}

impl StaticBsatnLayout {
    /// Serialize `row` from BFLATN to BSATN into `buf`.
    ///
    /// # Safety
    ///
    /// - `buf` must be at least `self.bsatn_length` long.
    /// - `row` must store a valid, initialized instance of the BFLATN row type
    ///   for which `self` was computed.
    ///   As a consequence of this, for every `field` in `self.fields`,
    ///   `row[field.bflatn_offset .. field.bflatn_offset + length]` will be initialized.
    pub unsafe fn serialize_row_into(&self, buf: &mut [MaybeUninit<Byte>], row: &Bytes) {
        debug_assert!(buf.len() >= self.bsatn_length as usize);
        for field in &self.fields[..] {
            // SAFETY: forward caller requirements.
            unsafe { field.copy(buf, row) };
        }
    }

    /// Construct a `StaticBsatnLayout` for converting BFLATN rows of `row_type` into BSATN.
    ///
    /// Returns `None` if `row_type` contains a column which does not have a constant length in BSATN,
    /// either a [`VarLenType`]
    /// or a [`SumTypeLayout`] whose variants do not have the same "live" unpadded length.
    pub fn for_row_type(row_type: &RowTypeLayout) -> Option<Self> {
        let mut builder = LayoutBuilder::new_builder();
        builder.visit_product(row_type.product())?;
        Some(builder.build())
    }
}

/// An identifier for a series of bytes within a BFLATN row
/// which can be directly copied into an output BSATN buffer
/// with a known length and offset.
///
/// Within the row type's BFLATN layout, `row[bflatn_offset .. (bflatn_offset + length)]`
/// must not contain any padding bytes,
/// i.e. all of those bytes must be fully initialized if the row is initialized.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
struct MemcpyField {
    /// Offset in the BFLATN row from which to begin `memcpy`ing, in bytes.
    bflatn_offset: u16,

    /// Offset in the BSATN buffer to which to begin `memcpy`ing, in bytes.
    // TODO(perf): Could be a running counter, but this way we just have all the `memcpy` args in one place.
    // Should bench; I (pgoldman 2024-03-25) suspect this allows more insn parallelism and is therefore better.
    bsatn_offset: u16,

    /// Length to `memcpy`, in bytes.
    length: u16,
}

impl MemcpyField {
    /// Copies the bytes at `row[self.bflatn_offset .. self.bflatn_offset + self.length]`
    /// into `buf[self.bsatn_offset + self.length]`.
    ///
    /// # Safety
    ///
    /// - `buf` must be exactly `self.bsatn_offset + self.length` long.
    /// - `row` must be exactly `self.bflatn_offset + self.length` long.
    unsafe fn copy(&self, buf: &mut [MaybeUninit<Byte>], row: &Bytes) {
        let len = self.length as usize;
        // SAFETY: forward caller requirement #1.
        let to = unsafe { buf.get_unchecked_mut(range_move(0..len, self.bsatn_offset as usize)) };
        let dst = to.as_mut_ptr().cast();
        // SAFETY: forward caller requirement #2.
        let from = unsafe { row.get_unchecked(range_move(0..len, self.bflatn_offset as usize)) };
        let src = from.as_ptr();

        // SAFETY:
        // 1. `src` is valid for reads for `len` bytes as it came from `from`, a shared slice.
        // 2. `dst` is valid for writes for `len` bytes as it came from `to`, an exclusive slice.
        // 3. Alignment for `u8` is trivially satisfied for any pointer.
        // 4. As `from` and `to` are shared and exclusive slices, they cannot overlap.
        unsafe { ptr::copy_nonoverlapping(src, dst, len) }
    }

    fn is_empty(&self) -> bool {
        self.length == 0
    }
}

/// A builder for a [`StaticBsatnLayout`].
struct LayoutBuilder {
    /// Always at least one element.
    fields: Vec<MemcpyField>,
}

impl LayoutBuilder {
    fn new_builder() -> Self {
        Self {
            fields: vec![MemcpyField {
                bflatn_offset: 0,
                bsatn_offset: 0,
                length: 0,
            }],
        }
    }

    fn build(self) -> StaticBsatnLayout {
        let LayoutBuilder { fields } = self;
        let fields: Vec<_> = fields.into_iter().filter(|field| !field.is_empty()).collect();
        let bsatn_length = fields.last().map(|last| last.bsatn_offset + last.length).unwrap_or(0);
        let fields = fields.into_boxed_slice();
        StaticBsatnLayout { bsatn_length, fields }
    }

    fn current_field(&self) -> &MemcpyField {
        self.fields.last().unwrap()
    }

    fn current_field_mut(&mut self) -> &mut MemcpyField {
        self.fields.last_mut().unwrap()
    }

    fn next_bflatn_offset(&self) -> u16 {
        let last = self.current_field();
        last.bflatn_offset + last.length
    }

    fn next_bsatn_offset(&self) -> u16 {
        let last = self.current_field();
        last.bsatn_offset + last.length
    }

    fn visit_product(&mut self, product: &ProductTypeLayout) -> Option<()> {
        let base_bflatn_offset = self.next_bflatn_offset();
        for elt in product.elements.iter() {
            self.visit_product_element(elt, base_bflatn_offset)?;
        }
        Some(())
    }

    fn visit_product_element(&mut self, elt: &ProductTypeElementLayout, product_base_offset: u16) -> Option<()> {
        let elt_offset = product_base_offset + elt.offset;
        let next_bflatn_offset = self.next_bflatn_offset();
        if next_bflatn_offset != elt_offset {
            // Padding between previous element and this element,
            // so start a new field.
            //
            // Note that this is the only place we have to reason about alignment and padding
            // because the enclosing `ProductTypeLayout` has already computed valid aligned offsets
            // for the elements.

            let bsatn_offset = self.next_bsatn_offset();
            self.fields.push(MemcpyField {
                bsatn_offset,
                bflatn_offset: elt_offset,
                length: 0,
            });
        }
        self.visit_value(&elt.ty)
    }

    fn visit_value(&mut self, val: &AlgebraicTypeLayout) -> Option<()> {
        match val {
            AlgebraicTypeLayout::Sum(sum) => self.visit_sum(sum),
            AlgebraicTypeLayout::Product(prod) => self.visit_product(prod),
            AlgebraicTypeLayout::Primitive(prim) => {
                self.visit_primitive(prim);
                Some(())
            }

            // Var-len types (obviously) don't have a known BSATN length,
            // so fail.
            AlgebraicTypeLayout::VarLen(_) => None,
        }
    }

    fn visit_sum(&mut self, sum: &SumTypeLayout) -> Option<()> {
        // If the sum has no variants, it's the never type, so there's no point in computing a layout.
        let first_variant = sum.variants.first()?;

        let variant_layout = |variant: &SumTypeVariantLayout| {
            let mut builder = LayoutBuilder::new_builder();
            builder.visit_value(&variant.ty)?;
            Some(builder.build())
        };

        // Check that the variants all have the same `StaticBsatnLayout`.
        // If they don't, bail.
        let first_variant_layout = variant_layout(first_variant)?;
        for later_variant in &sum.variants[1..] {
            let later_variant_layout = variant_layout(later_variant)?;
            if later_variant_layout != first_variant_layout {
                return None;
            }
        }

        if first_variant_layout.bsatn_length == 0 {
            // For C-style enums (those without payloads),
            // simply serialize the tag and move on.
            self.current_field_mut().length += 1;
            return Some(());
        }

        // Now that we've reached this point, we know that `first_variant_layout`
        // applies to the values of all the variants.

        let tag_bflatn_offset = self.next_bflatn_offset();
        let payload_bflatn_offset = tag_bflatn_offset + sum.payload_offset;

        let tag_bsatn_offset = self.next_bsatn_offset();
        let payload_bsatn_offset = tag_bsatn_offset + 1;

        // Serialize the tag, consolidating into the previous memcpy if possible.
        self.visit_primitive(&PrimitiveType::U8);

        if sum.payload_offset > 1 {
            // Add an empty marker field to keep track of padding.
            self.fields.push(MemcpyField {
                bflatn_offset: payload_bflatn_offset,
                bsatn_offset: payload_bsatn_offset,
                length: 0,
            });
        } // Otherwise, nothing to do.

        // Lay out the variants.
        // Since all variants have the same layout, we just use the first one.
        self.visit_value(&first_variant.ty)?;

        Some(())
    }

    fn visit_primitive(&mut self, prim: &PrimitiveType) {
        self.current_field_mut().length += prim.size() as u16
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::blob_store::HashMapBlobStore;
    use proptest::prelude::*;
    use spacetimedb_sats::{bsatn, proptest::generate_typed_row, AlgebraicType, ProductType};

    fn assert_expected_layout(ty: ProductType, bsatn_length: u16, fields: &[(u16, u16, u16)]) {
        let expected_layout = StaticBsatnLayout {
            bsatn_length,
            fields: fields
                .iter()
                .copied()
                .map(|(bflatn_offset, bsatn_offset, length)| MemcpyField {
                    bflatn_offset,
                    bsatn_offset,
                    length,
                })
                .collect(),
        };
        let row_type = RowTypeLayout::from(ty);
        let Some(computed_layout) = StaticBsatnLayout::for_row_type(&row_type) else {
            panic!("assert_expected_layout: Computed `None` for row {row_type:#?}\nExpected:{expected_layout:#?}");
        };
        assert_eq!(
            computed_layout, expected_layout,
            "assert_expected_layout: Computed layout (left) does not match expected layout (right)"
        );
    }

    #[test]
    fn known_types_expected_layout() {
        for prim in [
            AlgebraicType::Bool,
            AlgebraicType::U8,
            AlgebraicType::I8,
            AlgebraicType::U16,
            AlgebraicType::I16,
            AlgebraicType::U32,
            AlgebraicType::I32,
            AlgebraicType::U64,
            AlgebraicType::I64,
            AlgebraicType::U128,
            AlgebraicType::I128,
        ] {
            let size = AlgebraicTypeLayout::from(prim.clone()).size() as u16;
            assert_expected_layout(ProductType::from([prim]), size, &[(0, 0, size)]);
        }

        for (ty, bsatn_length, fields) in [
            (ProductType::new([].into()), 0, &[][..]),
            (
                ProductType::from([AlgebraicType::sum([
                    AlgebraicType::U8,
                    AlgebraicType::I8,
                    AlgebraicType::Bool,
                ])]),
                2,
                // In BFLATN, sums have padding after the tag to the max alignment of any variant payload.
                // In this case, 0 bytes of padding, because all payloads are aligned to 1.
                // Since there's no padding, the memcpys can be consolidated.
                &[(0, 0, 2)][..],
            ),
            (
                ProductType::from([AlgebraicType::sum([
                    AlgebraicType::product([
                        AlgebraicType::U8,
                        AlgebraicType::U8,
                        AlgebraicType::U8,
                        AlgebraicType::U8,
                    ]),
                    AlgebraicType::product([AlgebraicType::U16, AlgebraicType::U16]),
                    AlgebraicType::U32,
                ])]),
                5,
                // In BFLATN, sums have padding after the tag to the max alignment of any variant payload.
                // In this case, 3 bytes of padding.
                &[(0, 0, 1), (4, 1, 4)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::sum([AlgebraicType::U128, AlgebraicType::I128]),
                    AlgebraicType::U32,
                ]),
                21,
                // In BFLATN, sums have padding after the tag to the max alignment of any variant payload.
                // In this case, 15 bytes of padding.
                &[(0, 0, 1), (16, 1, 20)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::U128,
                    AlgebraicType::U64,
                    AlgebraicType::U32,
                    AlgebraicType::U16,
                    AlgebraicType::U8,
                ]),
                31,
                &[(0, 0, 31)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::U8,
                    AlgebraicType::U16,
                    AlgebraicType::U32,
                    AlgebraicType::U64,
                    AlgebraicType::U128,
                ]),
                31,
                &[(0, 0, 1), (2, 1, 30)][..],
            ),
            // Make sure sums with no variant data are handled correctly.
            (
                ProductType::from([AlgebraicType::sum([AlgebraicType::product::<[AlgebraicType; 0]>([])])]),
                1,
                &[(0, 0, 1)][..],
            ),
            (
                ProductType::from([AlgebraicType::sum([
                    AlgebraicType::product::<[AlgebraicType; 0]>([]),
                    AlgebraicType::product::<[AlgebraicType; 0]>([]),
                ])]),
                1,
                &[(0, 0, 1)][..],
            ),
            // Various experiments with 1-byte-aligned payloads.
            // These are particularly nice for memcpy consolidation as there's no padding.
            (
                ProductType::from([AlgebraicType::sum([
                    AlgebraicType::product([AlgebraicType::U8, AlgebraicType::U8]),
                    AlgebraicType::product([AlgebraicType::Bool, AlgebraicType::Bool]),
                ])]),
                3,
                &[(0, 0, 3)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::sum([AlgebraicType::Bool, AlgebraicType::U8]),
                    AlgebraicType::sum([AlgebraicType::U8, AlgebraicType::Bool]),
                ]),
                4,
                &[(0, 0, 4)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::U16,
                    AlgebraicType::sum([AlgebraicType::U8, AlgebraicType::Bool]),
                    AlgebraicType::U16,
                ]),
                6,
                &[(0, 0, 6)][..],
            ),
            (
                ProductType::from([
                    AlgebraicType::U32,
                    AlgebraicType::sum([AlgebraicType::U16, AlgebraicType::I16]),
                    AlgebraicType::U32,
                ]),
                11,
                &[(0, 0, 5), (6, 5, 6)][..],
            ),
        ] {
            assert_expected_layout(ty, bsatn_length, fields);
        }
    }

    #[test]
    fn known_types_not_applicable() {
        for ty in [
            AlgebraicType::String,
            AlgebraicType::bytes(),
            AlgebraicType::never(),
            AlgebraicType::array(AlgebraicType::U16),
            AlgebraicType::map(AlgebraicType::U8, AlgebraicType::I8),
            AlgebraicType::sum([AlgebraicType::U8, AlgebraicType::U16]),
        ] {
            let layout = RowTypeLayout::from(ProductType::from([ty]));
            if let Some(computed) = StaticBsatnLayout::for_row_type(&layout) {
                panic!("Expected row type not to have a constant BSATN layout!\nRow type: {layout:#?}\nBSATN layout: {computed:#?}");
            }
        }
    }

    proptest! {
        // The test `known_bsatn_same_as_bflatn_from` generates a lot of rejects,
        // as a vast majority of the space of `ProductType` does not have a fixed BSATN length.
        // Writing a proptest generator which produces only types that have a fixed BSATN length
        // seems hard, because we'd have to generate sums with known matching layouts,
        // so we just bump the `max_global_rejects` up as high as it'll go and move on with our lives.
        //
        // Note that I (pgoldman 2024-03-21) tried modifying `generate_typed_row`
        // to not emit `String`, `Array` or `Map` types (the trivially var-len types),
        // but did not see a meaningful decrease in the number of rejects.
        // This is because a majority of the var-len BSATN types in the `generate_typed_row` space
        // are due to sums with inconsistent payload layouts.
        //
        // We still include the test `known_bsatn_same_as_bsatn_from`
        // because it tests row types not covered in `known_types_expected_layout`,
        // especially larger types with unusual sequences of aligned fields.
        #![proptest_config(ProptestConfig { max_global_rejects: 65536, ..Default::default()})]

        #[test]
        fn known_bsatn_same_as_bflatn_from((ty, val) in generate_typed_row()) {
            let mut blob_store = HashMapBlobStore::default();
            let mut table = crate::table::test::table(ty);
            let Some(bsatn_layout) = StaticBsatnLayout::for_row_type(table.row_layout()) else {
                // `ty` has a var-len member or a sum with different payload lengths,
                // so the fast path doesn't apply.
                return Err(TestCaseError::reject("Var-length type"));
            };

            let size = table.row_layout().size();
            let (_, row_ref) = table.insert(&mut blob_store, &val).unwrap();

            let slow_path = bsatn::to_vec(&row_ref).unwrap();

            let (page, offset) = row_ref.page_and_offset();
            let bytes = page.get_row_data(offset, size);

            let len = bsatn_layout.bsatn_length as usize;
            let mut fast_path = Vec::with_capacity(len);
            let buf = fast_path.spare_capacity_mut();
            unsafe {
                bsatn_layout.serialize_row_into(buf, bytes);
            }
            unsafe { fast_path.set_len(len); }

            assert_eq!(slow_path, fast_path);
        }
    }
}
