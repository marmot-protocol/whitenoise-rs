//! Serde helpers for serializing `Duration` as nanosecond `u128` values.
//!
//! [`as_nanos`] calls [`Duration::as_nanos`] and forwards the result to
//! [`Serializer::serialize_u128`], producing a JSON number that covers the full
//! nanosecond range without truncation.
//!
//! Used by `#[serde(serialize_with = "...")]` on `PerfBreakdown`,
//! `PerfSample`, and `BenchmarkResult` fields.

use std::time::Duration;

use serde::Serializer;

pub fn as_nanos<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_u128(duration.as_nanos())
}
