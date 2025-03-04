//! This module is responsible for compacting Ingester's data

use std::sync::Arc;

use datafusion::{error::DataFusionError, physical_plan::SendableRecordBatchStream};
use iox_query::{
    exec::{Executor, ExecutorType},
    frontend::reorg::ReorgPlanner,
    QueryChunk, QueryChunkMeta,
};
use schema::sort::{adjust_sort_key_columns, compute_sort_key, SortKey};
use snafu::{ResultExt, Snafu};

use crate::{data::partition::PersistingBatch, query::QueryableBatch};

#[derive(Debug, Snafu)]
#[allow(missing_copy_implementations, missing_docs)]
pub(crate) enum Error {
    #[snafu(display("Error while building logical plan for Ingester's compaction"))]
    LogicalPlan {
        source: iox_query::frontend::reorg::Error,
    },

    #[snafu(display("Error while building physical plan for Ingester's compaction"))]
    PhysicalPlan { source: DataFusionError },

    #[snafu(display("Error while executing Ingester's compaction"))]
    ExecutePlan { source: DataFusionError },

    #[snafu(display(
        "Error while building delete predicate from start time, {}, stop time, {}, and serialized \
         predicate, {}",
        min,
        max,
        predicate
    ))]
    DeletePredicate {
        source: predicate::delete_predicate::Error,
        min: String,
        max: String,
        predicate: String,
    },

    #[snafu(display("Could not convert row count to i64"))]
    RowCountTypeConversion { source: std::num::TryFromIntError },

    #[snafu(display("Error computing min and max for record batches: {}", source))]
    MinMax { source: iox_query::util::Error },
}

/// A specialized `Error` for Ingester's Compact errors
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// Result of calling [`compact_persisting_batch`]
pub(crate) struct CompactedStream {
    /// A stream of compacted, deduplicated
    /// [`RecordBatch`](arrow::record_batch::RecordBatch)es
    pub(crate) stream: SendableRecordBatchStream,

    /// The sort key value the catalog should be updated to, if any.
    ///
    /// If returned, the compaction required extending the partition's
    /// [`SortKey`] (typically because new columns were in this parquet file
    /// that were not in previous files).
    pub(crate) catalog_sort_key_update: Option<SortKey>,

    /// The sort key to be used for compaction.
    ///
    /// This should be used in the [`IoxMetadata`] for the compacted data, and
    /// may be a subset of the full sort key contained in
    /// [`Self::catalog_sort_key_update`] (or the existing sort key in the
    /// catalog).
    ///
    /// [`IoxMetadata`]: parquet_file::metadata::IoxMetadata
    pub(crate) data_sort_key: SortKey,
}

impl std::fmt::Debug for CompactedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactedStream")
            .field("stream", &"<SendableRecordBatchStream>")
            .field("data_sort_key", &self.data_sort_key)
            .field("catalog_sort_key_update", &self.catalog_sort_key_update)
            .finish()
    }
}

/// Compact a given persisting batch into a [`CompactedStream`] or
/// `None` if there is no data to compact.
pub(crate) async fn compact_persisting_batch(
    executor: &Executor,
    sort_key: Option<SortKey>,
    batch: Arc<PersistingBatch>,
) -> Result<CompactedStream> {
    assert!(!batch.data.data.is_empty());

    // Get sort key from the catalog or compute it from
    // cardinality.
    let (data_sort_key, catalog_sort_key_update) = match sort_key {
        Some(sk) => {
            // Remove any columns not present in this data from the
            // sort key that will be used to compact this parquet file
            // (and appear in its metadata)
            //
            // If there are any new columns, add them to the end of the sort key in the catalog and
            // return that to be updated in the catalog.
            adjust_sort_key_columns(&sk, &batch.data.schema().primary_key())
        }
        None => {
            let sort_key = compute_sort_key(
                batch.data.schema().as_ref(),
                batch.data.data.iter().map(|sb| sb.data.as_ref()),
            );
            // Use the sort key computed from the cardinality as the sort key for this parquet
            // file's metadata, also return the sort key to be stored in the catalog
            (sort_key.clone(), Some(sort_key))
        }
    };

    // Compact
    let stream = compact(executor, Arc::clone(&batch.data), data_sort_key.clone()).await?;

    Ok(CompactedStream {
        stream,
        catalog_sort_key_update,
        data_sort_key,
    })
}

/// Compact a given Queryable Batch
pub(crate) async fn compact(
    executor: &Executor,
    data: Arc<QueryableBatch>,
    sort_key: SortKey,
) -> Result<SendableRecordBatchStream> {
    // Build logical plan for compaction
    let ctx = executor.new_context(ExecutorType::Reorg);
    let logical_plan = ReorgPlanner::new(ctx.child_ctx("ReorgPlanner"))
        .compact_plan(data.schema(), [data as Arc<dyn QueryChunk>], sort_key)
        .context(LogicalPlanSnafu {})?;

    // Build physical plan
    let physical_plan = ctx
        .create_physical_plan(&logical_plan)
        .await
        .context(PhysicalPlanSnafu {})?;

    // Execute the plan and return the compacted stream
    let output_stream = ctx
        .execute_stream(physical_plan)
        .await
        .context(ExecutePlanSnafu {})?;

    Ok(output_stream)
}

#[cfg(test)]
mod tests {
    use arrow_util::assert_batches_eq;
    use mutable_batch_lp::lines_to_batches;
    use schema::selection::Selection;
    use uuid::Uuid;

    use super::*;
    use crate::test_util::{
        create_batches_with_influxtype, create_batches_with_influxtype_different_cardinality,
        create_batches_with_influxtype_different_columns,
        create_batches_with_influxtype_different_columns_different_order,
        create_batches_with_influxtype_same_columns_different_type,
        create_one_record_batch_with_influxtype_duplicates,
        create_one_record_batch_with_influxtype_no_duplicates,
        create_one_row_record_batch_with_influxtype, make_persisting_batch, make_queryable_batch,
    };

    // this test was added to guard against https://github.com/influxdata/influxdb_iox/issues/3782
    // where if sending in a single row it would compact into an output of two batches, one of
    // which was empty, which would cause this to panic.
    #[tokio::test]
    async fn test_compact_persisting_batch_on_one_record_batch_with_one_row() {
        // create input data
        let batch = lines_to_batches("cpu bar=2 20", 0)
            .unwrap()
            .get("cpu")
            .unwrap()
            .to_arrow(Selection::All)
            .unwrap();
        let batches = vec![Arc::new(batch)];
        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["time"];
        assert_eq!(expected_pk, pk);

        // compact
        let exc = Executor::new(1);
        let CompactedStream { stream, .. } =
            compact_persisting_batch(&exc, Some(SortKey::empty()), persisting_batch)
                .await
                .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on tag1 & time
        let expected_data = vec![
            "+-----+--------------------------------+",
            "| bar | time                           |",
            "+-----+--------------------------------+",
            "| 2   | 1970-01-01T00:00:00.000000020Z |",
            "+-----+--------------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);
    }

    #[tokio::test]
    async fn test_compact_persisting_batch_on_one_record_batch_no_dupilcates() {
        // create input data
        let batches = create_one_record_batch_with_influxtype_no_duplicates().await;

        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "time"];
        assert_eq!(expected_pk, pk);

        // compact
        let exc = Executor::new(1);
        let CompactedStream {
            stream,
            data_sort_key,
            catalog_sort_key_update,
        } = compact_persisting_batch(&exc, Some(SortKey::empty()), persisting_batch)
            .await
            .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on tag1 & time
        let expected_data = vec![
            "+-----------+------+-----------------------------+",
            "| field_int | tag1 | time                        |",
            "+-----------+------+-----------------------------+",
            "| 70        | UT   | 1970-01-01T00:00:00.000020Z |",
            "| 10        | VT   | 1970-01-01T00:00:00.000010Z |",
            "| 1000      | WA   | 1970-01-01T00:00:00.000008Z |",
            "+-----------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);

        assert_eq!(data_sort_key, SortKey::from_columns(["tag1", "time"]));

        assert_eq!(
            catalog_sort_key_update.unwrap(),
            SortKey::from_columns(["tag1", "time"])
        );
    }

    #[tokio::test]
    async fn test_compact_persisting_batch_no_sort_key() {
        // create input data
        let batches = create_batches_with_influxtype_different_cardinality().await;

        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag3", "time"];
        assert_eq!(expected_pk, pk);

        let exc = Executor::new(1);

        // NO SORT KEY from the catalog here, first persisting batch
        let CompactedStream {
            stream,
            data_sort_key,
            catalog_sort_key_update,
        } = compact_persisting_batch(&exc, Some(SortKey::empty()), persisting_batch)
            .await
            .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on the computed sort key of tag1, tag3, & time
        let expected_data = vec![
            "+-----------+------+------+-----------------------------+",
            "| field_int | tag1 | tag3 | time                        |",
            "+-----------+------+------+-----------------------------+",
            "| 70        | UT   | OR   | 1970-01-01T00:00:00.000220Z |",
            "| 50        | VT   | AL   | 1970-01-01T00:00:00.000210Z |",
            "| 10        | VT   | PR   | 1970-01-01T00:00:00.000210Z |",
            "| 1000      | WA   | TX   | 1970-01-01T00:00:00.000028Z |",
            "+-----------+------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);

        assert_eq!(
            data_sort_key,
            SortKey::from_columns(["tag1", "tag3", "time"])
        );

        assert_eq!(
            catalog_sort_key_update.unwrap(),
            SortKey::from_columns(["tag1", "tag3", "time"])
        );
    }

    #[tokio::test]
    async fn test_compact_persisting_batch_with_specified_sort_key() {
        // create input data
        let batches = create_batches_with_influxtype_different_cardinality().await;

        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag3", "time"];
        assert_eq!(expected_pk, pk);

        let exc = Executor::new(1);

        // SPECIFY A SORT KEY HERE to simulate a sort key being stored in the catalog
        // this is NOT what the computed sort key would be based on this data's cardinality
        let CompactedStream {
            stream,
            data_sort_key,
            catalog_sort_key_update,
        } = compact_persisting_batch(
            &exc,
            Some(SortKey::from_columns(["tag3", "tag1", "time"])),
            persisting_batch,
        )
        .await
        .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on the specified sort key of tag3, tag1, &
        // time
        let expected_data = vec![
            "+-----------+------+------+-----------------------------+",
            "| field_int | tag1 | tag3 | time                        |",
            "+-----------+------+------+-----------------------------+",
            "| 50        | VT   | AL   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | UT   | OR   | 1970-01-01T00:00:00.000220Z |",
            "| 10        | VT   | PR   | 1970-01-01T00:00:00.000210Z |",
            "| 1000      | WA   | TX   | 1970-01-01T00:00:00.000028Z |",
            "+-----------+------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);

        assert_eq!(
            data_sort_key,
            SortKey::from_columns(["tag3", "tag1", "time"])
        );

        // The sort key does not need to be updated in the catalog
        assert!(catalog_sort_key_update.is_none());
    }

    #[tokio::test]
    async fn test_compact_persisting_batch_new_column_for_sort_key() {
        // create input data
        let batches = create_batches_with_influxtype_different_cardinality().await;

        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag3", "time"];
        assert_eq!(expected_pk, pk);

        let exc = Executor::new(1);

        // SPECIFY A SORT KEY HERE to simulate a sort key being stored in the catalog
        // this is NOT what the computed sort key would be based on this data's cardinality
        // The new column, tag1, should get added just before the time column
        let CompactedStream {
            stream,
            data_sort_key,
            catalog_sort_key_update,
        } = compact_persisting_batch(
            &exc,
            Some(SortKey::from_columns(["tag3", "time"])),
            persisting_batch,
        )
        .await
        .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on the specified sort key of tag3, tag1, &
        // time
        let expected_data = vec![
            "+-----------+------+------+-----------------------------+",
            "| field_int | tag1 | tag3 | time                        |",
            "+-----------+------+------+-----------------------------+",
            "| 50        | VT   | AL   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | UT   | OR   | 1970-01-01T00:00:00.000220Z |",
            "| 10        | VT   | PR   | 1970-01-01T00:00:00.000210Z |",
            "| 1000      | WA   | TX   | 1970-01-01T00:00:00.000028Z |",
            "+-----------+------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);

        assert_eq!(
            data_sort_key,
            SortKey::from_columns(["tag3", "tag1", "time"])
        );

        // The sort key in the catalog needs to be updated to include the new column
        assert_eq!(
            catalog_sort_key_update.unwrap(),
            SortKey::from_columns(["tag3", "tag1", "time"])
        );
    }

    #[tokio::test]
    async fn test_compact_persisting_batch_missing_column_for_sort_key() {
        // create input data
        let batches = create_batches_with_influxtype_different_cardinality().await;

        // build persisting batch from the input batches
        let uuid = Uuid::new_v4();
        let table_name = "test_table";
        let shard_id = 1;
        let seq_num_start: i64 = 1;
        let table_id = 1;
        let partition_id = 1;
        let persisting_batch = make_persisting_batch(
            shard_id,
            seq_num_start,
            table_id,
            table_name,
            partition_id,
            uuid,
            batches,
        );

        // verify PK
        let schema = persisting_batch.data.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag3", "time"];
        assert_eq!(expected_pk, pk);

        let exc = Executor::new(1);

        // SPECIFY A SORT KEY HERE to simulate a sort key being stored in the catalog
        // this is NOT what the computed sort key would be based on this data's cardinality
        // This contains a sort key, "tag4", that doesn't appear in the data.
        let CompactedStream {
            stream,
            data_sort_key,
            catalog_sort_key_update,
        } = compact_persisting_batch(
            &exc,
            Some(SortKey::from_columns(["tag3", "tag1", "tag4", "time"])),
            persisting_batch,
        )
        .await
        .unwrap();

        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .expect("should execute plan");

        // verify compacted data
        // should be the same as the input but sorted on the specified sort key of tag3, tag1, &
        // time
        let expected_data = vec![
            "+-----------+------+------+-----------------------------+",
            "| field_int | tag1 | tag3 | time                        |",
            "+-----------+------+------+-----------------------------+",
            "| 50        | VT   | AL   | 1970-01-01T00:00:00.000210Z |",
            "| 70        | UT   | OR   | 1970-01-01T00:00:00.000220Z |",
            "| 10        | VT   | PR   | 1970-01-01T00:00:00.000210Z |",
            "| 1000      | WA   | TX   | 1970-01-01T00:00:00.000028Z |",
            "+-----------+------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected_data, &output_batches);

        assert_eq!(
            data_sort_key,
            SortKey::from_columns(["tag3", "tag1", "time"])
        );

        // The sort key in the catalog should NOT get a new value
        assert!(catalog_sort_key_update.is_none());
    }

    #[tokio::test]
    async fn test_compact_one_row_batch() {
        test_helpers::maybe_start_logging();

        // create input data
        let batches = create_one_row_record_batch_with_influxtype().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify no empty record batches - bug #3782
        assert_eq!(output_batches.len(), 1);

        // verify compacted data
        let expected = vec![
            "+-----------+------+-----------------------------+",
            "| field_int | tag1 | time                        |",
            "+-----------+------+-----------------------------+",
            "| 1000      | MA   | 1970-01-01T00:00:00.000001Z |",
            "+-----------+------+-----------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);
    }

    #[tokio::test]
    async fn test_compact_one_batch_with_duplicates() {
        // create input data
        let batches = create_one_record_batch_with_influxtype_duplicates().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();
        // verify no empty record bacthes - bug #3782
        assert_eq!(output_batches.len(), 2);
        assert_eq!(output_batches[0].num_rows(), 6);
        assert_eq!(output_batches[1].num_rows(), 1);

        // verify compacted data
        //  data is sorted and all duplicates are removed
        let expected = vec![
            "+-----------+------+--------------------------------+",
            "| field_int | tag1 | time                           |",
            "+-----------+------+--------------------------------+",
            "| 10        | AL   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 70        | CT   | 1970-01-01T00:00:00.000000500Z |",
            "| 30        | MT   | 1970-01-01T00:00:00.000000005Z |",
            "| 1000      | MT   | 1970-01-01T00:00:00.000001Z    |",
            "| 1000      | MT   | 1970-01-01T00:00:00.000002Z    |",
            "| 20        | MT   | 1970-01-01T00:00:00.000007Z    |",
            "+-----------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);
    }

    #[tokio::test]
    async fn test_compact_many_batches_same_columns_with_duplicates() {
        // create many-batches input data
        let batches = create_batches_with_influxtype().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify compacted data
        // data is sorted and all duplicates are removed
        let expected = vec![
            "+-----------+------+--------------------------------+",
            "| field_int | tag1 | time                           |",
            "+-----------+------+--------------------------------+",
            "| 100       | AL   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 70        | CT   | 1970-01-01T00:00:00.000000500Z |",
            "| 30        | MT   | 1970-01-01T00:00:00.000000005Z |",
            "| 1000      | MT   | 1970-01-01T00:00:00.000001Z    |",
            "| 1000      | MT   | 1970-01-01T00:00:00.000002Z    |",
            "| 5         | MT   | 1970-01-01T00:00:00.000005Z    |",
            "| 10        | MT   | 1970-01-01T00:00:00.000007Z    |",
            "+-----------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);
    }

    #[tokio::test]
    async fn test_compact_many_batches_different_columns_with_duplicates() {
        // create many-batches input data
        let batches = create_batches_with_influxtype_different_columns().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag2", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "tag2", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify compacted data
        // data is sorted and all duplicates are removed
        let expected = vec![
            "+-----------+------------+------+------+--------------------------------+",
            "| field_int | field_int2 | tag1 | tag2 | time                           |",
            "+-----------+------------+------+------+--------------------------------+",
            "| 10        |            | AL   |      | 1970-01-01T00:00:00.000000050Z |",
            "| 100       | 100        | AL   | MA   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        |            | CT   |      | 1970-01-01T00:00:00.000000100Z |",
            "| 70        |            | CT   |      | 1970-01-01T00:00:00.000000500Z |",
            "| 70        | 70         | CT   | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 30        |            | MT   |      | 1970-01-01T00:00:00.000000005Z |",
            "| 1000      |            | MT   |      | 1970-01-01T00:00:00.000001Z    |",
            "| 1000      |            | MT   |      | 1970-01-01T00:00:00.000002Z    |",
            "| 20        |            | MT   |      | 1970-01-01T00:00:00.000007Z    |",
            "| 5         | 5          | MT   | AL   | 1970-01-01T00:00:00.000005Z    |",
            "| 10        | 10         | MT   | AL   | 1970-01-01T00:00:00.000007Z    |",
            "| 1000      | 1000       | MT   | CT   | 1970-01-01T00:00:00.000001Z    |",
            "+-----------+------------+------+------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &output_batches);
    }

    #[tokio::test]
    async fn test_compact_many_batches_different_columns_different_order_with_duplicates() {
        // create many-batches input data
        let batches = create_batches_with_influxtype_different_columns_different_order().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag2", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "tag2", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify compacted data
        // data is sorted and all duplicates are removed
        // CORRECT RESULT
        let expected = vec![
            "+-----------+------+------+--------------------------------+",
            "| field_int | tag1 | tag2 | time                           |",
            "+-----------+------+------+--------------------------------+",
            "| 5         |      | AL   | 1970-01-01T00:00:00.000005Z    |",
            "| 10        |      | AL   | 1970-01-01T00:00:00.000007Z    |",
            "| 70        |      | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 1000      |      | CT   | 1970-01-01T00:00:00.000001Z    |",
            "| 100       |      | MA   | 1970-01-01T00:00:00.000000050Z |",
            "| 10        | AL   | MA   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        | CT   | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 70        | CT   | CT   | 1970-01-01T00:00:00.000000500Z |",
            "| 30        | MT   | AL   | 1970-01-01T00:00:00.000000005Z |",
            "| 20        | MT   | AL   | 1970-01-01T00:00:00.000007Z    |",
            "| 1000      | MT   | CT   | 1970-01-01T00:00:00.000001Z    |",
            "| 1000      | MT   | CT   | 1970-01-01T00:00:00.000002Z    |",
            "+-----------+------+------+--------------------------------+",
        ];

        assert_batches_eq!(&expected, &output_batches);
    }

    // BUG
    #[tokio::test]
    async fn test_compact_many_batches_different_columns_different_order_with_duplicates2() {
        // create many-batches input data
        let batches = create_batches_with_influxtype_different_columns_different_order().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // verify PK
        let schema = compact_batch.schema();
        let pk = schema.primary_key();
        let expected_pk = vec!["tag1", "tag2", "time"];
        assert_eq!(expected_pk, pk);

        let sort_key = compute_sort_key(
            &schema,
            compact_batch.data.iter().map(|sb| sb.data.as_ref()),
        );
        assert_eq!(sort_key, SortKey::from_columns(["tag1", "tag2", "time"]));

        // compact
        let exc = Executor::new(1);
        let stream = compact(&exc, compact_batch, sort_key).await.unwrap();
        let output_batches = datafusion::physical_plan::common::collect(stream)
            .await
            .unwrap();

        // verify compacted data
        // data is sorted and all duplicates are removed
        let expected = vec![
            "+-----------+------+------+--------------------------------+",
            "| field_int | tag1 | tag2 | time                           |",
            "+-----------+------+------+--------------------------------+",
            "| 5         |      | AL   | 1970-01-01T00:00:00.000005Z    |",
            "| 10        |      | AL   | 1970-01-01T00:00:00.000007Z    |",
            "| 70        |      | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 1000      |      | CT   | 1970-01-01T00:00:00.000001Z    |",
            "| 100       |      | MA   | 1970-01-01T00:00:00.000000050Z |",
            "| 10        | AL   | MA   | 1970-01-01T00:00:00.000000050Z |",
            "| 70        | CT   | CT   | 1970-01-01T00:00:00.000000100Z |",
            "| 70        | CT   | CT   | 1970-01-01T00:00:00.000000500Z |",
            "| 30        | MT   | AL   | 1970-01-01T00:00:00.000000005Z |",
            "| 20        | MT   | AL   | 1970-01-01T00:00:00.000007Z    |",
            "| 1000      | MT   | CT   | 1970-01-01T00:00:00.000001Z    |",
            "| 1000      | MT   | CT   | 1970-01-01T00:00:00.000002Z    |",
            "+-----------+------+------+--------------------------------+",
        ];

        assert_batches_eq!(&expected, &output_batches);
    }

    #[tokio::test]
    #[should_panic(expected = "Schemas compatible")]
    async fn test_compact_many_batches_same_columns_different_types() {
        // create many-batches input data
        let batches = create_batches_with_influxtype_same_columns_different_type().await;

        // build queryable batch from the input batches
        let compact_batch = make_queryable_batch("test_table", 0, 1, batches);

        // the schema merge will thorw a panic
        compact_batch.schema();
    }
}
