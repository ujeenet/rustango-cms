//! Clearing a page's references before the page row is deleted.
//!
//! Every table that points at `cms_page` (revisions, tags, categories,
//! logs, workflow state, host page-type extension tables…) declares a
//! plain foreign key with no `ON DELETE` action, so deleting a page that
//! any of them references failed with a foreign-key error — and every
//! page has at least its first revision. The references are cleared in
//! the same transaction as the delete, found from the registered models
//! rather than a hand-kept list, so a host app's extension tables are
//! covered too: a nullable reference is set to NULL, a required one is
//! deleted (after whatever points at *those* rows, recursively).
//!
//! The statements are built with the ORM's schema-driven queries
//! ([`UpdateQuery`], [`DeleteQuery`], [`SelectQuery`]), since the tables
//! are only known at run time.

use rustango::core::{
    Assignment, DeleteQuery, Expr, FieldType, Filter, ModelSchema, Op, Relation, SelectQuery,
    SqlValue, UpdateQuery, WhereExpr,
};
use rustango::sql::{ExecError, PoolTx};

/// Rows per `IN (…)` list — under every dialect's bind-parameter limit.
const CHUNK: usize = 500;

/// How deep reference chains are followed (page → revision → …).
const MAX_DEPTH: u8 = 4;

/// Clear every reference to the page `page_id`, inside the caller's
/// transaction, so the page row can be deleted next.
///
/// # Errors
/// Driver / query failures.
pub(crate) async fn clear_page_references_tx(tx: &mut PoolTx<'_>, page_id: i64) -> Result<(), ExecError> {
    clear_references(tx, "cms_page", &[page_id], 0).await
}

/// One model field that references a table.
struct Referrer {
    schema: &'static ModelSchema,
    column: &'static str,
    nullable: bool,
    /// The referring table's own integer primary key, to follow chains.
    pk: Option<&'static str>,
}

/// The managed models' fields that reference `target`.
fn referrers(target: &str) -> Vec<Referrer> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in rustango::inventory::iter::<rustango::core::ModelEntry> {
        let schema = entry.schema;
        if !schema.managed {
            continue;
        }
        let pk = schema
            .fields
            .iter()
            .find(|f| f.primary_key && matches!(f.ty, FieldType::I64 | FieldType::I32 | FieldType::I16))
            .map(|f| f.column);
        for f in schema.fields {
            let points_here = matches!(
                f.relation,
                Some(Relation::Fk { to, .. } | Relation::O2O { to, .. }) if to == target
            );
            if points_here && seen.insert((schema.table, f.column)) {
                out.push(Referrer { schema, column: f.column, nullable: f.nullable, pk });
            }
        }
    }
    out
}

fn clear_references<'a>(
    tx: &'a mut PoolTx<'_>,
    target: &'a str,
    ids: &'a [i64],
    depth: u8,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ExecError>> + Send + 'a>> {
    Box::pin(async move {
        if ids.is_empty() || depth > MAX_DEPTH {
            return Ok(());
        }
        for r in referrers(target) {
            for chunk in ids.chunks(CHUNK) {
                let within = || -> WhereExpr {
                    let list: Vec<SqlValue> = chunk.iter().map(|id| SqlValue::from(*id)).collect();
                    WhereExpr::Predicate(Filter::new(r.column, Op::In, SqlValue::List(list)))
                };
                if r.nullable {
                    let query = UpdateQuery::new(
                        r.schema,
                        vec![Assignment::new(r.column, Expr::Literal(SqlValue::Null))],
                        within(),
                    );
                    rustango::sql::update_tx(tx, &query).await?;
                    continue;
                }
                if let Some(pk) = r.pk {
                    let mut q = SelectQuery::new(r.schema);
                    q.where_clause = within();
                    q.projection = Some(vec![pk]);
                    let stmt = tx.dialect().compile_select(&q)?;
                    let rows: Vec<(i64,)> = rustango::sql::raw_query_tx(tx, &stmt.sql, stmt.params).await?;
                    let child_ids: Vec<i64> = rows.into_iter().map(|(id,)| id).collect();
                    clear_references(tx, r.schema.table, &child_ids, depth + 1).await?;
                }
                let query = DeleteQuery::new(r.schema, within());
                rustango::sql::delete_tx(tx, &query).await?;
            }
        }
        Ok(())
    })
}
