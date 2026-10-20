//! Read-only source sampling; every encoded file is written under a new disposable directory.
use analytics_storage::parquet_options::writer_properties;
use parquet::{
    arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder},
    basic::Compression,
    file::properties::WriterProperties,
};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().ok_or("output directory required")?);
    fs::create_dir(&root)?;
    for (sample, source) in args.enumerate() {
        let source = PathBuf::from(source);
        let mut reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&source)?)?
            .with_batch_size(16384)
            .build()?;
        let batch = reader.next().ok_or("empty sample")??;
        for codec in ["uncompressed", "snappy", "zstd1"] {
            let path = root.join(format!("sample-{sample}-{codec}.parquet"));
            let props = match codec {
                "uncompressed" => WriterProperties::builder()
                    .set_compression(Compression::UNCOMPRESSED)
                    .build(),
                "snappy" => WriterProperties::builder()
                    .set_compression(Compression::SNAPPY)
                    .build(),
                _ => writer_properties(),
            };
            let started = Instant::now();
            let mut writer =
                ArrowWriter::try_new(fs::File::create(&path)?, batch.schema(), Some(props))?;
            writer.write(&batch)?;
            writer.close()?;
            let write_ms = started.elapsed().as_secs_f64() * 1000.;
            let mut reads = vec![];
            for _ in 0..3 {
                let started = Instant::now();
                let mut reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path)?)?
                    .with_batch_size(batch.num_rows())
                    .build()?;
                let actual = reader.next().ok_or("missing encoded sample")??;
                assert_eq!(actual, batch);
                assert!(reader.next().is_none());
                reads.push(started.elapsed().as_secs_f64() * 1000.);
            }
            reads.sort_by(f64::total_cmp);
            let metadata = fs::metadata(&path)?;
            println!(
                "{}",
                serde_json::json!({"source":source,"rows":batch.num_rows(),"columns":batch.num_columns(),"codec":codec,"bytes":metadata.len(),"allocated_bytes":metadata.blocks()*512,"write_ms":write_ms,"warm_read_median_ms":reads[1],"logical_values_equal":true})
            );
        }
    }
    Ok(())
}
