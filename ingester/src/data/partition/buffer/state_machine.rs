#![allow(dead_code)]
use std::sync::Arc;

use arrow::record_batch::RecordBatch;
use data_types::SequenceNumber;
use mutable_batch::MutableBatch;

mod buffering;
mod persisting;
mod snapshot;

pub(in crate::data::partition::buffer) use buffering::*;
pub(crate) use persisting::*;

use crate::data::SequenceNumberRange;

use super::traits::{Queryable, Writeable};

/// A result type for fallible transitions.
///
/// The type system ensures the state machine is always returned to the caller,
/// regardless of the transition outcome.
#[derive(Debug)]
pub(crate) enum Transition<A, B> {
    /// The transition succeeded, and the new state is contained within.
    Ok(BufferState<A>),
    /// The state machine failed to transition due to an invariant not being
    /// upheld, and the original state is contained within.
    Unchanged(BufferState<B>),
}

impl<A, B> Transition<A, B> {
    /// A helper function to construct [`Self::Ok`] variants.
    pub(super) fn ok(v: A, sequence_range: SequenceNumberRange) -> Transition<A, B> {
        Self::Ok(BufferState {
            state: v,
            sequence_range,
        })
    }

    /// A helper function to construct [`Self::Unchanged`] variants.
    pub(super) fn unchanged(v: BufferState<B>) -> Transition<A, B> {
        Self::Unchanged(v)
    }
}

/// A finite state machine for buffering writes, and converting them into a
/// queryable data format on-demand.
///
/// This FSM is used to provide explicit states for each stage of the data
/// lifecycle within a partition buffer:
///
/// ```text
///                  ┌──────────────┐
///                  │  Buffering   │
///                  └───────┬──────┘
///                          │
///                          ▼
///                  ┌ ─ ─ ─ ─ ─ ─ ─       ┌ ─ ─ ─ ─ ─ ─ ─
///                      Snapshot   ├─────▶   Persisting  │
///                  └ ─ ─ ─ ─ ─ ─ ─       └ ─ ─ ─ ─ ─ ─ ─
/// ```
///
/// Boxes with dashed lines indicate immutable, queryable states that contain
/// data in an efficient data format for query execution ([`RecordBatch`]).
///
/// Boxes with solid lines indicate a mutable state to which further writes can
/// be applied.
///
/// A [`BufferState`] tracks the bounding [`SequenceNumber`] values it has
/// observed, and enforces monotonic writes (w.r.t their [`SequenceNumber`]).
#[derive(Debug)]
pub(crate) struct BufferState<T> {
    state: T,
    sequence_range: SequenceNumberRange,
}

impl BufferState<Buffering> {
    /// Initialise a new buffer state machine.
    pub(super) fn new() -> Self {
        Self {
            state: Buffering::default(),
            sequence_range: SequenceNumberRange::default(),
        }
    }
}

impl<T> BufferState<T> {
    pub fn sequence_number_range(&self) -> &SequenceNumberRange {
        &self.sequence_range
    }
}

/// A [`BufferState`] in a mutable state can accept writes and record their
/// [`SequenceNumber`].
impl<T> BufferState<T>
where
    T: Writeable,
{
    /// The provided [`SequenceNumber`] MUST be for the given [`MutableBatch`].
    ///
    /// # Panics
    ///
    /// This method panics if it is called non-monotonic writes/sequence
    /// numbers.
    pub(crate) fn write(
        &mut self,
        batch: MutableBatch,
        n: SequenceNumber,
    ) -> Result<(), mutable_batch::Error> {
        self.state.write(batch)?;
        self.sequence_range.observe(n);
        Ok(())
    }
}

/// A [`BufferState`] in a queryable state delegates the read to the current
/// state machine state.
impl<T> Queryable for BufferState<T>
where
    T: Queryable,
{
    /// Returns the current buffer data.
    ///
    /// This is always a cheap method call.
    fn get_query_data(&self) -> Vec<Arc<RecordBatch>> {
        self.state.get_query_data()
    }
}

#[cfg(test)]
mod tests {
    use snapshot::*;
    use std::ops::Deref;

    use arrow_util::assert_batches_eq;
    use mutable_batch_lp::test_helpers::lp_to_mutable_batch;
    use schema::selection::Selection;

    use super::*;

    #[test]
    fn test_buffer_lifecycle() {
        // Initialise a buffer in the base state.
        let mut buffer: BufferState<Buffering> = BufferState::new();

        // Validate the sequence number ranges are not populated.
        assert!(buffer.sequence_number_range().inclusive_min().is_none());
        assert!(buffer.sequence_number_range().inclusive_max().is_none());

        // Write some data to a buffer.
        buffer
            .write(
                lp_to_mutable_batch(
                    r#"bananas,tag=platanos great=true,how_much=42 668563242000000042"#,
                )
                .1,
                SequenceNumber::new(0),
            )
            .expect("write to empty buffer should succeed");

        // Extract the queryable data from the buffer and validate it.
        //
        // Keep the data to validate they are ref-counted copies after further
        // writes below. Note this construct allows the caller to decide when/if
        // to allocate.
        let w1_data = buffer.get_query_data().to_owned();

        let expected = vec![
            "+-------+----------+----------+--------------------------------+",
            "| great | how_much | tag      | time                           |",
            "+-------+----------+----------+--------------------------------+",
            "| true  | 42       | platanos | 1991-03-10T00:00:42.000000042Z |",
            "+-------+----------+----------+--------------------------------+",
        ];
        assert_batches_eq!(&expected, &[w1_data[0].deref().clone()]);

        // Apply another write.
        buffer
            .write(
                lp_to_mutable_batch(
                    r#"bananas,tag=platanos great=true,how_much=1000 668563242000000043"#,
                )
                .1,
                SequenceNumber::new(1),
            )
            .expect("write to empty buffer should succeed");

        // Snapshot the buffer into an immutable, queryable data format.
        let buffer: BufferState<Snapshot> = match buffer.snapshot() {
            Transition::Ok(v) => v,
            Transition::Unchanged(_) => panic!("did not transition to snapshot state"),
        };

        // Verify the writes are still queryable.
        let w2_data = buffer.get_query_data().to_owned();
        let expected = vec![
            "+-------+----------+----------+--------------------------------+",
            "| great | how_much | tag      | time                           |",
            "+-------+----------+----------+--------------------------------+",
            "| true  | 42       | platanos | 1991-03-10T00:00:42.000000042Z |",
            "| true  | 1000     | platanos | 1991-03-10T00:00:42.000000043Z |",
            "+-------+----------+----------+--------------------------------+",
        ];
        assert_eq!(w2_data.len(), 1);
        assert_batches_eq!(&expected, &[w2_data[0].deref().clone()]);

        // Ensure the same data is returned for a second read.
        {
            let second_read = buffer.get_query_data();
            assert_eq!(w2_data, second_read);

            // And that no data was actually copied.
            let same_arcs = w2_data
                .iter()
                .zip(second_read.iter())
                .all(|(a, b)| Arc::ptr_eq(a, &b));
            assert!(same_arcs);
        }

        // Finally transition into the terminal persisting state.
        let buffer: BufferState<Persisting> = buffer.into_persisting();

        // Validate the sequence number ranges were updated as writes occurred.
        assert_eq!(
            buffer.sequence_number_range().inclusive_min(),
            Some(SequenceNumber::new(0))
        );
        assert_eq!(
            buffer.sequence_number_range().inclusive_max(),
            Some(SequenceNumber::new(1))
        );

        // Extract the final buffered result
        let final_data = buffer.into_data();

        // And once again verify no data was changed, copied or re-ordered.
        assert_eq!(w2_data, final_data);
        let same_arcs = w2_data
            .into_iter()
            .zip(final_data.into_iter())
            .all(|(a, b)| Arc::ptr_eq(&a, &b));
        assert!(same_arcs);
    }

    #[test]
    fn test_snapshot_buffer_different_but_compatible_schemas() {
        let mut buffer = BufferState::new();

        // Missing tag `t1`
        let (_, mut mb1) = lp_to_mutable_batch(r#"foo iv=1i,uv=774u,fv=1.0,bv=true,sv="hi" 1"#);
        buffer.state.write(mb1.clone()).unwrap();

        // Missing field `iv`
        let (_, mb2) = lp_to_mutable_batch(r#"foo,t1=aoeu uv=1u,fv=12.0,bv=false,sv="bye" 10000"#);
        buffer.state.write(mb2.clone()).unwrap();

        let buffer: BufferState<Snapshot> = match buffer.snapshot() {
            Transition::Ok(v) => v,
            Transition::Unchanged(_) => panic!("failed to transition"),
        };

        assert_eq!(buffer.get_query_data().len(), 1);

        let snapshot = &buffer.get_query_data()[0];

        // Generate the combined buffer from the original inputs to compare
        // against.
        mb1.extend_from(&mb2).unwrap();
        let want = mb1.to_arrow(Selection::All).unwrap();

        assert_eq!(&**snapshot, &want);
    }
}
