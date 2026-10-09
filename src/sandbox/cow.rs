#![allow(dead_code)]
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CowError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Table not found: {0}")]
    TableNotFound(String),
}

/// Copy-on-Write: Copy affected rows to shadow before UPDATE
///
/// This ensures:
/// 1. The shadow table has the rows we're about to modify
/// 2. We capture the expected state for conflict detection
pub async fn copy_rows_to_shadow(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
    primary_key: &str,
    where_clause: &str,
    hash_columns: &[String],
) -> Result<u64, CowError> {
    // 1. Copy rows to shadow (if not already there)
    let copy_sql = format!(
        r#"
        INSERT INTO {schema}.{table}
        SELECT * FROM public.{table}
        WHERE {where_clause}
          AND {pk} NOT IN (SELECT {pk} FROM {schema}.{table})
        ON CONFLICT ({pk}) DO NOTHING
    "#,
        schema = schema_name,
        table = table_name,
        pk = primary_key,
        where_clause = where_clause,
    );
    let copied = client.execute(&copy_sql, &[]).await?;

    // 2. Capture expected state for conflict detection
    let hash_expr = if hash_columns.is_empty() {
        format!("md5({}::text)", primary_key)
    } else {
        format!("md5(concat_ws('|', {}))", hash_columns.join(", "))
    };

    let capture_sql = format!(
        r#"
        INSERT INTO {schema}._expected_state (table_name, row_id, state_hash)
        SELECT
            '{table}',
            {pk}::text,
            {hash_expr}
        FROM public.{table}
        WHERE {where_clause}
        ON CONFLICT (table_name, row_id) DO NOTHING
    "#,
        schema = schema_name,
        table = table_name,
        pk = primary_key,
        hash_expr = hash_expr,
        where_clause = where_clause,
    );
    client.execute(&capture_sql, &[]).await?;

    Ok(copied)
}

/// Create shadow table for a given table
///
/// Uses LIKE ... INCLUDING ALL to copy structure including indexes
pub async fn create_shadow_table(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<(), CowError> {
    // Check if shadow already exists
    let exists = client
        .query_one(
            r#"
        SELECT EXISTS(
            SELECT 1 FROM information_schema.tables
            WHERE table_schema = $1 AND table_name = $2
        )
        "#,
            &[&schema_name, &table_name],
        )
        .await?;

    let shadow_exists: bool = exists.get(0);
    if shadow_exists {
        return Ok(());
    }

    // Create shadow table with same structure
    client
        .execute(
            &format!(
                "CREATE TABLE {}.{} (LIKE public.{} INCLUDING ALL)",
                schema_name, table_name, table_name
            ),
            &[],
        )
        .await?;

    tracing::debug!("Created shadow table {}.{}", schema_name, table_name);

    Ok(())
}

/// Ensure shadow table exists, using advisory lock to prevent races
pub async fn ensure_shadow_table(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<(), CowError> {
    // Use advisory lock to prevent concurrent creation
    let lock_key = hash_lock_key(schema_name, table_name);

    client
        .execute("SELECT pg_advisory_xact_lock($1)", &[&lock_key])
        .await?;

    // Now safe to create (lock will release when transaction ends)
    create_shadow_table(client, schema_name, table_name).await
}

/// Generate a stable lock key from schema and table name
fn hash_lock_key(schema_name: &str, table_name: &str) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    schema_name.hash(&mut hasher);
    table_name.hash(&mut hasher);
    hasher.finish() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_key_stability() {
        let key1 = hash_lock_key("session_abc", "users");
        let key2 = hash_lock_key("session_abc", "users");
        let key3 = hash_lock_key("session_abc", "orders");

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }
}
