use std::sync::Arc;
use std::time::Duration;

use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion_uwheel::UWheelOptimizer;

use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use datafusion::arrow::array::AsArray;
use datafusion::arrow::datatypes::Int64Type;
use datafusion::error::Result;
use datafusion::prelude::*;
use datafusion_uwheel::builder::Builder;
use hdrhistogram::Histogram;
use minstant::Instant;

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(short, long, value_parser, default_value_t = 1000)]
    queries: usize,
}

/// This benchmark compares regular "datafusion" with "datafusion_uwheel" using the NYC Taxi dataset.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    println!("Running with {:#?}", args);

    // Create "normal" DataFusion context

    // create local session context
    let ctx = SessionContext::new();

    let filename = "../../data/yellow_tripdata_2022-01.parquet";

    // register parquet file with the execution context
    ctx.register_parquet("yellow_tripdata", &filename, ParquetReadOptions::default())
        .await?;

    // Create ctx with UWheelOptimizer

    let uwheel_ctx = SessionContext::new();
    let table_path = "../../data/";

    // Parse the path
    let table_path = ListingTableUrl::parse(table_path)?;

    // Create default parquet options
    let file_format = ParquetFormat::new();
    let listing_options =
        ListingOptions::new(Arc::new(file_format)).with_file_extension(".parquet");

    // Resolve the schema
    let resolved_schema = listing_options
        .infer_schema(&uwheel_ctx.state(), &table_path)
        .await?;

    let config = ListingTableConfig::new(table_path)
        .with_listing_options(listing_options)
        .with_schema(resolved_schema);

    // Create a new TableProvider
    let provider = Arc::new(ListingTable::try_new(config)?);

    // Build a provider over parquet files in data/ using the time column "tpep_dropoff_datetime"
    let optimizer: Arc<UWheelOptimizer> = Arc::new(
        Builder::new("tpep_dropoff_datetime")
            .with_name("yellow_tripdata")
            .with_min_max_wheels(vec!["fare_amount", "trip_distance"]) // Create Min/Max wheels for the columns "fare_amount" and "trip_distance"
            .build_with_provider(provider)
            .await
            .unwrap(),
    );

    // Set UWheelOptimizer as the query planner
    let session_state = uwheel_ctx.state().with_query_planner(optimizer.clone());
    let uwheel_ctx = SessionContext::new_with_state(session_state);

    // Register the table using the underlying provider
    uwheel_ctx
        .register_table("yellow_tripdata", optimizer.provider())
        .unwrap();

    let start_date = NaiveDate::from_ymd_opt(2022, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();

    let end_date = NaiveDate::from_ymd_opt(2022, 1, 31)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();

    println!("===== MINUTE RANGES =====");
    let min_ranges = generate_minute_time_ranges(start_date, end_date, args.queries);

    bench_fare_datafusion("datafusion", &ctx, &min_ranges).await;
    bench_fare_datafusion("datafusion-uwheel", &uwheel_ctx, &min_ranges).await;

    println!("===== HOUR RANGES =====");

    let hr_ranges = generate_hour_time_ranges(start_date, end_date, args.queries);
    bench_fare_datafusion("datafusion", &ctx, &hr_ranges).await;
    bench_fare_datafusion("datafusion-uwheel", &uwheel_ctx, &hr_ranges).await;

    Ok(())
}

pub fn generate_minute_time_ranges(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    count: usize,
) -> Vec<(u64, u64)> {
    // Calculate total minutes within the date range
    let total_minutes = (end - start).num_minutes() as u64;

    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        // Randomly select start and end minutes
        let start_minute = fastrand::u64(0..total_minutes - 1); // exclude last min
        let end_minute = fastrand::u64(start_minute + 1..total_minutes);

        // Construct DateTime objects with minute alignment
        let start_time = start + chrono::Duration::minutes(start_minute as i64);
        let end_time = start + chrono::Duration::minutes(end_minute as i64);

        ranges.push((
            start_time.timestamp_millis() as u64,
            end_time.timestamp_millis() as u64,
        ));
    }
    ranges
}

pub fn generate_hour_time_ranges(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    count: usize,
) -> Vec<(u64, u64)> {
    // Calculate total hours within the date range
    let total_hours = (end - start).num_hours() as u64;

    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        // Randomly select start and end hours
        let start_hour = fastrand::u64(0..total_hours - 1); // exclude last hour
        let end_hour = fastrand::u64(start_hour + 1..total_hours);

        // Construct DateTime objects with minute alignment
        let start_time = start + chrono::Duration::minutes(start_hour as i64);
        let end_time = start + chrono::Duration::minutes(end_hour as i64);

        ranges.push((
            start_time.timestamp_millis() as u64,
            end_time.timestamp_millis() as u64,
        ));
    }
    ranges
}

async fn bench_fare_datafusion(id: &str, ctx: &SessionContext, ranges: &[(u64, u64)]) {
    let queries: Vec<_> = ranges
        .iter()
        .copied()
        .map(|(start, end)| {
            let start = DateTime::from_timestamp_millis(start as i64)
                .unwrap()
                .to_utc()
                .naive_utc()
                .to_string();
            let end = DateTime::from_timestamp_millis(end as i64)
                .unwrap()
                .to_utc()
                .naive_utc()
                .to_string();
            format!(
                "SELECT COUNT(*) FROM yellow_tripdata \
            WHERE tpep_dropoff_datetime >= '{}' \
            AND tpep_dropoff_datetime < '{}'",
                start, end
            )
        })
        .collect();
    let mut hist = hdrhistogram::Histogram::<u64>::new(4).unwrap();
    let full = Instant::now();

    for query in queries {
        // dbg!(&query);
        let now = Instant::now();
        let df = ctx.sql(&query).await.unwrap();
        let res = df.collect().await.unwrap();
        hist.record(now.elapsed().as_micros() as u64).unwrap();
        let _count: i64 = res[0]
            .project(&[0])
            .unwrap()
            .column(0)
            .as_primitive::<Int64Type>()
            .value(0);
        #[cfg(feature = "debug")]
        dbg!(_count);
    }
    let runtime = full.elapsed();

    println!(
        "{:?} Executed {} queries with {:.2}QPS took {:?}",
        id,
        ranges.len(),
        (ranges.len() as f64 / runtime.as_secs_f64()),
        runtime
    );

    print_hist(id, &hist);
}

fn print_hist(id: &str, hist: &Histogram<u64>) {
    println!(
        "{} latencies:\t\tmin: {: >4}µs\tp50: {: >4}µs\tp99: {: \
         >4}µs\tp99.9: {: >4}µs\tp99.99: {: >4}µs\tp99.999: {: >4}µs\t max: {: >4}µs \t count: {}",
        id,
        Duration::from_micros(hist.min()).as_micros(),
        Duration::from_micros(hist.value_at_quantile(0.5)).as_micros(),
        Duration::from_micros(hist.value_at_quantile(0.99)).as_micros(),
        Duration::from_micros(hist.value_at_quantile(0.999)).as_micros(),
        Duration::from_micros(hist.value_at_quantile(0.9999)).as_micros(),
        Duration::from_micros(hist.value_at_quantile(0.99999)).as_micros(),
        Duration::from_micros(hist.max()).as_micros(),
        hist.len(),
    );
}
