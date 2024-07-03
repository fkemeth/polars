mod nested;

use std::collections::VecDeque;

use arrow::array::{Array, DictionaryArray, DictionaryKey, PrimitiveArray};
use arrow::bitmap::MutableBitmap;
use arrow::datatypes::ArrowDataType;
pub use nested::next_dict as nested_next_dict;
use polars_error::{polars_err, PolarsResult};
use polars_utils::iter::FallibleIterator;

use super::utils::{
    self, dict_indices_decoder, extend_from_decoder, get_selected_rows, DecodedState, Decoder,
    FilteredOptionalPageValidity, MaybeNext, OptionalPageValidity,
};
use super::PagesIter;
use crate::parquet::deserialize::SliceFilteredIter;
use crate::parquet::encoding::hybrid_rle::BufferedHybridRleDecoderIter;
use crate::parquet::encoding::Encoding;
use crate::parquet::page::{DataPage, DictPage, Page};
use crate::parquet::schema::Repetition;

// The state of a `DataPage` of `Primitive` parquet primitive type
#[derive(Debug)]
pub enum State<'a> {
    Optional(Optional<'a>),
    Required(Required<'a>),
    FilteredRequired(FilteredRequired<'a>),
    FilteredOptional(
        FilteredOptionalPageValidity<'a>,
        BufferedHybridRleDecoderIter<'a>,
    ),
}

#[derive(Debug)]
pub struct Required<'a> {
    values: BufferedHybridRleDecoderIter<'a>,
}

impl<'a> Required<'a> {
    fn try_new(page: &'a DataPage) -> PolarsResult<Self> {
        let values = dict_indices_decoder(page)?.into_iter();
        Ok(Self { values })
    }
}

#[derive(Debug)]
pub struct FilteredRequired<'a> {
    values: SliceFilteredIter<BufferedHybridRleDecoderIter<'a>>,
}

impl<'a> FilteredRequired<'a> {
    fn try_new(page: &'a DataPage) -> PolarsResult<Self> {
        let values = dict_indices_decoder(page)?.into_iter();

        let rows = get_selected_rows(page);
        let values = SliceFilteredIter::new(values, rows);

        Ok(Self { values })
    }
}

#[derive(Debug)]
pub struct Optional<'a> {
    values: BufferedHybridRleDecoderIter<'a>,
    validity: OptionalPageValidity<'a>,
}

impl<'a> Optional<'a> {
    fn try_new(page: &'a DataPage) -> PolarsResult<Self> {
        let values = dict_indices_decoder(page)?.into_iter();

        Ok(Self {
            values,
            validity: OptionalPageValidity::try_new(page)?,
        })
    }
}

impl<'a> utils::PageState<'a> for State<'a> {
    fn len(&self) -> usize {
        match self {
            State::Optional(optional) => optional.validity.len(),
            State::Required(required) => required.values.size_hint().0,
            State::FilteredRequired(required) => required.values.size_hint().0,
            State::FilteredOptional(validity, _) => validity.len(),
        }
    }
}

#[derive(Debug)]
pub struct PrimitiveDecoder<K>
where
    K: DictionaryKey,
{
    phantom_k: std::marker::PhantomData<K>,
}

impl<K> Default for PrimitiveDecoder<K>
where
    K: DictionaryKey,
{
    #[inline]
    fn default() -> Self {
        Self {
            phantom_k: std::marker::PhantomData,
        }
    }
}

impl<'a, K> utils::Decoder<'a> for PrimitiveDecoder<K>
where
    K: DictionaryKey,
{
    type State = State<'a>;
    type Dict = ();
    type DecodedState = (Vec<K>, MutableBitmap);

    fn build_state(
        &self,
        page: &'a DataPage,
        _: Option<&'a Self::Dict>,
    ) -> PolarsResult<Self::State> {
        let is_optional =
            page.descriptor.primitive_type.field_info.repetition == Repetition::Optional;
        let is_filtered = page.selected_rows().is_some();

        match (page.encoding(), is_optional, is_filtered) {
            (Encoding::PlainDictionary | Encoding::RleDictionary, false, false) => {
                Required::try_new(page).map(State::Required)
            },
            (Encoding::PlainDictionary | Encoding::RleDictionary, true, false) => {
                Optional::try_new(page).map(State::Optional)
            },
            (Encoding::PlainDictionary | Encoding::RleDictionary, false, true) => {
                FilteredRequired::try_new(page).map(State::FilteredRequired)
            },
            (Encoding::PlainDictionary | Encoding::RleDictionary, true, true) => {
                Ok(State::FilteredOptional(
                    FilteredOptionalPageValidity::try_new(page)?,
                    dict_indices_decoder(page)?.into_iter(),
                ))
            },
            _ => Err(utils::not_implemented(page)),
        }
    }

    fn with_capacity(&self, capacity: usize) -> Self::DecodedState {
        (
            Vec::<K>::with_capacity(capacity),
            MutableBitmap::with_capacity(capacity),
        )
    }

    fn extend_from_state(
        &self,
        state: &mut Self::State,
        decoded: &mut Self::DecodedState,
        remaining: usize,
    ) -> PolarsResult<()> {
        let (values, validity) = decoded;
        match state {
            State::Optional(page) => {
                extend_from_decoder(
                    validity,
                    &mut page.validity,
                    Some(remaining),
                    values,
                    &mut page.values.by_ref().map(|x| {
                        match (x as usize).try_into() {
                            Ok(key) => key,
                            // todo: convert this to an error.
                            Err(_) => panic!("The maximum key is too small"),
                        }
                    }),
                );
                page.values.get_result()?;
            },
            State::Required(page) => {
                values.extend(
                    page.values
                        .by_ref()
                        .map(|x| {
                            let x: K = match (x as usize).try_into() {
                                Ok(key) => key,
                                // todo: convert this to an error.
                                Err(_) => {
                                    panic!("The maximum key is too small")
                                },
                            };
                            x
                        })
                        .take(remaining),
                );
                page.values.get_result()?;
            },
            State::FilteredOptional(page_validity, page_values) => {
                extend_from_decoder(
                    validity,
                    page_validity,
                    Some(remaining),
                    values,
                    &mut page_values.by_ref().map(|x| {
                        let x: K = match (x as usize).try_into() {
                            Ok(key) => key,
                            // todo: convert this to an error.
                            Err(_) => {
                                panic!("The maximum key is too small")
                            },
                        };
                        x
                    }),
                );
                page_values.get_result()?;
            },
            State::FilteredRequired(page) => {
                values.extend(
                    page.values
                        .by_ref()
                        .map(|x| {
                            let x: K = match (x as usize).try_into() {
                                Ok(key) => key,
                                // todo: convert this to an error.
                                Err(_) => {
                                    panic!("The maximum key is too small")
                                },
                            };
                            x
                        })
                        .take(remaining),
                );
                page.values.iter.get_result()?;
            },
        }
        Ok(())
    }

    fn deserialize_dict(&self, _: &DictPage) -> Self::Dict {}
}

fn finish_key<K: DictionaryKey>(values: Vec<K>, validity: MutableBitmap) -> PrimitiveArray<K> {
    PrimitiveArray::new(K::PRIMITIVE.into(), values.into(), validity.into())
}

#[inline]
pub(super) fn next_dict<K: DictionaryKey, I: PagesIter, F: Fn(&DictPage) -> Box<dyn Array>>(
    iter: &mut I,
    items: &mut VecDeque<(Vec<K>, MutableBitmap)>,
    dict: &mut Option<Box<dyn Array>>,
    data_type: ArrowDataType,
    remaining: &mut usize,
    chunk_size: Option<usize>,
    read_dict: F,
) -> MaybeNext<PolarsResult<DictionaryArray<K>>> {
    if items.len() > 1 {
        let (values, validity) = items.pop_front().unwrap();
        let keys = finish_key(values, validity);
        return MaybeNext::Some(DictionaryArray::try_new(
            data_type,
            keys,
            dict.clone().unwrap(),
        ));
    }
    match iter.next() {
        Err(e) => MaybeNext::Some(Err(e.into())),
        Ok(Some(page)) => {
            let (page, dict) = match (&dict, page) {
                (None, Page::Data(_)) => {
                    return MaybeNext::Some(Err(polars_err!(ComputeError:
                        "not implemented: dictionary arrays from non-dict-encoded pages",
                    )));
                },
                (_, Page::Dict(dict_page)) => {
                    *dict = Some(read_dict(dict_page));
                    return next_dict(
                        iter, items, dict, data_type, remaining, chunk_size, read_dict,
                    );
                },
                (Some(dict), Page::Data(page)) => (page, dict),
            };

            // there is a new page => consume the page from the start
            let maybe_page = PrimitiveDecoder::<K>::default().build_state(page, None);
            let page = match maybe_page {
                Ok(page) => page,
                Err(e) => return MaybeNext::Some(Err(e)),
            };

            if let Err(e) = utils::extend_from_new_page(
                page,
                chunk_size,
                items,
                remaining,
                &PrimitiveDecoder::<K>::default(),
            ) {
                return MaybeNext::Some(Err(e));
            }

            if items.front().unwrap().len() < chunk_size.unwrap_or(usize::MAX) {
                MaybeNext::More
            } else {
                let (values, validity) = items.pop_front().unwrap();
                let keys = finish_key(values, validity);
                MaybeNext::Some(DictionaryArray::try_new(data_type, keys, dict.clone()))
            }
        },
        Ok(None) => {
            if let Some((values, validity)) = items.pop_front() {
                // we have a populated item and no more pages
                // the only case where an item's length may be smaller than chunk_size
                debug_assert!(values.len() <= chunk_size.unwrap_or(usize::MAX));

                let keys = finish_key(values, validity);
                MaybeNext::Some(DictionaryArray::try_new(
                    data_type,
                    keys,
                    dict.clone().unwrap(),
                ))
            } else {
                MaybeNext::None
            }
        },
    }
}
