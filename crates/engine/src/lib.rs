//! DuckDB access isolated behind the [`QueryEngine`] trait.
//!
//! The UI and pipeline never touch DuckDB directly. The default build has no
//! native dependency; enable `duckdb-native` for a system (or downloaded)
//! library and `duckdb-bundled` for a source build with the Parquet, JSON,
//! and ICU extensions. The native backend additionally exposes
//! `DuckDbEngine::load_market_rows`, a worker-local loader that joins
//! historical universe membership as-of each market date and returns
//! normalized [`analytics_core::MarketRow`] values for factor execution.
//!
//! The DuckDB connection is not `Sync` by design: create and use it on one
//! worker thread only.
#![warn(missing_docs)]

use analytics_core::{ColumnSchema, DatasetFile, ExportRequest, PageRequest, QueryPage};
use std::path::Path;
use thiserror::Error;

/// Errors returned by the SQL and market-loading boundary.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Built without a DuckDB backend; enable `duckdb-native` or
    /// `duckdb-bundled`.
    #[error("DuckDB backend is not enabled; build with `duckdb-native` or `duckdb-bundled`")]
    BackendUnavailable,
    /// Structurally invalid request (bad page size, invalid identifier, ...).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Error reported by DuckDB itself.
    #[error("DuckDB error: {0}")]
    DuckDb(String),
}

/// Convenience result alias for engine calls.
pub type Result<T> = std::result::Result<T, EngineError>;

/// SQL surface used by the UI and pipeline.
///
/// All methods take `&mut self` because a DuckDB connection is single
/// threaded; implementations are expected to live on one worker thread.
pub trait QueryEngine {
    /// Lists Parquet files under `root` without reading row data.
    fn list_parquet(&mut self, root: &Path) -> Result<Vec<DatasetFile>>;
    /// Returns the result column schemas of `sql` by executing it with
    /// `LIMIT 0`.
    fn describe(&mut self, sql: &str) -> Result<Vec<ColumnSchema>>;
    /// Fetches one page of `request` using keyset (cursor) pagination:
    /// sort keys are validated as identifiers, cursor values become a
    /// lexicographic predicate, and no `OFFSET` is generated.
    fn query_page(&mut self, request: PageRequest) -> Result<QueryPage>;
    /// Streams the full result of `request.sql` into `request.destination`
    /// as Parquet (ZSTD) or CSV via DuckDB `COPY`.
    fn export(&mut self, request: ExportRequest) -> Result<()>;
}

#[cfg(feature = "duckdb-native")]
mod duckdb_backend {
    use super::*;
    use analytics_core::{DataType, DateRange, KeysetCursor, MarketRow, ScalarValue};
    use chrono::NaiveDate;
    use duckdb::Connection;
    use std::fs;

    /// DuckDB-backed [`QueryEngine`] over one worker-local connection.
    pub struct DuckDbEngine {
        connection: Connection,
        query_snapshot: Option<String>,
        snapshot_generation: u64,
    }

    impl DuckDbEngine {
        /// Opens an in-memory database.
        pub fn open_in_memory() -> Result<Self> {
            Connection::open_in_memory()
                .map(|connection| Self {
                    connection,
                    query_snapshot: None,
                    snapshot_generation: 0,
                })
                .map_err(|error| EngineError::DuckDb(error.to_string()))
        }

        /// Resolve relative query input files against a fixed workspace root.
        pub fn set_file_root(&mut self, root: &Path) -> Result<()> {
            let path = root.to_string_lossy().replace('\'', "''");
            self.connection
                .execute_batch(&format!("SET file_search_path = '{path}'"))
                .map_err(|e| EngineError::DuckDb(e.to_string()))
        }

        /// Opens (or creates) a database file at `path`.
        pub fn open(path: impl AsRef<Path>) -> Result<Self> {
            Connection::open(path)
                .map(|connection| Self {
                    connection,
                    query_snapshot: None,
                    snapshot_generation: 0,
                })
                .map_err(|error| EngineError::DuckDb(error.to_string()))
        }

        /// Replaces the connection-local query snapshot with the result of `sql`.
        /// Returns a SELECT over the temporary table; source expressions are
        /// evaluated once. The previous snapshot is removed even if SQL fails.
        pub fn replace_query_snapshot(&mut self, sql: &str) -> Result<String> {
            self.clear_query_snapshot()?;
            self.snapshot_generation =
                self.snapshot_generation.checked_add(1).ok_or_else(|| {
                    EngineError::InvalidRequest("query snapshot generation exhausted".into())
                })?;
            let table = format!("__jfquant_snapshot_{}", self.snapshot_generation);
            self.connection
                .execute_batch(&format!(
                    "CREATE TEMP TABLE {table} AS SELECT * FROM ({sql}) AS snapshot_source"
                ))
                .map_err(|error| EngineError::DuckDb(error.to_string()))?;
            self.query_snapshot = Some(table.clone());
            Ok(format!("SELECT * FROM {table}"))
        }

        /// Drops the active query snapshot on this connection's worker thread.
        pub fn clear_query_snapshot(&mut self) -> Result<()> {
            if let Some(table) = &self.query_snapshot {
                self.connection
                    .execute_batch(&format!("DROP TABLE IF EXISTS {table}"))
                    .map_err(|error| EngineError::DuckDb(error.to_string()))?;
                self.query_snapshot = None;
            }
            Ok(())
        }

        /// Loads normalized market rows after an as-of historical-universe
        /// join. The connection and all DuckDB handles remain on this worker.
        pub fn load_market_rows(
            &mut self,
            market_path: &Path,
            universe_path: &Path,
            universe_id: &str,
            date_range: &DateRange,
        ) -> Result<Vec<MarketRow>> {
            date_range
                .validate()
                .map_err(|error| EngineError::InvalidRequest(error.to_string()))?;
            let market = parquet_source(market_path);
            let universe = parquet_source(universe_path);
            let sql = format!(
                "WITH joined AS (
                    SELECT m.*, u.is_member,
                        row_number() OVER (PARTITION BY m.instrument_id, m.trade_date
                            ORDER BY u.trade_date DESC) AS membership_rank
                    FROM read_parquet('{market}', union_by_name=true) m
                    JOIN read_parquet('{universe}', union_by_name=true) u
                      ON u.instrument_id = m.instrument_id
                     AND u.universe_id = ?
                     AND u.trade_date <= m.trade_date
                    WHERE m.trade_date BETWEEN ? AND ?
                )
                SELECT instrument_id, CAST(trade_date AS VARCHAR),
                    open, high, low, close, volume, amount,
                    COALESCE(adj_open, open), COALESCE(adj_high, high),
                    COALESCE(adj_low, low), COALESCE(adj_close, close),
                    is_trading, CAST(listing_date AS VARCHAR)
                FROM joined
                WHERE membership_rank = 1 AND is_member
                ORDER BY trade_date, instrument_id",
                market = escape_sql_literal(&market),
                universe = escape_sql_literal(&universe),
            );
            let mut statement = self
                .connection
                .prepare(&sql)
                .map_err(|error| EngineError::DuckDb(error.to_string()))?;
            let start = date_range.start.to_string();
            let end = date_range.end.to_string();
            let mut rows = statement
                .query(duckdb::params![universe_id, start, end])
                .map_err(|error| EngineError::DuckDb(error.to_string()))?;
            let mut output = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|error| EngineError::DuckDb(error.to_string()))?
            {
                macro_rules! get {
                    ($index:expr, $ty:ty) => {
                        row.get::<_, $ty>($index)
                            .map_err(|error| EngineError::DuckDb(error.to_string()))?
                    };
                }
                let trade_date = parse_date(get!(1, String))?;
                let listing_date = get!(13, Option<String>).map(parse_date).transpose()?;
                output.push(MarketRow {
                    instrument_id: get!(0, String),
                    trade_date,
                    open: get!(2, Option<f64>),
                    high: get!(3, Option<f64>),
                    low: get!(4, Option<f64>),
                    close: get!(5, Option<f64>),
                    volume: get!(6, Option<f64>),
                    amount: get!(7, Option<f64>),
                    adj_open: get!(8, Option<f64>),
                    adj_high: get!(9, Option<f64>),
                    adj_low: get!(10, Option<f64>),
                    adj_close: get!(11, Option<f64>),
                    is_trading: get!(12, Option<bool>),
                    listing_date,
                });
            }
            Ok(output)
        }

        fn csv_or_parquet_copy(request: &ExportRequest) -> String {
            match request.format {
                analytics_core::ExportFormat::Parquet => format!(
                    "COPY ({}) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                    request.sql,
                    request
                        .destination
                        .display()
                        .to_string()
                        .replace('\'', "''")
                ),
                analytics_core::ExportFormat::Csv => format!(
                    "COPY ({}) TO '{}' (HEADER, FORMAT CSV)",
                    request.sql,
                    request
                        .destination
                        .display()
                        .to_string()
                        .replace('\'', "''")
                ),
            }
        }
    }

    fn parquet_source(path: &Path) -> String {
        if path.is_file() {
            path.display().to_string()
        } else {
            format!("{}/**/*.parquet", path.display())
        }
    }

    fn escape_sql_literal(value: &str) -> String {
        value.replace('\'', "''")
    }

    fn parse_date(value: String) -> Result<NaiveDate> {
        NaiveDate::parse_from_str(&value, "%Y-%m-%d")
            .map_err(|error| EngineError::DuckDb(format!("invalid date {value}: {error}")))
    }

    fn map_duckdb_type<T: std::fmt::Debug>(data_type: T) -> DataType {
        let name = format!("{data_type:?}");
        match name.as_str() {
            "Boolean" => DataType::Boolean,
            "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" | "UInt64" => {
                DataType::Int64
            }
            "Float32" | "Float64" => DataType::Float64,
            "Date32" => DataType::Date,
            _ => DataType::Utf8,
        }
    }

    impl QueryEngine for DuckDbEngine {
        fn list_parquet(&mut self, root: &Path) -> Result<Vec<DatasetFile>> {
            let mut out = Vec::new();
            fn visit(path: &Path, out: &mut Vec<DatasetFile>) -> std::io::Result<()> {
                if path.is_file() {
                    if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
                    {
                        out.push(DatasetFile {
                            path: path.to_path_buf(),
                            rows: None,
                            schema: Vec::new(),
                        });
                    }
                    return Ok(());
                }
                for entry in fs::read_dir(path)? {
                    visit(&entry?.path(), out)?;
                }
                Ok(())
            }
            visit(root, &mut out).map_err(|error| EngineError::DuckDb(error.to_string()))?;
            Ok(out)
        }

        fn describe(&mut self, sql: &str) -> Result<Vec<ColumnSchema>> {
            let query = format!("SELECT * FROM ({sql}) AS q LIMIT 0");
            let mut statement = self
                .connection
                .prepare(&query)
                .map_err(|e| EngineError::DuckDb(e.to_string()))?;
            {
                let _rows = statement
                    .query([])
                    .map_err(|e| EngineError::DuckDb(e.to_string()))?;
            }
            let columns = statement
                .column_names()
                .iter()
                .enumerate()
                .map(|(index, name)| ColumnSchema {
                    name: name.to_string(),
                    data_type: map_duckdb_type(statement.column_type(index)),
                    nullable: true,
                })
                .collect();
            Ok(columns)
        }

        fn query_page(&mut self, mut request: PageRequest) -> Result<QueryPage> {
            request
                .validate()
                .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
            if request.after.is_some() && request.sort.is_empty() {
                return Err(EngineError::InvalidRequest(
                    "keyset cursor requires sort keys".into(),
                ));
            }
            let columns = self.describe(&request.sql)?;
            for key in &mut request.sort {
                key.column = columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(&key.column))
                    .ok_or_else(|| {
                        EngineError::InvalidRequest(format!("unknown sort column {}", key.column))
                    })?
                    .name
                    .clone();
            }
            let cursor_filter = request
                .after
                .as_ref()
                .map(|cursor| keyset_predicate(&request.sort, cursor))
                .transpose()?;
            let order_by = if request.sort.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {}", order_clause(&request.sort)?)
            };
            let query = format!(
                "SELECT * FROM ({}) AS q{}{} LIMIT {}",
                request.sql,
                cursor_filter
                    .map(|filter| format!(" WHERE {filter}"))
                    .unwrap_or_default(),
                order_by,
                request.limit + 1
            );
            let mut statement = self
                .connection
                .prepare(&query)
                .map_err(|e| EngineError::DuckDb(e.to_string()))?;
            let mut rows = statement
                .query([])
                .map_err(|e| EngineError::DuckDb(e.to_string()))?;
            let mut values = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| EngineError::DuckDb(e.to_string()))?
            {
                let mut result = Vec::new();
                for index in 0..row.as_ref().column_count() {
                    let value = row
                        .get_ref(index)
                        .map_err(|e| EngineError::DuckDb(e.to_string()))?;
                    result.push(scalar_value(value)?);
                }
                values.push(result);
            }
            let has_next = values.len() > request.limit as usize;
            if has_next && !request.sort.is_empty() {
                let keys = |row: &Vec<ScalarValue>| {
                    request
                        .sort
                        .iter()
                        .filter_map(|sort| {
                            columns.iter().position(|column| column.name == sort.column)
                        })
                        .map(|index| row[index].clone())
                        .collect::<Vec<_>>()
                };
                let previous = keys(&values[values.len() - 2]);
                let following = keys(&values[values.len() - 1]);
                let identical = previous.iter().zip(&following).all(|(a, b)| {
                    a == b || matches!((a, b), (ScalarValue::Float64(a), ScalarValue::Float64(b)) if a.is_nan() && b.is_nan())
                });
                if identical {
                    return Err(EngineError::InvalidRequest(
                        "sort keys are not unique at the page boundary; add a unique tie-breaker"
                            .into(),
                    ));
                }
            }
            if has_next {
                values.pop();
            }
            let next_cursor = (has_next && !request.sort.is_empty()).then(|| {
                KeysetCursor(
                    values
                        .last()
                        .into_iter()
                        .flat_map(|row| {
                            request
                                .sort
                                .iter()
                                .filter_map(|sort| {
                                    columns.iter().position(|column| column.name == sort.column)
                                })
                                .map(|index| row[index].clone())
                        })
                        .collect(),
                )
            });
            Ok(QueryPage {
                columns,
                rows: values,
                next_cursor,
                total_rows: None,
            })
        }

        fn export(&mut self, request: ExportRequest) -> Result<()> {
            if let Some(parent) = request
                .destination
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|e| EngineError::DuckDb(e.to_string()))?;
            }
            self.connection
                .execute(&Self::csv_or_parquet_copy(&request), [])
                .map_err(|e| EngineError::DuckDb(e.to_string()))?;
            Ok(())
        }
    }

    fn scalar_value(value: duckdb::types::ValueRef<'_>) -> Result<ScalarValue> {
        use duckdb::types::ValueRef;
        Ok(match value {
            ValueRef::Null => ScalarValue::Null,
            ValueRef::Boolean(value) => ScalarValue::Boolean(value),
            ValueRef::TinyInt(value) => ScalarValue::Int64(value.into()),
            ValueRef::SmallInt(value) => ScalarValue::Int64(value.into()),
            ValueRef::Int(value) => ScalarValue::Int64(value.into()),
            ValueRef::BigInt(value) => ScalarValue::Int64(value),
            ValueRef::UTinyInt(value) => ScalarValue::Int64(value.into()),
            ValueRef::USmallInt(value) => ScalarValue::Int64(value.into()),
            ValueRef::UInt(value) => ScalarValue::Int64(value.into()),
            ValueRef::UBigInt(value) => ScalarValue::Int64(value.try_into().map_err(|_| {
                EngineError::InvalidRequest("uint64 result exceeds int64 range".into())
            })?),
            ValueRef::Float(value) => ScalarValue::Float64(value.into()),
            ValueRef::Double(value) => ScalarValue::Float64(value),
            ValueRef::Text(value) => ScalarValue::Utf8(String::from_utf8_lossy(value).into_owned()),
            ValueRef::Date32(days) => ScalarValue::Date(
                NaiveDate::from_ymd_opt(1970, 1, 1)
                    .unwrap()
                    .checked_add_signed(chrono::Duration::days(days.into()))
                    .ok_or_else(|| {
                        EngineError::InvalidRequest("date result out of range".into())
                    })?,
            ),
            other => ScalarValue::Utf8(format!("{other:?}")),
        })
    }

    fn order_clause(sort: &[analytics_core::SortKey]) -> Result<String> {
        sort.iter()
            .map(|key| {
                validate_identifier(&key.column)?;
                Ok(format!(
                    "q.{} {} NULLS LAST",
                    key.column,
                    if key.descending { "DESC" } else { "ASC" }
                ))
            })
            .collect::<Result<Vec<_>>>()
            .map(|parts| parts.join(", "))
    }

    fn keyset_predicate(
        sort: &[analytics_core::SortKey],
        cursor: &analytics_core::KeysetCursor,
    ) -> Result<String> {
        if sort.len() != cursor.0.len() {
            return Err(EngineError::InvalidRequest(
                "cursor values must match sort keys".into(),
            ));
        }
        let mut terms = Vec::new();
        for index in 0..sort.len() {
            let mut parts = Vec::new();
            for (prefix, key) in sort.iter().enumerate().take(index) {
                validate_identifier(&key.column)?;
                parts.push(format!(
                    "q.{} IS NOT DISTINCT FROM {}",
                    key.column,
                    scalar_literal(&cursor.0[prefix])?
                ));
            }
            validate_identifier(&sort[index].column)?;
            let operator = if sort[index].descending { "<" } else { ">" };
            parts.push(if matches!(&cursor.0[index], ScalarValue::Null) {
                "FALSE".into()
            } else {
                format!(
                    "(q.{} {} {} OR q.{} IS NULL)",
                    sort[index].column,
                    operator,
                    scalar_literal(&cursor.0[index])?,
                    sort[index].column
                )
            });
            terms.push(format!("({})", parts.join(" AND ")));
        }
        Ok(terms.join(" OR "))
    }

    fn validate_identifier(identifier: &str) -> Result<()> {
        if identifier.is_empty()
            || !identifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(EngineError::InvalidRequest(format!(
                "invalid sort column {identifier}"
            )));
        }
        Ok(())
    }

    fn scalar_literal(value: &ScalarValue) -> Result<String> {
        Ok(match value {
            ScalarValue::Null => "NULL".into(),
            ScalarValue::Boolean(value) => value.to_string(),
            ScalarValue::Int64(value) => value.to_string(),
            ScalarValue::Float64(value) if value.is_nan() => "CAST('NaN' AS DOUBLE)".into(),
            ScalarValue::Float64(value) if *value == f64::INFINITY => {
                "CAST('Infinity' AS DOUBLE)".into()
            }
            ScalarValue::Float64(value) if *value == f64::NEG_INFINITY => {
                "CAST('-Infinity' AS DOUBLE)".into()
            }
            ScalarValue::Float64(value) => value.to_string(),
            ScalarValue::Utf8(value) => format!("'{}'", value.replace('\'', "''")),
            ScalarValue::Date(value) => format!("DATE '{}'", value),
        })
    }

    pub use DuckDbEngine as PublicDuckDbEngine;
}

#[cfg(feature = "duckdb-native")]
pub use duckdb_backend::PublicDuckDbEngine as DuckDbEngine;

#[cfg(not(feature = "duckdb-native"))]
/// Placeholder used when no DuckDB backend is compiled in; every constructor
/// returns [`EngineError::BackendUnavailable`].
pub struct DuckDbEngine;

#[cfg(not(feature = "duckdb-native"))]
impl DuckDbEngine {
    /// Always fails with [`EngineError::BackendUnavailable`].
    pub fn open_in_memory() -> Result<Self> {
        Err(EngineError::BackendUnavailable)
    }
}
