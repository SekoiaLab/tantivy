use std::fmt::Debug;
use std::net::Ipv6Addr;

use columnar::column_values::CompactSpaceU64Accessor;
use columnar::{
    Column, ColumnType, Dictionary, MonotonicallyMappableToU128, MonotonicallyMappableToU64,
    NumericalValue, StrColumn,
};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::aggregation::agg_data::{
    build_segment_agg_collectors, AggRefNode, AggregationsSegmentCtx,
};
use crate::aggregation::bucket::composite::calendar_interval;
use crate::aggregation::bucket::composite::map::{DynArrayHeapMap, MAX_DYN_ARRAY_SIZE};
use crate::aggregation::bucket::{
    parse_into_milliseconds, CalendarInterval, CompositeAggregation, CompositeAggregationSource,
    MissingOrder, Order,
};
use crate::aggregation::format_date;
use crate::aggregation::intermediate_agg_result::{
    IntermediateAggregationResult, IntermediateAggregationResults, IntermediateBucketResult,
    IntermediateCompositeBucketEntry, IntermediateCompositeBucketResult, IntermediateKey,
};
use crate::aggregation::segment_agg_result::SegmentAggregationCollector;
use crate::TantivyError;

#[derive(Clone, Debug)]
struct CompositeBucketCollector {
    count: u32,
    sub_aggs: Option<Box<dyn SegmentAggregationCollector>>,
}

impl CompositeBucketCollector {
    fn new(sub_aggs: Option<Box<dyn SegmentAggregationCollector>>) -> Self {
        CompositeBucketCollector { count: 0, sub_aggs }
    }
    #[inline]
    fn collect(
        &mut self,
        doc: crate::DocId,
        agg_data: &mut AggregationsSegmentCtx,
    ) -> crate::Result<()> {
        self.count += 1;
        if let Some(sub_aggs) = &mut self.sub_aggs {
            sub_aggs.collect(doc, agg_data)?;
        }
        Ok(())
    }
}

/// The value is represented as a tuple of:
/// - the column index or missing value sentinel
///   - if the value is present, store the accessor index + 1
///   - if the value is missing, store 0 (for missing first) or u8::MAX (for missing last)
/// - the fast field value u64 representation
///   - 0 if the field is missing
///   - regular u64 repr if the ordering is ascending
///   - bitwise NOT of the u64 repr if the ordering is descending
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
struct InternalValueRepr(u8, u64);

impl InternalValueRepr {
    #[inline]
    fn new(raw: u64, accessor_idx: u8, order: Order) -> Self {
        match order {
            Order::Asc => InternalValueRepr(accessor_idx + 1, raw),
            Order::Desc => InternalValueRepr(accessor_idx + 1, !raw),
        }
    }
    #[inline]
    fn new_missing(order: Order, missing_order: MissingOrder) -> Self {
        let column_idx = match (missing_order, order) {
            (MissingOrder::First, _) => 0,
            (MissingOrder::Last, _) => u8::MAX,
            (MissingOrder::Default, Order::Asc) => 0,
            (MissingOrder::Default, Order::Desc) => u8::MAX,
        };
        InternalValueRepr(column_idx, 0)
    }
    #[inline]
    fn decode(self, order: Order) -> Option<(u8, u64)> {
        if self.0 == u8::MAX || self.0 == 0 {
            return None;
        }
        match order {
            Order::Asc => Some((self.0 - 1, self.1)),
            Order::Desc => Some((self.0 - 1, !self.1)),
        }
    }
}

/// The collector puts values from the fast field into the correct buckets and
/// does a conversion to the correct datatype.
#[derive(Clone, Debug)]
pub struct SegmentCompositeCollector {
    buckets: DynArrayHeapMap<InternalValueRepr, CompositeBucketCollector>,
    accessor_idx: usize,
}

impl SegmentAggregationCollector for SegmentCompositeCollector {
    fn add_intermediate_aggregation_result(
        self: Box<Self>,
        agg_data: &AggregationsSegmentCtx,
        results: &mut IntermediateAggregationResults,
    ) -> crate::Result<()> {
        let name = agg_data
            .get_composite_req_data(self.accessor_idx)
            .name
            .clone();

        let buckets = self.into_intermediate_bucket_result(agg_data)?;
        results.push(
            name,
            IntermediateAggregationResult::Bucket(IntermediateBucketResult::Composite { buckets }),
        )?;

        Ok(())
    }

    #[inline]
    fn collect(
        &mut self,
        doc: crate::DocId,
        agg_data: &mut AggregationsSegmentCtx,
    ) -> crate::Result<()> {
        self.collect_block(&[doc], agg_data)
    }

    #[inline]
    fn collect_block(
        &mut self,
        docs: &[crate::DocId],
        agg_data: &mut AggregationsSegmentCtx,
    ) -> crate::Result<()> {
        let mem_pre = self.get_memory_consumption();
        let composite_agg_data = agg_data.take_composite_req_data(self.accessor_idx);

        for doc in docs {
            let mut sub_level_values = SmallVec::new();
            recursive_key_visitor(
                *doc,
                agg_data,
                &composite_agg_data,
                0,
                &mut sub_level_values,
                &mut self.buckets,
            )?;
        }
        agg_data.put_back_composite_req_data(self.accessor_idx, composite_agg_data);

        let mem_delta = self.get_memory_consumption() - mem_pre;
        if mem_delta > 0 {
            agg_data.limits.add_memory_consumed(mem_delta)?;
        }

        Ok(())
    }

    fn flush(&mut self, agg_data: &mut AggregationsSegmentCtx) -> crate::Result<()> {
        for sub_agg_collector in self.buckets.values_mut() {
            if let Some(sub_aggs_collector) = &mut sub_agg_collector.sub_aggs {
                sub_aggs_collector.flush(agg_data)?;
            }
        }
        Ok(())
    }
}

impl SegmentCompositeCollector {
    fn get_memory_consumption(&self) -> u64 {
        // TODO: the footprint is underestimated because we don't account for the
        // sub-aggregations which are trait objects
        self.buckets.memory_consumption()
    }

    pub(crate) fn from_req_and_validate(
        req_data: &mut AggregationsSegmentCtx,
        node: &AggRefNode,
    ) -> crate::Result<Self> {
        validate_req(req_data, node.idx_in_req_data)?;

        let has_sub_aggregations = !node.children.is_empty();
        let blueprint = if has_sub_aggregations {
            let sub_aggregation = build_segment_agg_collectors(req_data, &node.children)?;
            Some(sub_aggregation)
        } else {
            None
        };
        let composite_req_data = req_data.get_composite_req_data_mut(node.idx_in_req_data);
        composite_req_data.sub_aggregation_blueprint = blueprint;

        Ok(SegmentCompositeCollector {
            buckets: DynArrayHeapMap::try_new(composite_req_data.req.sources.len())?,
            accessor_idx: node.idx_in_req_data,
        })
    }

    #[inline]
    pub(crate) fn into_intermediate_bucket_result(
        self,
        agg_data: &AggregationsSegmentCtx,
    ) -> crate::Result<IntermediateCompositeBucketResult> {
        let mut dict: FxHashMap<Vec<Option<IntermediateKey>>, IntermediateCompositeBucketEntry> =
            Default::default();
        dict.reserve(self.buckets.size());
        let composite_data = agg_data.get_composite_req_data(self.accessor_idx);
        for (key_internal_repr, agg) in self.buckets.into_iter() {
            let key = resolve_key(&key_internal_repr, composite_data)?;
            let mut sub_aggregation_res = IntermediateAggregationResults::default();
            if let Some(sub_aggs_collector) = agg.sub_aggs {
                sub_aggs_collector
                    .add_intermediate_aggregation_result(agg_data, &mut sub_aggregation_res)?;
            }

            dict.insert(
                key,
                IntermediateCompositeBucketEntry {
                    doc_count: agg.count,
                    sub_aggregation: sub_aggregation_res,
                },
            );
        }

        Ok(IntermediateCompositeBucketResult {
            entries: dict,
            target_size: composite_data.req.size,
            orders: composite_data
                .req
                .sources
                .iter()
                .map(|source| match source {
                    CompositeAggregationSource::Terms(t) => (t.order, t.missing_order),
                    CompositeAggregationSource::Histogram(h) => (h.order, h.missing_order),
                    CompositeAggregationSource::DateHistogram(d) => (d.order, d.missing_order),
                })
                .collect(),
        })
    }
}

fn validate_req(req_data: &mut AggregationsSegmentCtx, accessor_idx: usize) -> crate::Result<()> {
    let composite_data = req_data.get_composite_req_data(accessor_idx);
    let req = &composite_data.req;
    if req.sources.is_empty() {
        return Err(TantivyError::InvalidArgument(
            "composite aggregation must have at least one source".to_string(),
        ));
    }
    if req.size == 0 {
        return Err(TantivyError::InvalidArgument(
            "composite aggregation 'size' must be > 0".to_string(),
        ));
    }
    let col_types = composite_data
        .composite_accessors
        .iter()
        .map(|accessors| accessors.iter().map(|a| a.column_type).collect::<Vec<_>>());

    for source_columns in col_types {
        if source_columns.len() > MAX_DYN_ARRAY_SIZE {
            return Err(TantivyError::InvalidArgument(format!(
                "composite aggregation source supports maximum {MAX_DYN_ARRAY_SIZE} sources",
            )));
        }
        if source_columns.contains(&ColumnType::Bytes) {
            return Err(TantivyError::InvalidArgument(
                "composite aggregation does not support 'bytes' field type".to_string(),
            ));
        }
        if source_columns.contains(&ColumnType::DateTime) && source_columns.len() > 1 {
            return Err(TantivyError::InvalidArgument(
                "composite aggregation expects 'date' fields to have a single column".to_string(),
            ));
        }
        if source_columns.contains(&ColumnType::IpAddr) && source_columns.len() > 1 {
            return Err(TantivyError::InvalidArgument(
                "composite aggregation expects 'ip' fields to have a single column".to_string(),
            ));
        }
    }
    Ok(())
}

fn collect_bucket_with_limit(
    doc_id: crate::DocId,
    agg_data: &mut AggregationsSegmentCtx,
    composite_agg_data: &CompositeAggReqData,
    buckets: &mut DynArrayHeapMap<InternalValueRepr, CompositeBucketCollector>,
    key: &[InternalValueRepr],
) -> crate::Result<()> {
    // we still have room for buckets, just insert
    if (buckets.size() as u32) < composite_agg_data.req.size {
        buckets
            .get_or_insert_with(key, || {
                CompositeBucketCollector::new(composite_agg_data.sub_aggregation_blueprint.clone())
            })
            .collect(doc_id, agg_data)?;
        return Ok(());
    }

    // map is full, but we can still update the bucket if it already exists
    if let Some(entry) = buckets.get_mut(key) {
        entry.collect(doc_id, agg_data)?;
        return Ok(());
    }

    // check if the item qualfies to enter the top-k, and evict the highest if it does
    if let Some(highest_key) = buckets.peek_highest() {
        if key < highest_key {
            buckets.evict_highest();
            buckets
                .get_or_insert_with(key, || {
                    CompositeBucketCollector::new(
                        composite_agg_data.sub_aggregation_blueprint.clone(),
                    )
                })
                .collect(doc_id, agg_data)?;
        }
    }

    Ok(())
}

fn resolve_key(
    internal_key: &[InternalValueRepr],
    agg_data: &CompositeAggReqData,
) -> crate::Result<Vec<Option<IntermediateKey>>> {
    internal_key
        .into_iter()
        .enumerate()
        .map(|(idx, val)| {
            resolve_internal_value_repr(
                *val,
                &agg_data.req.sources[idx],
                &agg_data.composite_accessors[idx],
            )
        })
        .collect()
}

fn resolve_internal_value_repr(
    internal_value_repr: InternalValueRepr,
    source: &CompositeAggregationSource,
    composite_accessors: &[CompositeAccessor],
) -> crate::Result<Option<IntermediateKey>> {
    let decoded_value_opt = match source {
        CompositeAggregationSource::Terms(source) => internal_value_repr.decode(source.order),
        CompositeAggregationSource::Histogram(source) => internal_value_repr.decode(source.order),
        CompositeAggregationSource::DateHistogram(source) => {
            internal_value_repr.decode(source.order)
        }
    };
    let Some((decoded_accessor_idx, val)) = decoded_value_opt else {
        return Ok(None);
    };
    let CompositeAccessor {
        column_type,
        str_dict_column,
        column,
        ..
    } = &composite_accessors[decoded_accessor_idx as usize];
    let key = match source {
        CompositeAggregationSource::Terms(_) => {
            resolve_term(val, column_type, str_dict_column, column)?
        }
        CompositeAggregationSource::Histogram(_) => IntermediateKey::F64(f64::from_u64(val)),
        CompositeAggregationSource::DateHistogram(_) => IntermediateKey::I64(i64::from_u64(val)),
    };

    Ok(Some(key))
}

fn resolve_term(
    val: u64,
    column_type: &ColumnType,
    str_dict_column: &Option<StrColumn>,
    column: &Column,
) -> crate::Result<IntermediateKey> {
    let key = if *column_type == ColumnType::Str {
        let fallback_dict = Dictionary::empty();
        let term_dict = str_dict_column
            .as_ref()
            .map(|el| el.dictionary())
            .unwrap_or_else(|| &fallback_dict);

        // TODO try use sorted_ords_to_term_cb to batch
        let mut buffer = Vec::new();
        term_dict.ord_to_term(val, &mut buffer)?;
        IntermediateKey::Str(
            String::from_utf8(buffer.to_vec()).expect("could not convert to String"),
        )
    } else if *column_type == ColumnType::DateTime {
        let val = i64::from_u64(val);
        let date = format_date(val)?;
        IntermediateKey::Str(date)
    } else if *column_type == ColumnType::Bool {
        let val = bool::from_u64(val);
        IntermediateKey::Bool(val)
    } else if *column_type == ColumnType::IpAddr {
        let compact_space_accessor = column
            .values
            .clone()
            .downcast_arc::<CompactSpaceU64Accessor>()
            .map_err(|_| {
                TantivyError::AggregationError(crate::aggregation::AggregationError::InternalError(
                    "Type mismatch: Could not downcast to CompactSpaceU64Accessor".to_string(),
                ))
            })?;
        let val: u128 = compact_space_accessor.compact_to_u128(val as u32);
        let val = Ipv6Addr::from_u128(val);
        IntermediateKey::IpAddr(val)
    } else {
        if *column_type == ColumnType::U64 {
            IntermediateKey::U64(val)
        } else if *column_type == ColumnType::I64 {
            IntermediateKey::I64(i64::from_u64(val))
        } else {
            let val = f64::from_u64(val);
            let val: NumericalValue = val.into();

            match val.normalize() {
                NumericalValue::U64(val) => IntermediateKey::U64(val),
                NumericalValue::I64(val) => IntermediateKey::I64(val),
                NumericalValue::F64(val) => IntermediateKey::F64(val),
            }
        }
    };
    Ok(key)
}

/// Depth-first walk of the accessors to build the composite key combinations
/// and update the buckets.
fn recursive_key_visitor(
    doc_id: crate::DocId,
    agg_data: &mut AggregationsSegmentCtx,
    composite_agg_data: &CompositeAggReqData,
    source_offset: usize,
    sub_level_values: &mut SmallVec<[InternalValueRepr; MAX_DYN_ARRAY_SIZE]>,
    buckets: &mut DynArrayHeapMap<InternalValueRepr, CompositeBucketCollector>,
) -> crate::Result<()> {
    if source_offset == composite_agg_data.req.sources.len() {
        collect_bucket_with_limit(
            doc_id,
            agg_data,
            composite_agg_data,
            buckets,
            sub_level_values,
        )?;
        return Ok(());
    }

    let current_level_accessor = &composite_agg_data.composite_accessors[source_offset];
    let current_level_source = &composite_agg_data.req.sources[source_offset];
    let mut missing = true;
    for (i, accessor) in current_level_accessor.iter().enumerate() {
        // TODO: optimize with prefetching using fetch_block
        let values = accessor.column.values_for_doc(doc_id);
        for value in values {
            missing = false;
            match current_level_source {
                CompositeAggregationSource::Terms(source) => {
                    sub_level_values.push(InternalValueRepr::new(value, i as u8, source.order));
                }
                CompositeAggregationSource::Histogram(source) => {
                    let float_value = match accessor.column_type {
                        ColumnType::U64 => value as f64,
                        ColumnType::I64 => i64::from_u64(value) as f64,
                        // Dates are stored as nanoseconds since epoch but the
                        // interval is in milliseconds
                        ColumnType::DateTime => i64::from_u64(value) as f64 / 1_000_000.,
                        ColumnType::F64 => f64::from_u64(value),
                        _ => {
                            panic!(
                                "unexpected type {:?}. This should not happen",
                                accessor.column_type
                            )
                        }
                    };
                    let bucket_value = (float_value / source.interval).floor() * source.interval;
                    let bucket_value_u64 = f64::to_u64(bucket_value);
                    sub_level_values.push(InternalValueRepr::new(
                        bucket_value_u64,
                        i as u8,
                        source.order,
                    ));
                }
                CompositeAggregationSource::DateHistogram(hist_source) => {
                    let value_ns = match accessor.column_type {
                        // Dates are stored as nanoseconds since epoch but the
                        // interval is in milliseconds
                        ColumnType::DateTime => i64::from_u64(value),
                        _ => {
                            panic!(
                                "unexpected type {:?}. This should not happen",
                                accessor.column_type
                            )
                        }
                    };
                    let bucket_value_i64 = match accessor.date_histogram_interval {
                        PrecomputedDateInterval::FixedMilliseconds(fixed_interval_ms) => {
                            (value_ns / 1_000_000 / fixed_interval_ms) * fixed_interval_ms
                        }
                        PrecomputedDateInterval::Calendar(CalendarInterval::Year) => {
                            calendar_interval::try_year_bucket(value_ns)? / 1_000_000
                        }
                        PrecomputedDateInterval::Calendar(CalendarInterval::Month) => {
                            calendar_interval::try_month_bucket(value_ns)? / 1_000_000
                        }
                        PrecomputedDateInterval::Calendar(CalendarInterval::Week) => {
                            calendar_interval::week_bucket(value_ns) / 1_000_000
                        }
                        PrecomputedDateInterval::NotApplicable => {
                            panic!("interval not precomputed for date histogram source")
                        }
                    };
                    let bucket_value_u64 = i64::to_u64(bucket_value_i64);
                    sub_level_values.push(InternalValueRepr::new(
                        bucket_value_u64,
                        i as u8,
                        hist_source.order,
                    ));
                }
            }
            recursive_key_visitor(
                doc_id,
                agg_data,
                composite_agg_data,
                source_offset + 1,
                sub_level_values,
                buckets,
            )?;
            sub_level_values.pop();
        }
    }
    if missing {
        match current_level_source {
            CompositeAggregationSource::Terms(source) => {
                if source.missing_bucket == false {
                    // missing bucket not requested, skip this branch
                    return Ok(());
                }
                sub_level_values.push(InternalValueRepr::new_missing(
                    source.order,
                    source.missing_order,
                ));
            }
            CompositeAggregationSource::Histogram(source) => {
                if source.missing_bucket == false {
                    // missing bucket not requested, skip this branch
                    return Ok(());
                }
                sub_level_values.push(InternalValueRepr::new_missing(
                    source.order,
                    source.missing_order,
                ));
            }
            CompositeAggregationSource::DateHistogram(source) => {
                if source.missing_bucket == false {
                    // missing bucket not requested, skip this branch
                    return Ok(());
                }
                sub_level_values.push(InternalValueRepr::new_missing(
                    source.order,
                    source.missing_order,
                ));
            }
        }

        recursive_key_visitor(
            doc_id,
            agg_data,
            composite_agg_data,
            source_offset + 1,
            sub_level_values,
            buckets,
        )?;
        sub_level_values.pop();
    }
    Ok(())
}

// context data for a composite aggregation

/// Contains all information required by the [SegmentCompositeCollector] to perform the
/// composite aggregation on a segment.
pub struct CompositeAggReqData {
    /// Note: sub_aggregation_blueprint is filled later when building collectors
    pub sub_aggregation_blueprint: Option<Box<dyn SegmentAggregationCollector>>,
    /// The name of the aggregation.
    pub name: String,
    /// The normalized term aggregation request.
    pub req: CompositeAggregation,
    /// Accessors for each source, each source can have multiple accessors (columns).
    pub composite_accessors: Vec<Vec<CompositeAccessor>>,
}

/// A parsed representation of the date interval for date histogram sources
#[derive(Clone, Copy, Debug)]
pub enum PrecomputedDateInterval {
    /// This is not a date histogram source
    NotApplicable,
    /// Source was configured with a fixed interval
    FixedMilliseconds(i64),
    /// Source was configured with a calendar interval
    Calendar(CalendarInterval),
}

impl PrecomputedDateInterval {
    /// Validates the date histogram source interval fields and parses a date interval from them.
    pub fn from_date_histogram_source_intervals(
        fixed_interval: &Option<String>,
        calendar_interval: Option<CalendarInterval>,
    ) -> crate::Result<Self> {
        match (fixed_interval, calendar_interval) {
            (Some(_), Some(_)) | (None, None) => Err(TantivyError::InvalidArgument(
                "date histogram source must one and only one of fixed_interval or \
                 calendar_interval set"
                    .to_string(),
            )),
            (Some(fixed_interval), None) => {
                let fixed_interval_ms = parse_into_milliseconds(&fixed_interval)?;
                Ok(PrecomputedDateInterval::FixedMilliseconds(
                    fixed_interval_ms,
                ))
            }
            (None, Some(calendar_interval)) => {
                Ok(PrecomputedDateInterval::Calendar(calendar_interval))
            }
        }
    }
}

/// Accessors for a single column in a composite source.
pub struct CompositeAccessor {
    /// The fast field column
    pub column: Column<u64>,
    /// The column type
    pub column_type: ColumnType,
    /// Term dictionary if the column type is Str
    ///
    /// Only used by term sources
    pub str_dict_column: Option<StrColumn>,
    /// Parsed date interval for date histogram sources
    pub date_histogram_interval: PrecomputedDateInterval,
}
