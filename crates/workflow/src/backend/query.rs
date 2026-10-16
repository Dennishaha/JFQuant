//! Worker-local DuckDB connection and immutable pagination snapshot.
use super::{DuckDbEngine, PageRequest, PathBuf, QueryEngine, QueryPage, ScalarValue, SortKey};
#[derive(Default)]
pub(super) struct QuerySession {
    root: Option<PathBuf>,
    engine: Option<DuckDbEngine>,
    query_snapshot: Option<QuerySnapshot>,
}
struct QuerySnapshot {
    request: PageRequest,
    sql: String,
    ordinal: String,
}

impl QuerySession {
    pub(super) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }
    pub(super) fn next(
        &mut self,
        after: analytics_core::KeysetCursor,
    ) -> Result<QueryPage, String> {
        let mut request = self
            .query_snapshot
            .as_ref()
            .ok_or("query_expired: 查询已失效")?
            .request
            .clone();
        request.after = Some(after);
        self.query_page(request)
    }
    pub(super) fn engine(&mut self) -> Result<&mut DuckDbEngine, String> {
        if self.engine.is_none() {
            let mut engine = DuckDbEngine::open_in_memory().map_err(|e| e.to_string())?;
            if let Some(root) = &self.root {
                engine.set_file_root(root).map_err(|e| e.to_string())?;
            }
            self.engine = Some(engine);
        }
        Ok(self.engine.as_mut().expect("engine initialized"))
    }

    pub(super) fn query_page(&mut self, request: PageRequest) -> Result<QueryPage, String> {
        // Desktop cursors contain a hidden ordinal, not the selected column's
        // value. Sort all remaining columns to make ties deterministic; identical
        // rows remain distinct and NULLs are placed last in either direction.
        if request.after.is_none() {
            self.query_snapshot = None;
            if let Some(engine) = &mut self.engine {
                engine
                    .clear_query_snapshot()
                    .map_err(|error| error.to_string())?;
            }
        }
        let mut validation = request.clone();
        validation.after = None;
        validation.validate().map_err(|error| error.to_string())?;
        if let Some(cursor) = &request.after
            && !matches!(cursor.0.as_slice(), [ScalarValue::Int64(value)] if *value > 0)
        {
            return Err("分页游标无效，请重新执行查询。".into());
        }
        if request.after.is_some() {
            let snapshot = self
                .query_snapshot
                .as_ref()
                .ok_or("分页结果已失效，请重新执行查询。")?;
            if !same_query(&request, &snapshot.request) {
                return Err("查询条件已改变，请重新执行查询。".into());
            }
            let sql = snapshot.sql.clone();
            let ordinal = snapshot.ordinal.clone();
            return self.snapshot_page(sql, ordinal, request.limit, request.after);
        }
        let engine = self.engine()?;
        let columns = engine
            .describe(&request.sql)
            .map_err(|error| error.to_string())?;
        let mut sort = request.sort.clone();
        for key in &mut sort {
            key.column = columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(&key.column))
                .ok_or_else(|| format!("排序列不存在：{}。请选择默认排序后重新执行。", key.column))?
                .name
                .clone();
        }
        for column in &columns {
            if !sort.iter().any(|key| key.column == column.name) {
                sort.push(SortKey {
                    column: column.name.clone(),
                    descending: false,
                });
            }
        }
        let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        let order = sort
            .iter()
            .map(|key| {
                format!(
                    "q.{} {} NULLS LAST",
                    quote(&key.column),
                    if key.descending { "DESC" } else { "ASC" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut ordinal = "__jfquant_page_row".to_string();
        while columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(&ordinal))
        {
            ordinal.push('_');
        }
        let sql = format!(
            "SELECT row_number() OVER (ORDER BY {order}) AS {}, q.* FROM ({}) AS q",
            quote(&ordinal),
            request.sql
        );
        let sql = engine
            .replace_query_snapshot(&sql)
            .map_err(|error| error.to_string())?;
        self.query_snapshot = Some(QuerySnapshot {
            request: request.clone(),
            sql: sql.clone(),
            ordinal: ordinal.clone(),
        });
        self.snapshot_page(sql, ordinal, request.limit, request.after)
    }

    fn snapshot_page(
        &mut self,
        sql: String,
        ordinal: String,
        limit: u32,
        after: Option<analytics_core::KeysetCursor>,
    ) -> Result<QueryPage, String> {
        let mut page = self
            .engine()?
            .query_page(PageRequest {
                sql,
                limit,
                sort: vec![SortKey {
                    column: ordinal,
                    descending: false,
                }],
                after,
            })
            .map_err(|error| error.to_string())?;
        page.columns.remove(0);
        for row in &mut page.rows {
            row.remove(0);
        }
        Ok(page)
    }
}
fn same_query(a: &PageRequest, b: &PageRequest) -> bool {
    a.sql == b.sql
        && a.limit == b.limit
        && a.sort.len() == b.sort.len()
        && a.sort
            .iter()
            .zip(&b.sort)
            .all(|(a, b)| a.column == b.column && a.descending == b.descending)
}
