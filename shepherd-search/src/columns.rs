//! Creates the card columns declared in `mapping.toml` `[columns]` and their indexes.
//!
//! `sync` runs at startup before serving: it adds missing columns (metadata-only for nullable
//! columns) and fails if an existing column has a different type. `ensure_indexes` runs in the
//! background: one process at a time (advisory lock) creates missing indexes `CONCURRENTLY` and
//! replaces invalid ones left by an interrupted build. Nothing is ever dropped for a column removed
//! from the mapping.

use sqlx::PgPool;

use crate::{
    backend::postgres::{column_ident, null_sentinel},
    mapping::{ColumnType, Columns},
    model::Dir,
};

const SYNC_LOCK: i64 = 0x7365_6172_6368_0001;
const INDEX_LOCK: i64 = 0x7365_6172_6368_0002;

fn pg_type_name(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "text",
        ColumnType::Numeric => "numeric",
        ColumnType::Timestamp => "timestamp with time zone",
    }
}

pub async fn sync(pool: &PgPool, columns: &Columns) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SYNC_LOCK)
        .execute(&mut *tx)
        .await?;
    for (name, def) in columns {
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
             WHERE attrelid = 'search.cards'::regclass AND attname = $1 AND attnum > 0 AND NOT attisdropped",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        match existing {
            Some(t) => anyhow::ensure!(
                t == pg_type_name(def.ty),
                "search.cards.{name} is {t}; mapping declares {}",
                pg_type_name(def.ty)
            ),
            None => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "ALTER TABLE search.cards ADD COLUMN {} {}",
                    column_ident(name),
                    def.ty.sql()
                )))
                .execute(&mut *tx)
                .await?;
                tracing::info!(column = %name, "added card column");
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// `(index name, definition)` for every declared column: a btree for filters and, when sortable,
/// keyset indexes matching `backend::postgres::column_sort_expr` in both directions.
pub fn wanted_indexes(columns: &Columns) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, def) in columns {
        let c = column_ident(name);
        out.push((
            format!("cards_col_{name}_idx"),
            format!("ON search.cards ({c})"),
        ));
        if def.sort {
            let desc = null_sentinel(def.ty, Dir::Desc);
            let asc = null_sentinel(def.ty, Dir::Asc);
            out.push((
                format!("cards_col_{name}_desc_idx"),
                format!("ON search.cards ((COALESCE({c}, {desc})) DESC, doc_id DESC)"),
            ));
            out.push((
                format!("cards_col_{name}_asc_idx"),
                format!("ON search.cards ((COALESCE({c}, {asc})), doc_id)"),
            ));
        }
    }
    out
}

/// Returns without work if another process holds the index lock.
pub async fn ensure_indexes(pool: &PgPool, columns: &Columns) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(INDEX_LOCK)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Ok(());
    }
    let res = async {
        for (name, def) in wanted_indexes(columns) {
            let valid: Option<bool> = sqlx::query_scalar(
                "SELECT i.indisvalid FROM pg_index i
                 JOIN pg_class c ON c.oid = i.indexrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'search' AND c.relname = $1",
            )
            .bind(&name)
            .fetch_optional(&mut *conn)
            .await?;
            match valid {
                Some(true) => continue,
                Some(false) => {
                    tracing::warn!(index = %name, "dropping invalid index");
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                        "DROP INDEX CONCURRENTLY IF EXISTS search.{name}"
                    )))
                    .execute(&mut *conn)
                    .await?;
                }
                None => {}
            }
            tracing::info!(index = %name, "creating index");
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} {def}"
            )))
            .execute(&mut *conn)
            .await?;
        }
        anyhow::Ok(())
    }
    .await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(INDEX_LOCK)
        .execute(&mut *conn)
        .await?;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_definitions() {
        let m: crate::mapping::Mapping =
            toml::from_str(include_str!("../config/mapping.toml")).unwrap();
        let names: Vec<String> = wanted_indexes(&m.columns)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names,
            [
                "cards_col_amount_idx",
                "cards_col_amount_desc_idx",
                "cards_col_amount_asc_idx",
                "cards_col_received_at_idx",
                "cards_col_received_at_desc_idx",
                "cards_col_received_at_asc_idx",
                "cards_col_status_idx",
            ]
        );
    }
}
