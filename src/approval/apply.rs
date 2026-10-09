use thiserror::Error;
use tokio_postgres::Transaction;

use super::conflict::{check_conflicts, Conflict, ConflictError, TableConflictCheck};

#[derive(Debug)]
pub enum ApplyResult {
    Success { applied: usize },
    Conflict { conflicts: Vec<Conflict> },
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Session not found")]
    SessionNotFound,

    #[error("Session not in pending_review status")]
    InvalidStatus,
}

/// Apply all staged changes from a session to production
///
/// 1. Check for conflicts
/// 2. If no conflicts (or force=true), apply in a transaction
/// 3. Return result
pub async fn apply_session(
    client: &mut tokio_postgres::Client,
    schema_name: &str,
    tables: &[TableApplyConfig],
    force: bool,
) -> Result<ApplyResult, ApplyError> {
    // 1. Build conflict check configs
    let conflict_checks: Vec<TableConflictCheck> = tables.iter().map(|t| {
        TableConflictCheck {
            table_name: t.table_name.clone(),
            primary_key: t.primary_key.clone(),
            primary_key_type: t.primary_key_type.clone(),
            hash_columns: t.hash_columns.clone(),
            check_insert_collisions: t.has_inserts,
        }
    }).collect();

    // 2. Check conflicts
    let conflicts = check_conflicts(client, schema_name, &conflict_checks).await
        .map_err(|e| match e {
            ConflictError::Database(e) => ApplyError::Database(e),
            ConflictError::ConflictsDetected(c) => {
                // This shouldn't happen since check_conflicts returns Ok with conflicts
                ApplyError::Database(tokio_postgres::Error::__private_api_timeout())
            }
        })?;

    if !conflicts.is_empty() && !force {
        return Ok(ApplyResult::Conflict { conflicts });
    }

    // 3. Apply in transaction
    let tx = client.transaction().await?;
    let mut applied = 0;

    for table in tables {
        applied += apply_table(&tx, schema_name, table).await?;
    }

    tx.commit().await?;

    Ok(ApplyResult::Success { applied })
}

async fn apply_table(
    tx: &Transaction<'_>,
    schema_name: &str,
    table: &TableApplyConfig,
) -> Result<usize, tokio_postgres::Error> {
    let mut count = 0;

    // Apply INSERTs (rows in shadow not in prod)
    let insert_result = tx.execute(&format!(r#"
        INSERT INTO public.{table}
        SELECT s.* FROM {schema}.{table} s
        WHERE s.{pk} NOT IN (SELECT {pk} FROM public.{table})
    "#,
        schema = schema_name,
        table = table.table_name,
        pk = table.primary_key,
    ), &[]).await?;
    count += insert_result as usize;

    // Apply UPDATEs (rows in both shadow and prod)
    // We need to update all columns except the primary key
    if !table.update_columns.is_empty() {
        let set_clause = table.update_columns.iter()
            .map(|col| format!("{col} = s.{col}"))
            .collect::<Vec<_>>()
            .join(", ");

        let update_result = tx.execute(&format!(r#"
            UPDATE public.{table} p
            SET {set_clause}
            FROM {schema}.{table} s
            WHERE p.{pk} = s.{pk}
              AND p.{pk} IN (SELECT {pk} FROM public.{table})
        "#,
            schema = schema_name,
            table = table.table_name,
            pk = table.primary_key,
            set_clause = set_clause,
        ), &[]).await?;
        count += update_result as usize;
    }

    // Apply DELETEs
    let delete_result = tx.execute(&format!(r#"
        DELETE FROM public.{table}
        WHERE {pk}::text IN (
            SELECT row_id FROM {schema}._deletes WHERE table_name = $1
        )
    "#,
        schema = schema_name,
        table = table.table_name,
        pk = table.primary_key,
    ), &[&table.table_name]).await?;
    count += delete_result as usize;

    Ok(count)
}

#[derive(Debug, Clone)]
pub struct TableApplyConfig {
    pub table_name: String,
    pub primary_key: String,
    pub primary_key_type: String,
    pub hash_columns: Vec<String>,
    pub update_columns: Vec<String>,
    pub has_inserts: bool,
}

/// Get all columns except primary key for UPDATE SET clause
pub async fn get_update_columns(
    client: &tokio_postgres::Client,
    table_name: &str,
    primary_key: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let rows = client.query(r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = $1
          AND column_name != $2
        ORDER BY ordinal_position
    "#, &[&table_name, &primary_key]).await?;

    Ok(rows.iter().map(|r| r.get("column_name")).collect())
}
