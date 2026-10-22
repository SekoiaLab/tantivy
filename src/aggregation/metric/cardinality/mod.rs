//! Cardinality aggregation.
//!
//!   * [`str_collector`] holds the `ColumnType::Str` segment collector, which accumulates term
//!     ordinals and resolves them to HLL coupons at finalization.
//!   * [`numeric_collector`] holds the segment collector for every other column type, which feeds
//!     the HLL sketch directly during collection.
//!
//! Both segment collectors converge on the same
//! [`IntermediateMetricResult::Cardinality`] payload, so results coming from a
//! str column and from a numeric column (e.g. a JSON path that resolves to
//! both) merge through the single [`CardinalityCollector::merge_fruits`]
//! implementation here.

mod numeric_collector;
mod str_collector;
mod term_ord_accumulator;

use std::hash::Hash;

use columnar::ColumnType;
use common::BitSet;
use datasketches::hll::{Coupon, HllSketch, HllType, HllUnion};
pub(crate) use numeric_collector::SegmentNumericCardinalityCollector;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub(crate) use str_collector::SegmentStrCardinalityCollector;
pub(crate) use term_ord_accumulator::{TermOrdSet, BITSET_MAX_TERM_ORD};

use crate::aggregation::segment_agg_result::SegmentAggregationCollector;
use crate::aggregation::value_source::ValueSource;
use crate::aggregation::*;

/// Precision threshold used when the request doesn't set one, the same default as Elasticsearch.
const DEFAULT_PRECISION_THRESHOLD: u64 = 3_000;
/// Smallest log2 of the number of HLL registers supported by the sketch.
const MIN_LG_K: u8 = 4;
/// Largest log2 of the number of HLL registers we allow. It is what Elasticsearch's maximum
/// precision threshold (40000) maps to, and bounds a sketch to 256KB (Hll4).
const MAX_LG_K: u8 = 19;
/// Hll4 packs registers in 4 bits: half the size of Hll8, with identical estimates.
const HLL_TYPE: HllType = HllType::Hll4;

/// Returns the log2 of the number of HLL registers for a precision threshold.
///
/// A sketch keeps the hashes of the values it has seen (coupon mode), which is near-exact, until
/// it holds `0.75 * 2^(lg_k - 3)` values. We pick the smallest `lg_k` for which this covers the
/// threshold, so that, like in Elasticsearch, counts below the threshold are expected to be close
/// to accurate. Thresholds are capped at [`MAX_LG_K`].
fn lg_k_for_precision_threshold(precision_threshold: u64) -> u8 {
    (MIN_LG_K..MAX_LG_K)
        // 0.75 * 2^(lg_k - 3) >= threshold, in integers.
        .find(|lg_k| 3u128 << (lg_k - 3) >= 4 * precision_threshold as u128)
        .unwrap_or(MAX_LG_K)
}

/// # Cardinality
///
/// The cardinality aggregation allows for computing an estimate
/// of the number of different values in a data set based on the
/// Apache DataSketches HyperLogLog algorithm. This is particularly useful for
/// understanding the uniqueness of values in a large dataset where counting
/// each unique value individually would be computationally expensive.
///
/// For example, you might use a cardinality aggregation to estimate the number
/// of unique visitors to a website by aggregating on a field that contains
/// user IDs or session IDs.
///
/// To use the cardinality aggregation, you'll need to provide a field to
/// aggregate on. The following example demonstrates a request for the cardinality
/// of the "user_id" field:
///
/// ```JSON
/// {
///     "cardinality": {
///         "field": "user_id"
///     }
/// }
/// ```
///
/// This request will return an estimate of the number of unique values in the
/// "user_id" field.
///
/// ## Missing Values
///
/// The `missing` parameter defines how documents that are missing a value should be treated.
/// By default, documents without a value for the specified field are ignored. However, you can
/// specify a default value for these documents using the `missing` parameter. This can be useful
/// when you want to include documents with missing values in the aggregation.
///
/// For example, the following request treats documents with missing values in the "user_id"
/// field as if they had a value of "unknown":
///
/// ```JSON
/// {
///     "cardinality": {
///         "field": "user_id",
///         "missing": "unknown"
///     }
/// }
/// ```
///
/// # Estimation Accuracy
///
/// The cardinality aggregation provides an approximate count, which is usually
/// accurate within a small error range. This trade-off allows for efficient
/// computation even on very large datasets.
///
/// The `precision_threshold` parameter works like in Elasticsearch: counts below it are expected
/// to be close to accurate, and higher thresholds use more memory. It defaults to 3000 and is
/// capped at 40000.
///
/// ```JSON
/// {
///     "cardinality": {
///         "field": "user_id",
///         "precision_threshold": 10000
///     }
/// }
/// ```
///
/// Above the threshold, the relative error is about `1.04 / sqrt(2^lg_k)`, where `lg_k` is
/// derived from the threshold: about 0.6% for the default threshold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardinalityAggregationReq {
    /// The field name to compute the percentiles on.
    pub field: String,
    /// The missing parameter defines how documents that are missing a value should be treated.
    /// By default they will be ignored but it is also possible to treat them as if they had a
    /// value. Examples in JSON format:
    /// { "field": "my_numbers", "missing": "10.0" }
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub missing: Option<Key>,
    /// Count below which the result is expected to be close to accurate. Defaults to 3000.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub precision_threshold: Option<u64>,
}

/// Contains all information required by the segment cardinality collectors to perform the
/// cardinality aggregation on a segment.
pub(crate) struct CardinalityAggReqData {
    /// The column accessor to access the fast field values.
    pub(crate) accessor: Box<dyn ValueSource>,
    /// The missing value normalized to the internal u64 representation of the field type.
    pub(crate) missing_value_for_accessor: Option<u64>,
    /// The name of the aggregation.
    pub(crate) name: String,
    /// The aggregation request.
    pub(crate) req: CardinalityAggregationReq,
}

impl CardinalityAggReqData {
    /// Estimate the memory consumption of this struct in bytes.
    pub fn get_memory_consumption(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl CardinalityAggregationReq {
    /// Creates a new [`CardinalityAggregationReq`] instance from a field name.
    pub fn from_field_name(field_name: String) -> Self {
        Self {
            field: field_name,
            missing: None,
            precision_threshold: None,
        }
    }
    /// Returns the field name the aggregation is computed on.
    pub fn field_name(&self) -> &str {
        &self.field
    }

    /// Returns the log2 of the number of HLL registers used for this request.
    pub(crate) fn lg_k(&self) -> u8 {
        lg_k_for_precision_threshold(
            self.precision_threshold
                .unwrap_or(DEFAULT_PRECISION_THRESHOLD),
        )
    }
}

#[derive(Clone, Debug)]
/// The cardinality collector used during segment collection and for merging results.
/// Uses Apache DataSketches HLL (Hll4, `lg_k` derived from the precision threshold) for compact
/// binary serialization and cross-language compatibility (e.g. Java `datasketches` library).
pub struct CardinalityCollector {
    sketch: HllSketch,
    /// Salt derived from `ColumnType`, used to differentiate values of different column types
    /// that map to the same u64 (e.g. bool `false` = 0 vs i64 `0`).
    /// Not serialized — only needed during insertion, not after sketch registers are populated.
    salt: u8,
}

impl Default for CardinalityCollector {
    fn default() -> Self {
        Self::new(0, lg_k_for_precision_threshold(DEFAULT_PRECISION_THRESHOLD))
    }
}

impl PartialEq for CardinalityCollector {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

impl Serialize for CardinalityCollector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = self.sketch.serialize();
        serializer.serialize_bytes(&bytes)
    }
}

impl<'de> Deserialize<'de> for CardinalityCollector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        let sketch = HllSketch::deserialize(&bytes).map_err(serde::de::Error::custom)?;
        Ok(Self { sketch, salt: 0 })
    }
}

impl CardinalityCollector {
    pub(crate) fn new(salt: u8, lg_k: u8) -> Self {
        Self {
            sketch: HllSketch::new(lg_k, HLL_TYPE).expect("lg_k is within the supported range"),
            salt,
        }
    }

    /// Insert a value into the HLL sketch, salted by the column type.
    /// The salt ensures that identical u64 values from different column types
    /// (e.g. bool `false` vs i64 `0`) are counted as distinct.
    fn insert(&mut self, value: impl Hash) {
        self.sketch.update((self.salt, value));
    }

    fn insert_coupon(&mut self, coupon: Coupon) {
        self.sketch.update_with_coupon(coupon);
    }

    /// Compute the final cardinality estimate.
    pub fn finalize(self) -> Option<f64> {
        Some(self.sketch.estimate().trunc())
    }

    /// Serialize the HLL sketch to its compact binary representation.
    /// The format is cross-language compatible with Apache DataSketches (Java, C++, Python).
    pub fn to_sketch_bytes(&self) -> Vec<u8> {
        self.sketch.serialize()
    }

    pub(crate) fn merge_fruits(&mut self, right: CardinalityCollector) -> crate::Result<()> {
        // Sketches of one request share the same lg_k. They can differ when merging results
        // from nodes running different versions: the union then downsamples to the smaller one.
        let lg_max_k = self.sketch.lg_config_k().max(right.sketch.lg_config_k());
        let mut union = HllUnion::new(lg_max_k).expect("lg_k is within the supported range");
        union.update(&self.sketch);
        union.update(&right.sketch);
        self.sketch = union.to_sketch(HLL_TYPE);
        Ok(())
    }
}

/// Builds the segment collector for a cardinality aggregation.
///
/// str and non-str columns use two entirely different collectors: str
/// accumulates term ordinals and resolves them into HLL coupons at
/// finalization, non-str feeds the HLL sketch directly. Both produce the same
/// [`IntermediateMetricResult::Cardinality`], so they merge uniformly.
pub(crate) fn build_segment_cardinality_collector(
    req_data: CardinalityAggReqData,
) -> crate::Result<Box<dyn SegmentAggregationCollector>> {
    if req_data.accessor.column_type() != ColumnType::Str {
        return Ok(Box::new(SegmentNumericCardinalityCollector::from_req(
            req_data,
        )?));
    }
    // For str columns, we need to collect the set of term ordinals encounterred.
    //
    //
    // In the case of physical columns, the dictionary is known so we have an upperbound
    // of the term ord that will be encounterred.
    //
    // We choose a different representation depending on the number of maximum
    // number of terms.
    //   * small (< BITSET_MAX_TERM_ORD): `BitSet`, pre-allocated.
    //   * large: `TermOrdSet` (sparse HashSet that promotes to a paged bitset).
    Ok(
        if let Some(column) = req_data.accessor.as_physical_column() {
            let max_term_ord_inclusive = column.max_value();
            if max_term_ord_inclusive < BITSET_MAX_TERM_ORD {
                Box::new(SegmentStrCardinalityCollector::<BitSet>::from_req(
                    req_data,
                    max_term_ord_inclusive,
                ))
            } else {
                Box::new(SegmentStrCardinalityCollector::<TermOrdSet>::from_req(
                    req_data,
                    max_term_ord_inclusive,
                ))
            }
        } else {
            // For the moment, for non physical str column, we rely on the regular
            // term ord set.
            // TODO fix me! Although we do not have control over this, these values are likely
            // created using a inc counter, with their unicity given by a hashmap already.
            // We probably do not need another HashSet.
            Box::new(SegmentStrCardinalityCollector::<TermOrdSet>::from_req(
                req_data,
                u64::MAX,
            ))
        },
    )
}

#[cfg(test)]
mod tests {
    use columnar::MonotonicallyMappableToU64;

    use crate::aggregation::agg_req::Aggregations;
    use crate::aggregation::tests::{exec_request, get_test_index_from_terms};
    use crate::schema::{Schema, FAST, STRING};
    use crate::Index;

    #[test]
    fn cardinality_aggregation_test_empty_index() -> crate::Result<()> {
        let values = vec![];
        let index = get_test_index_from_terms(false, &values)?;
        let agg_req: Aggregations = serde_json::from_value(json!({
            "cardinality": {
                "cardinality": {
                    "field": "string_id",
                }
            },
        }))
        .unwrap();

        let res = exec_request(agg_req, &index)?;
        assert_eq!(res["cardinality"]["value"], 0.0);

        Ok(())
    }

    #[test]
    fn cardinality_aggregation_test_single_segment() -> crate::Result<()> {
        cardinality_aggregation_test_merge_segment(true)
    }
    #[test]
    fn cardinality_aggregation_test() -> crate::Result<()> {
        cardinality_aggregation_test_merge_segment(false)
    }
    fn cardinality_aggregation_test_merge_segment(merge_segments: bool) -> crate::Result<()> {
        let segment_and_terms = vec![
            vec!["terma"],
            vec!["termb"],
            vec!["termc"],
            vec!["terma"],
            vec!["terma"],
            vec!["terma"],
            vec!["termb"],
            vec!["terma"],
        ];
        let index = get_test_index_from_terms(merge_segments, &segment_and_terms)?;
        let agg_req: Aggregations = serde_json::from_value(json!({
            "cardinality": {
                "cardinality": {
                    "field": "string_id",
                }
            },
        }))
        .unwrap();

        let res = exec_request(agg_req, &index)?;
        assert_eq!(res["cardinality"]["value"], 3.0);

        Ok(())
    }

    #[test]
    fn cardinality_aggregation_bytes_excluded_from_accessors() -> crate::Result<()> {
        // `Bytes` columns are opened as raw per-segment dictionary ordinals (like `Str`), but
        // unlike `Str`, cardinality has no dictionary-resolution path for them: it would hash
        // the raw ordinal directly, which are segment dependant. Ignore bytes values instead of
        // counting them wrong.
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_bytes_field("raw", FAST);
        let index = Index::create_in_ram(schema_builder.build());
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(field => vec![1u8]))?;
            writer.add_document(doc!(field => vec![2u8]))?;
            writer.commit()?;
            writer.add_document(doc!(field => vec![3u8]))?;
            writer.add_document(doc!(field => vec![4u8]))?;
            writer.commit()?;
        }

        let agg_req: Aggregations = serde_json::from_value(json!({
            "cardinality": {
                "cardinality": {
                    "field": "raw"
                },
            }
        }))
        .unwrap();

        let res = exec_request(agg_req, &index)?;
        assert_eq!(res["cardinality"]["value"], 0.0);

        Ok(())
    }

    #[test]
    fn cardinality_aggregation_json() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_json_field("json", FAST);
        let index = Index::create_in_ram(schema_builder.build());
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(field => json!({"value": false})))?;
            writer.add_document(doc!(field => json!({"value": true})))?;
            writer.add_document(doc!(field => json!({"value": i64::from_u64(0u64)})))?;
            writer.add_document(doc!(field => json!({"value": i64::from_u64(1u64)})))?;
            writer.commit()?;
        }

        let agg_req: Aggregations = serde_json::from_value(json!({
            "cardinality": {
                "cardinality": {
                    "field": "json.value"
                },
            }
        }))
        .unwrap();

        let res = exec_request(agg_req, &index)?;
        assert_eq!(res["cardinality"]["value"], 4.0);

        Ok(())
    }

    /// A JSON path that resolves to both a Str column and a numeric column
    /// produces two collector instances per segment — one with `Str` buckets
    /// and one with `Numeric` buckets. Their `IntermediateMetricResult`s must
    /// merge into the union cardinality.
    #[test]
    fn cardinality_aggregation_json_str_and_numeric() -> crate::Result<()> {
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_json_field("json", FAST);
        let index = Index::create_in_ram(schema_builder.build());
        {
            let mut writer = index.writer_for_tests()?;
            writer.add_document(doc!(field => json!({"value": "hello"})))?;
            writer.add_document(doc!(field => json!({"value": "world"})))?;
            writer.add_document(doc!(field => json!({"value": "hello"})))?; // dup str
            writer.add_document(doc!(field => json!({"value": i64::from_u64(7u64)})))?;
            writer.add_document(doc!(field => json!({"value": i64::from_u64(42u64)})))?;
            writer.add_document(doc!(field => json!({"value": i64::from_u64(7u64)})))?; // dup num
            writer.commit()?;
        }

        let agg_req: Aggregations = serde_json::from_value(json!({
            "cardinality": {
                "cardinality": {
                    "field": "json.value"
                },
            }
        }))
        .unwrap();

        let res = exec_request(agg_req, &index)?;
        // 4 distinct values: "hello", "world", 7, 42.
        assert_eq!(res["cardinality"]["value"], 4.0);

        Ok(())
    }

    #[test]
    fn cardinality_collector_serde_roundtrip() {
        use super::CardinalityCollector;

        let mut collector = CardinalityCollector::default();
        collector.insert("hello");
        collector.insert("world");
        collector.insert("hello"); // duplicate

        let serialized = serde_json::to_vec(&collector).unwrap();
        let deserialized: CardinalityCollector = serde_json::from_slice(&serialized).unwrap();

        let original_estimate = collector.finalize().unwrap();
        let roundtrip_estimate = deserialized.finalize().unwrap();
        assert_eq!(original_estimate, roundtrip_estimate);
        assert_eq!(original_estimate, 2.0);
    }

    #[test]
    fn cardinality_collector_merge() {
        use super::CardinalityCollector;

        let mut left = CardinalityCollector::default();
        left.insert("a");
        left.insert("b");

        let mut right = CardinalityCollector::default();
        right.insert("b");
        right.insert("c");

        left.merge_fruits(right).unwrap();
        let estimate = left.finalize().unwrap();
        assert_eq!(estimate, 3.0);
    }

    /// Verifies that merging two small sketches (both in List/Set coupon mode)
    /// produces an exact result — i.e. the HllUnion does not unnecessarily
    /// promote to the full HLL array when the combined cardinality is small.
    #[test]
    fn cardinality_collector_merge_stays_exact_for_small_sets() {
        use super::CardinalityCollector;

        let mut left = CardinalityCollector::default();
        for i in 0u64..50 {
            left.insert(i);
        }

        let mut right = CardinalityCollector::default();
        for i in 30u64..100 {
            right.insert(i);
        }

        left.merge_fruits(right).unwrap();
        let estimate = left.finalize().unwrap();
        // 100 distinct values (0..100). Both sketches are in Set mode (< 192 coupons),
        // so the union should stay in coupon mode and give an exact count.
        assert_eq!(estimate, 100.0);
    }

    #[test]
    fn cardinality_collector_serialize_deserialize_binary() {
        use datasketches::hll::HllSketch;

        use super::CardinalityCollector;

        let mut collector = CardinalityCollector::default();
        collector.insert("apple");
        collector.insert("banana");
        collector.insert("cherry");

        let bytes = collector.to_sketch_bytes();
        let deserialized = HllSketch::deserialize(&bytes).unwrap();
        assert!((deserialized.estimate() - 3.0).abs() < 0.01);
    }

    /// Tests that the `missing` parameter correctly counts a single empty document
    /// for both u64 and str columns.
    #[test]
    fn cardinality_aggregation_missing_value_single_empty_doc() {
        let mut schema_builder = Schema::builder();
        let id_field = schema_builder.add_u64_field("id", FAST);
        let name_field = schema_builder.add_text_field("name", STRING | FAST);
        let index = Index::create_in_ram(schema_builder.build());
        let mut writer = index.writer_for_tests().unwrap();
        writer
            .add_document(doc!(id_field=>1u64,name_field=>"some_name"))
            .unwrap();
        writer.add_document(doc!()).unwrap();
        writer.commit().unwrap();

        {
            // int colum with missing value non redundant
            let agg_req: Aggregations = serde_json::from_value(json!({
                "cardinality": {
                    "cardinality": {
                        "field": "id",
                        "missing": 42u64
                    },
                }
            }))
            .unwrap();
            let res = exec_request(agg_req, &index).unwrap();
            assert_eq!(res["cardinality"]["value"], 2.0);
        }

        {
            // int colum with missing value redundant
            let agg_req: Aggregations = serde_json::from_value(json!({
                "cardinality": {
                    "cardinality": {
                        "field": "id",
                        "missing": 1u64
                    },
                }
            }))
            .unwrap();
            let res = exec_request(agg_req, &index).unwrap();
            assert_eq!(res["cardinality"]["value"], 1.0);
        }

        {
            // str colum with missing value non redundant
            // With more than one segment, this is not well handled.
            let agg_req: Aggregations = serde_json::from_value(json!({
                "cardinality": {
                    "cardinality": {
                        "field": "name",
                        "missing": "other_name"
                    },
                }
            }))
            .unwrap();
            let res = exec_request(agg_req, &index).unwrap();
            assert_eq!(res["cardinality"]["value"], 2.0);
        }

        {
            // str colum with missing value redundant
            let agg_req: Aggregations = serde_json::from_value(json!({
                "cardinality": {
                    "cardinality": {
                        "field": "name",
                        "missing": "some_name"
                    },
                }
            }))
            .unwrap();
            let res = exec_request(agg_req, &index).unwrap();
            assert_eq!(res["cardinality"]["value"], 1.0);
        }

        {
            // str column with missing value with a number type.
            let agg_req: Aggregations = serde_json::from_value(json!({
                "cardinality": {
                    "cardinality": {
                        "field": "name",
                        "missing": 3,
                    },
                }
            }))
            .unwrap();
            let res = exec_request(agg_req, &index).unwrap();
            assert_eq!(res["cardinality"]["value"], 2.0);
        }
    }

    #[test]
    fn cardinality_collector_salt_differentiates_types() {
        use super::CardinalityCollector;

        // Without salt, same u64 value from different column types would collide
        let mut collector_bool = CardinalityCollector::new(5, 11); // e.g. ColumnType::Bool
        collector_bool.insert(0u64); // false
        collector_bool.insert(1u64); // true

        let mut collector_i64 = CardinalityCollector::new(2, 11); // e.g. ColumnType::I64
        collector_i64.insert(0u64);
        collector_i64.insert(1u64);

        // Merge them
        collector_bool.merge_fruits(collector_i64).unwrap();
        let estimate = collector_bool.finalize().unwrap();
        // Should be 4 because salt makes (5, 0) != (2, 0) and (5, 1) != (2, 1)
        assert_eq!(estimate, 4.0);
    }

    #[test]
    fn cardinality_precision_threshold_to_lg_k() {
        use super::lg_k_for_precision_threshold;

        for (precision_threshold, expected_lg_k) in [
            (0, 4),
            (1, 4),
            (100, 11),
            (192, 11),
            (193, 12),
            (1_000, 14),
            (3_000, 15),
            (3_072, 15),
            (3_073, 16),
            (10_000, 17),
            (40_000, 19),
            (u64::MAX, 19),
        ] {
            assert_eq!(
                lg_k_for_precision_threshold(precision_threshold),
                expected_lg_k,
                "precision_threshold={precision_threshold}"
            );
        }
    }

    #[test]
    fn cardinality_precision_threshold_parsing() {
        use super::CardinalityAggregationReq;

        let req: CardinalityAggregationReq =
            serde_json::from_value(json!({"field": "f", "precision_threshold": 100})).unwrap();
        assert_eq!(req.precision_threshold, Some(100));
        assert_eq!(req.lg_k(), 11);

        let req: CardinalityAggregationReq = serde_json::from_value(json!({"field": "f"})).unwrap();
        assert_eq!(req.precision_threshold, None);
        assert_eq!(req.lg_k(), 15);
        assert_eq!(serde_json::to_value(&req).unwrap(), json!({"field": "f"}));

        let negative_res: Result<CardinalityAggregationReq, _> =
            serde_json::from_value(json!({"field": "f", "precision_threshold": -1}));
        assert!(negative_res.is_err());
    }

    /// Builds an index with one value per doc for `num_values` distinct values, spread over
    /// `num_segments` segments.
    fn index_with_distinct_values(num_values: u64, num_segments: u64) -> crate::Result<Index> {
        let mut schema_builder = Schema::builder();
        let num_field = schema_builder.add_u64_field("num", FAST);
        let str_field = schema_builder.add_text_field("str", STRING | FAST);
        let index = Index::create_in_ram(schema_builder.build());
        let mut writer = index.writer_for_tests()?;
        for segment in 0..num_segments {
            for value in (segment..num_values).step_by(num_segments as usize) {
                writer.add_document(doc!(
                    num_field => value * 7919,
                    str_field => format!("value-{value}"),
                ))?;
            }
            writer.commit()?;
        }
        Ok(index)
    }

    fn cardinalities(index: &Index, precision_threshold: Option<u64>) -> crate::Result<(f64, f64)> {
        let mut num_req = json!({"field": "num"});
        let mut str_req = json!({"field": "str"});
        if let Some(precision_threshold) = precision_threshold {
            num_req["precision_threshold"] = json!(precision_threshold);
            str_req["precision_threshold"] = json!(precision_threshold);
        }
        let agg_req: Aggregations = serde_json::from_value(json!({
            "num": {"cardinality": num_req},
            "str": {"cardinality": str_req},
        }))
        .unwrap();
        let res = exec_request(agg_req, index)?;
        Ok((
            res["num"]["value"].as_f64().unwrap(),
            res["str"]["value"].as_f64().unwrap(),
        ))
    }

    #[test]
    fn cardinality_aggregation_exact_up_to_default_precision_threshold() -> crate::Result<()> {
        for num_values in [100, 1_000, 3_000] {
            let index = index_with_distinct_values(num_values, 3)?;
            let (num, str) = cardinalities(&index, None)?;
            assert_eq!(num, num_values as f64, "num field, {num_values} values");
            assert_eq!(str, num_values as f64, "str field, {num_values} values");
        }
        Ok(())
    }

    #[test]
    fn cardinality_aggregation_precision_threshold_is_used() -> crate::Result<()> {
        let index = index_with_distinct_values(1_000, 3)?;
        // 1000 values don't fit in the coupon mode of a sketch for a threshold of 100: the
        // result is an estimate.
        let (num, str) = cardinalities(&index, Some(100))?;
        assert_ne!(num, 1_000.0);
        assert!((num - 1_000.0).abs() < 100.0, "num={num}");
        assert_ne!(str, 1_000.0);
        assert!((str - 1_000.0).abs() < 100.0, "str={str}");
        // A threshold above the number of values gives an exact count.
        assert_eq!(cardinalities(&index, Some(1_000))?, (1_000.0, 1_000.0));
        Ok(())
    }

    /// Mimics a Quickwit leaf: the intermediate result of each split is serialized, deserialized
    /// and merged, including for a split made of a single merged segment matching all documents.
    #[test]
    fn cardinality_aggregation_merge_serialized_split_results() -> crate::Result<()> {
        use crate::aggregation::intermediate_agg_result::IntermediateAggregationResults;
        use crate::aggregation::{AggregationLimitsGuard, DistributedAggregationCollector};
        use crate::query::AllQuery;
        use crate::IndexWriter;

        let large_split = index_with_distinct_values(20_000, 5)?;
        {
            let mut writer: IndexWriter = large_split.writer_for_tests()?;
            let segment_ids = large_split.searchable_segment_ids()?;
            writer.merge(&segment_ids).wait()?;
            writer.wait_merging_threads()?;
        }
        assert_eq!(large_split.searchable_segment_ids()?.len(), 1);
        let small_split = index_with_distinct_values(4_000, 1)?;

        for precision_threshold in [None, Some(100), Some(40_000)] {
            let mut req = json!({"field": "num"});
            if let Some(precision_threshold) = precision_threshold {
                req["precision_threshold"] = json!(precision_threshold);
            }
            let agg_req: Aggregations =
                serde_json::from_value(json!({"card": {"cardinality": req}})).unwrap();
            let split_result = |index: &Index| -> crate::Result<IntermediateAggregationResults> {
                let collector =
                    DistributedAggregationCollector::from_aggs(agg_req.clone(), Default::default());
                let result = index.reader()?.searcher().search(&AllQuery, &collector)?;
                let bytes = postcard::to_allocvec(&result).unwrap();
                Ok(postcard::from_bytes(&bytes).unwrap())
            };
            let finalize = |result: IntermediateAggregationResults| -> f64 {
                let res = result
                    .into_final_result(agg_req.clone(), AggregationLimitsGuard::default())
                    .unwrap();
                serde_json::to_value(res).unwrap()["card"]["value"]
                    .as_f64()
                    .unwrap()
            };
            let large = finalize(split_result(&large_split)?);
            assert!(
                (large - 20_000.0).abs() < 1_000.0,
                "large split alone: {large} ({precision_threshold:?})"
            );
            // Values 0..4000 of the small split are also in the large split.
            let mut merged = split_result(&large_split)?;
            merged.merge_fruits(split_result(&small_split)?)?;
            let merged = finalize(merged);
            assert!(
                (merged - 20_000.0).abs() < 1_000.0,
                "merged splits: {merged} ({precision_threshold:?})"
            );
        }
        Ok(())
    }

    #[test]
    fn cardinality_collector_merge_different_lg_k() {
        use super::CardinalityCollector;

        let fill = |lg_k: u8, values: std::ops::Range<u64>| {
            let mut collector = CardinalityCollector::new(0, lg_k);
            for value in values {
                collector.insert(value);
            }
            collector
        };
        // Both orders: the union downsamples to the smaller lg_k.
        for (left_lg_k, right_lg_k) in [(11, 15), (15, 11)] {
            let mut left = fill(left_lg_k, 0..10_000);
            left.merge_fruits(fill(right_lg_k, 5_000..15_000)).unwrap();
            assert_eq!(left.sketch.lg_config_k(), 11);
            let estimate = left.finalize().unwrap();
            assert!((estimate - 15_000.0).abs() < 1_000.0, "estimate={estimate}");
        }
        // An empty sketch, e.g. an empty bucket, doesn't lower the precision.
        let mut left = CardinalityCollector::new(0, 11);
        left.merge_fruits(fill(15, 0..3_000)).unwrap();
        assert_eq!(left.sketch.lg_config_k(), 15);
        assert_eq!(left.finalize().unwrap(), 3_000.0);
    }
}
