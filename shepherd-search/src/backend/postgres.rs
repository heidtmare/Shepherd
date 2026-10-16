//! Postgres full-text backend over `search.cards` / `search.pages`.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgRow, types::Json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::SearchBackend;
use crate::{
    facet_cache::{FacetCache, FacetCounts},
    mapping::{Card, ColumnType, Columns, Page, SCHEMA_FACET},
    model::{CardSummary, CardView, Dir, FacetCount, Hit, SearchResponse},
    query::{Clause, CmpOp, Cursor, Field, FieldKind, Plan, SortKey},
};

const MAX_FACET_VALUES: usize = 50;
const HEADLINE_CHARS: usize = 100_000;
const MAX_MATCHED_PAGES: usize = 20;
// Bound on overflow-page matches feeding the card match. See README "Limits".
const MAX_PAGE_DOC_MATCHES: usize = 100_000;
// Private-use code points delimit matches so the snippet can be HTML-escaped before inserting <mark>.
const HL_START: char = '\u{E000}';
const HL_STOP: char = '\u{E001}';

// `q` is the per-query CROSS JOIN providing `q.query` (tsquery) and `q.text` (raw input).
const SCORE: &str =
    "(ts_rank_cd(c.tsv, q.query, 32) + 0.1::real * similarity(COALESCE(c.filename, ''), q.text))";

/// Bounds on per-query work. See README "Limits".
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub rank_window: u32,
    pub facet_window: u32,
    pub facet_cache_ttl: Duration,
}

#[derive(Clone)]
pub struct PgBackend {
    write: PgPool,
    read: PgPool,
    limits: Limits,
    columns: Arc<Columns>,
    facet_cache: Arc<FacetCache>,
}

impl PgBackend {
    /// `columns` must already exist on `search.cards` (`columns::sync`).
    pub fn new(write: PgPool, read: PgPool, limits: Limits, columns: Columns) -> Self {
        Self {
            write,
            read,
            limits,
            columns: Arc::new(columns),
            facet_cache: Arc::new(FacetCache::new(limits.facet_cache_ttl)),
        }
    }
}

/// Quoted identifier for a declared column; names are validated by `mapping::valid_column_name`.
pub fn column_ident(name: &str) -> String {
    debug_assert!(crate::mapping::valid_column_name(name));
    format!("\"{name}\"")
}

/// Keyset sort expression for a sortable column; nulls take a sentinel that sorts last.
pub fn column_sort_expr(name: &str, ty: ColumnType, d: Dir) -> String {
    format!(
        "COALESCE(c.{}, {})",
        column_ident(name),
        null_sentinel(ty, d)
    )
}

/// Value substituted for NULL so it sorts last in direction `d`.
pub fn null_sentinel(ty: ColumnType, d: Dir) -> &'static str {
    match (ty, d) {
        (ColumnType::Numeric, Dir::Desc) => "'-Infinity'::numeric",
        (ColumnType::Numeric, Dir::Asc) => "'Infinity'::numeric",
        (ColumnType::Timestamp, Dir::Desc) => "'-infinity'::timestamptz",
        (ColumnType::Timestamp, Dir::Asc) => "'infinity'::timestamptz",
        (ColumnType::Text, _) => unreachable!("text columns are not sortable"),
    }
}

/// Select-list entries for every declared column, aliased `col_<name>`. Numerics are read as text.
fn push_column_select(qb: &mut QueryBuilder<Postgres>, columns: &Columns) {
    for (name, def) in columns {
        let cast = if def.ty == ColumnType::Numeric {
            "::text"
        } else {
            ""
        };
        qb.push(format_args!(
            "c.{}{cast} AS \"col_{name}\", ",
            column_ident(name)
        ));
    }
}

fn column(f: &Field) -> String {
    match f {
        Field::Filename => "c.filename".into(),
        Field::CreatedAt => "c.created_at".into(),
        Field::UpdatedAt => "c.updated_at".into(),
        Field::Schema => "c.schemas".into(),
        Field::Column { name, .. } => format!("c.{}", column_ident(name)),
    }
}

fn schema_facet(schema: &str) -> String {
    format!("{SCHEMA_FACET}/{schema}")
}

fn cast(k: FieldKind) -> &'static str {
    match k {
        FieldKind::Text | FieldKind::TextList => "text",
        FieldKind::Numeric => "numeric",
        FieldKind::Timestamp => "timestamptz",
    }
}

fn op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Gte => ">=",
        CmpOp::Gt => ">",
        CmpOp::Lte => "<=",
        CmpOp::Lt => "<",
    }
}

fn dir_sql(d: Dir) -> &'static str {
    match d {
        Dir::Asc => "ASC",
        Dir::Desc => "DESC",
    }
}

/// Sort expression and its type. Nullable columns are coalesced to a sentinel that sorts last
/// in the requested direction, keeping row-value keyset comparison well defined.
fn sort_expr(k: &SortKey, d: Dir) -> (String, &'static str) {
    match k {
        SortKey::Relevance => (SCORE.into(), "real"),
        SortKey::UpdatedAt => ("c.updated_at".into(), "timestamptz"),
        SortKey::CreatedAt => ("c.created_at".into(), "timestamptz"),
        SortKey::Filename => ("COALESCE(c.filename, '')".into(), "text"),
        SortKey::Column { name, ty } => (column_sort_expr(name, *ty, d), ty.sql()),
    }
}

/// Appends FROM + WHERE shared by the hits and facet queries.
fn push_from_where(
    qb: &mut QueryBuilder<Postgres>,
    plan: &Plan,
    extra_from: &str,
    with_cursor: bool,
) {
    qb.push(" FROM search.cards c");
    push_q_join(qb, plan);
    qb.push(extra_from);
    qb.push(" WHERE TRUE");
    if plan.q.is_some() {
        qb.push(" AND (c.tsv @@ q.query OR c.filename % q.text");
        if plan.include_pages {
            // Uncorrelated init plan; keeps the OR indexable (BitmapOr over tsv, trigram and pkey).
            // Only pages outside the card's indexed prefix can add matches (partial GIN index).
            qb.push(format_args!(
                " OR c.doc_id = ANY(ARRAY(SELECT p.doc_id FROM search.pages p \
                 WHERE NOT p.in_card AND p.tsv @@ q.query LIMIT {MAX_PAGE_DOC_MATCHES}))"
            ));
        }
        qb.push(")");
    }
    for clause in &plan.clauses {
        match clause {
            // `schema` is matched through its reserved facet: `facets` is GIN-indexed, `schemas` is not.
            Clause::Cmp { field, op, value } if field.kind() == FieldKind::TextList => {
                debug_assert_eq!(*op, CmpOp::Eq);
                qb.push(" AND c.facets @> ARRAY[");
                qb.push_bind(schema_facet(value));
                qb.push("]::text[]");
            }
            Clause::AnyOf { field, values } if field.kind() == FieldKind::TextList => {
                qb.push(" AND c.facets && ");
                qb.push_bind(values.iter().map(|v| schema_facet(v)).collect::<Vec<_>>());
                qb.push("::text[]");
            }
            Clause::Cmp { field, op, value } => {
                qb.push(format_args!(" AND {} {} ", column(field), op_sql(*op)));
                qb.push_bind(value.clone());
                qb.push(format_args!("::{}", cast(field.kind())));
            }
            Clause::AnyOf { field, values } => {
                qb.push(format_args!(" AND {} = ANY(", column(field)));
                qb.push_bind(values.clone());
                qb.push(format_args!("::{}[])", cast(field.kind())));
            }
            Clause::Contains(v) => {
                qb.push(" AND c.fields @> ");
                qb.push_bind(Json(v.clone()));
            }
            Clause::Facet(v) => {
                qb.push(" AND c.facets @> ARRAY[");
                qb.push_bind(v.clone());
                qb.push("]::text[]");
            }
        }
    }
    if with_cursor {
        push_cursor(qb, plan);
    }
}

fn push_q_join(qb: &mut QueryBuilder<Postgres>, plan: &Plan) {
    if let Some(q) = &plan.q {
        qb.push(" CROSS JOIN (SELECT websearch_to_tsquery('english', ");
        qb.push_bind(q.clone());
        qb.push(") AS query, ");
        qb.push_bind(q.clone());
        qb.push("::text AS text) AS q");
    }
}

fn push_cursor(qb: &mut QueryBuilder<Postgres>, plan: &Plan) {
    if let Some(c) = &plan.after {
        let (expr, ty) = sort_expr(&plan.sort, plan.dir);
        let cmp = if plan.dir == Dir::Desc { "<" } else { ">" };
        qb.push(format_args!(" AND ({expr}, c.doc_id) {cmp} ("));
        qb.push_bind(c.value.clone());
        qb.push(format_args!("::{ty}, "));
        qb.push_bind(c.id);
        qb.push(")");
    }
}

/// `rank_window` bounds relevance scoring: `ts_rank_cd` has no index support and costs O(matches),
/// so only the `rank_window` most recently updated matches are scored.
pub fn build_hits(plan: &Plan, rank_window: u32, columns: &Columns) -> QueryBuilder<Postgres> {
    let (sort, _) = sort_expr(&plan.sort, plan.dir);
    let dir = dir_sql(plan.dir);
    let has_q = plan.q.is_some();
    let mut qb =
        QueryBuilder::new("SELECT h.doc_id, h.score, h.sort_key::text AS sort_key, c.filename, ");
    push_column_select(&mut qb, columns);
    qb.push("c.facets, c.schemas, c.created_at, c.updated_at, ");
    if has_q {
        qb.push(format_args!(
            "ts_headline('english', left(c.content, {HEADLINE_CHARS}), h.tsq, \
             'MaxFragments=2, MaxWords=30, MinWords=10, StartSel={HL_START}, StopSel={HL_STOP}') AS highlight, "
        ));
    } else {
        qb.push("NULL::text AS highlight, ");
    }
    if has_q && plan.include_pages {
        qb.push(format_args!(
            "ARRAY(SELECT p.page_no FROM search.pages p WHERE p.doc_id = h.doc_id AND p.tsv @@ h.tsq \
             ORDER BY p.page_no LIMIT {MAX_MATCHED_PAGES}) AS matched_pages"
        ));
    } else {
        qb.push("'{}'::int[] AS matched_pages");
    }
    // Inner query selects the page; snippets are computed only for that page.
    qb.push(" FROM (SELECT c.doc_id, ");
    qb.push(if has_q { SCORE } else { "0::real" });
    qb.push(format_args!(" AS score, {sort} AS sort_key"));
    if has_q {
        qb.push(", q.query AS tsq");
    }
    if has_q && plan.sort == SortKey::Relevance {
        qb.push(" FROM (SELECT c.doc_id, c.tsv, c.filename");
        push_from_where(&mut qb, plan, "", false);
        qb.push(format_args!(
            " ORDER BY c.updated_at DESC, c.doc_id DESC LIMIT {rank_window}) c"
        ));
        push_q_join(&mut qb, plan);
        qb.push(" WHERE TRUE");
        push_cursor(&mut qb, plan);
    } else {
        push_from_where(&mut qb, plan, "", true);
    }
    qb.push(format_args!(
        " ORDER BY {sort} {dir}, c.doc_id {dir} LIMIT {}",
        plan.limit + 1
    ));
    qb.push(format_args!(
        ") h JOIN search.cards c ON c.doc_id = h.doc_id ORDER BY h.sort_key {dir}, h.doc_id {dir}"
    ));
    qb
}

/// Counts facet values over at most `facet_window` matches, in scan order (no sort, so the scan
/// stops at the window). Every row carries `total`, the number of matches aggregated; `total` below
/// the window means the counts are exact. One row with a NULL facet when no requested facet occurs.
pub fn build_facets(plan: &Plan, facet_window: u32) -> QueryBuilder<Postgres> {
    let mut qb = QueryBuilder::new("WITH m AS (SELECT c.facets");
    push_from_where(&mut qb, plan, "", false);
    qb.push(format_args!(
        " LIMIT {facet_window}), t AS (SELECT count(*) AS total FROM m)"
    ));
    qb.push(
        " SELECT t.total, f.facet, f.n FROM t LEFT JOIN (SELECT v.value AS facet, count(*) AS n",
    );
    qb.push(" FROM m CROSS JOIN LATERAL unnest(m.facets) AS v(value) WHERE split_part(v.value, '/', 1) = ANY(");
    qb.push_bind(plan.facets.clone());
    qb.push("::text[]) GROUP BY v.value) f ON TRUE ORDER BY f.n DESC, f.facet");
    qb
}

fn facet_counts(plan: &Plan, rows: &[PgRow], facet_window: u32) -> sqlx::Result<FacetCounts> {
    let mut counts: BTreeMap<String, Vec<FacetCount>> = plan
        .facets
        .iter()
        .map(|f| (f.clone(), Vec::new()))
        .collect();
    let mut total = 0i64;
    for r in rows {
        total = r.try_get("total")?;
        let Some(facet) = r.try_get::<Option<String>, _>("facet")? else {
            continue;
        };
        let Some((name, value)) = facet.split_once('/') else {
            continue;
        };
        if let Some(list) = counts.get_mut(name)
            && list.len() < MAX_FACET_VALUES
        {
            list.push(FacetCount {
                value: value.to_owned(),
                count: r.try_get("n")?,
            });
        }
    }
    Ok(FacetCounts {
        counts,
        exact: total < i64::from(facet_window),
    })
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            HL_START => out.push_str("<mark>"),
            HL_STOP => out.push_str("</mark>"),
            c => out.push(c),
        }
    }
    out
}

fn summary(r: &PgRow, columns: &Columns) -> sqlx::Result<CardSummary> {
    let mut values = BTreeMap::new();
    for (name, def) in columns {
        let alias = format!("col_{name}");
        let v = match def.ty {
            ColumnType::Timestamp => {
                r.try_get::<Option<OffsetDateTime>, _>(alias.as_str())?
                    .map(|t| {
                        t.format(&Rfc3339)
                            .expect("RFC 3339 formats any timestamptz")
                    })
            }
            _ => r.try_get::<Option<String>, _>(alias.as_str())?,
        };
        values.insert(name.clone(), v.map_or(serde_json::Value::Null, Into::into));
    }
    Ok(CardSummary {
        filename: r.try_get("filename")?,
        columns: values,
        facets: r.try_get("facets")?,
        schemas: r.try_get("schemas")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}

impl SearchBackend for PgBackend {
    async fn upsert(
        &self,
        card: &Card,
        pages: &[Page],
        seq: u64,
        force: bool,
        indexed_at: OffsetDateTime,
    ) -> anyhow::Result<bool> {
        let mut tx = self.write.begin().await?;
        let mut qb = QueryBuilder::new(
            "INSERT INTO search.cards (doc_id, filename, content, fields, facets, schemas, source_seq, \
             created_at, updated_at, indexed_at",
        );
        for name in self.columns.keys() {
            qb.push(format_args!(", {}", column_ident(name)));
        }
        qb.push(") VALUES (");
        let mut vals = qb.separated(", ");
        vals.push_bind(card.doc_id)
            .push_bind(&card.filename)
            .push_bind(&card.content)
            .push_bind(Json(&card.fields))
            .push_bind(&card.facets)
            .push_bind(&card.schemas)
            .push_bind(i64::try_from(seq)?)
            .push_bind(card.created_at)
            .push_bind(card.updated_at)
            .push_bind(indexed_at);
        for (name, def) in self.columns.iter() {
            vals.push_bind(card.columns.get(name).cloned());
            vals.push_unseparated(format_args!("::{}", def.ty.sql()));
        }
        qb.push(
            ") ON CONFLICT (doc_id) DO UPDATE SET \
             filename = EXCLUDED.filename, content = EXCLUDED.content, fields = EXCLUDED.fields, \
             facets = EXCLUDED.facets, schemas = EXCLUDED.schemas, \
             source_seq = GREATEST(search.cards.source_seq, EXCLUDED.source_seq), \
             created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at, indexed_at = EXCLUDED.indexed_at",
        );
        for name in self.columns.keys() {
            let c = column_ident(name);
            qb.push(format_args!(", {c} = EXCLUDED.{c}"));
        }
        qb.push(" WHERE ");
        qb.push_bind(force);
        qb.push(" OR search.cards.source_seq < EXCLUDED.source_seq RETURNING doc_id");
        let applied = qb.build().fetch_optional(&mut *tx).await?.is_some();
        if !applied {
            return Ok(false);
        }
        sqlx::query("DELETE FROM search.pages WHERE doc_id = $1")
            .bind(card.doc_id)
            .execute(&mut *tx)
            .await?;
        if !pages.is_empty() {
            let nos: Vec<i32> = pages.iter().map(|p| p.page_no).collect();
            let texts: Vec<&str> = pages.iter().map(|p| p.text.as_str()).collect();
            let in_card: Vec<bool> = pages.iter().map(|p| p.in_card).collect();
            sqlx::query(
                "INSERT INTO search.pages (doc_id, page_no, text, in_card)
                 SELECT $1, * FROM unnest($2::int[], $3::text[], $4::bool[])",
            )
            .bind(card.doc_id)
            .bind(&nos)
            .bind(&texts)
            .bind(&in_card)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn delete(&self, doc_id: Uuid) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM search.cards WHERE doc_id = $1")
            .bind(doc_id)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    async fn search(&self, plan: &Plan) -> anyhow::Result<SearchResponse> {
        let Limits {
            rank_window,
            facet_window,
            ..
        } = self.limits;
        let mut hits_q = build_hits(plan, rank_window, &self.columns);
        let hits_fut = hits_q.build().fetch_all(&self.read);
        let cache_key =
            (!plan.facets.is_empty() && self.facet_cache.enabled()).then(|| plan.facet_key());
        let cached = cache_key.as_deref().and_then(|k| self.facet_cache.get(k));
        if cache_key.is_some() {
            metrics::counter!("facet_cache_total", "hit" => cached.is_some().to_string())
                .increment(1);
        }
        let mut facet_q =
            (!plan.facets.is_empty() && cached.is_none()).then(|| build_facets(plan, facet_window));
        let facets_fut = async {
            match facet_q.as_mut() {
                Some(q) => q.build().fetch_all(&self.read).await,
                None => Ok(Vec::new()),
            }
        };
        let (rows, facet_rows) = tokio::try_join!(hits_fut, facets_fut)?;
        let facets = match cached {
            Some(c) => c,
            None if plan.facets.is_empty() => FacetCounts {
                counts: BTreeMap::new(),
                exact: true,
            },
            None => {
                let c = facet_counts(plan, &facet_rows, facet_window)?;
                if !c.exact {
                    metrics::counter!("facet_counts_truncated_total").increment(1);
                }
                if let Some(k) = cache_key {
                    self.facet_cache.put(k, c.clone());
                }
                c
            }
        };

        let limit = plan.limit as usize;
        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            Some(
                Cursor {
                    sort: plan.sort.name().into(),
                    dir: plan.dir,
                    value: last.try_get("sort_key")?,
                    id: last.try_get("doc_id")?,
                }
                .encode(),
            )
        } else {
            None
        };
        let hits = rows
            .iter()
            .take(limit)
            .map(|r| {
                Ok(Hit {
                    doc_id: r.try_get("doc_id")?,
                    score: r.try_get("score")?,
                    highlight: r
                        .try_get::<Option<String>, _>("highlight")?
                        .map(|h| html_escape(&h)),
                    matched_pages: r.try_get("matched_pages")?,
                    summary: summary(r, &self.columns)?,
                })
            })
            .collect::<sqlx::Result<Vec<_>>>()?;

        Ok(SearchResponse {
            hits,
            facet_counts: facets.counts,
            facet_counts_exact: facets.exact,
            next_cursor,
        })
    }

    async fn get(&self, doc_id: Uuid) -> anyhow::Result<Option<CardView>> {
        let mut qb = QueryBuilder::new("SELECT c.doc_id, c.filename, ");
        push_column_select(&mut qb, &self.columns);
        qb.push("c.facets, c.schemas, c.created_at, c.updated_at, c.fields FROM search.cards c WHERE c.doc_id = ");
        qb.push_bind(doc_id);
        let row = qb.build().fetch_optional(&self.read).await?;
        row.map(|r| {
            Ok(CardView {
                doc_id: r.try_get("doc_id")?,
                summary: summary(&r, &self.columns)?,
                fields: r.try_get::<Json<serde_json::Value>, _>("fields")?.0,
            })
        })
        .transpose()
    }

    async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.read).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SearchRequest;
    use serde_json::json;

    fn columns() -> Columns {
        toml::from_str::<crate::mapping::Mapping>(include_str!("../../config/mapping.toml"))
            .unwrap()
            .columns
    }

    fn plan(v: serde_json::Value) -> Plan {
        Plan::new(
            &serde_json::from_value::<SearchRequest>(v).unwrap(),
            &columns(),
        )
        .unwrap()
    }

    #[test]
    fn sql_browse() {
        insta::assert_snapshot!(build_hits(&plan(json!({})), 2000, &columns()).into_string());
    }

    #[test]
    fn sql_full() {
        let cursor = Cursor {
            sort: "relevance".into(),
            dir: Dir::Desc,
            value: "0.5".into(),
            id: Uuid::nil(),
        };
        let p = plan(json!({
            "q": "water damage",
            "include_pages": true,
            "facets": ["type", "status"],
            "cursor": cursor.encode(),
            "filters": [
                {"op": "eq", "field": "status", "value": "open"},
                {"op": "in", "field": "schema", "values": ["intake.ir.v1"]},
                {"op": "in", "field": "status", "values": ["a", "b"]},
                {"op": "range", "field": "amount", "gte": 100, "lt": 200},
                {"op": "contains", "value": {"intake.ir.v1": {"totals": {"currency": "USD"}}}},
                {"op": "facet", "value": "type/claim"}
            ]
        }));
        insta::assert_snapshot!("hits", build_hits(&p, 2000, &columns()).into_string());
        insta::assert_snapshot!("facets", build_facets(&p, 100_000).into_string());
    }

    #[test]
    fn sql_sort_amount_asc() {
        let cursor = Cursor {
            sort: "amount".into(),
            dir: Dir::Asc,
            value: "10".into(),
            id: Uuid::nil(),
        };
        insta::assert_snapshot!(
            build_hits(
                &plan(
                    json!({"sort": {"field": "amount", "dir": "asc"}, "cursor": cursor.encode()})
                ),
                2000,
                &columns()
            )
            .into_string()
        );
    }

    #[test]
    fn highlight_escaping() {
        let raw = format!("a <b> & {HL_START}water{HL_STOP} \"x\"");
        assert_eq!(
            html_escape(&raw),
            "a &lt;b&gt; &amp; <mark>water</mark> &quot;x&quot;"
        );
    }
}
