//! Common production encoding. Existing files and logical schemas are unchanged.
use parquet::{
    basic::{Compression, ZstdLevel},
    file::properties::WriterProperties,
};

/// Fast lossless compression for every production Rust Parquet writer.
pub fn writer_properties() -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(1).expect("valid Zstd level"),
        ))
        .build()
}
