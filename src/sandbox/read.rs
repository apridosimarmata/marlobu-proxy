#![allow(dead_code)]
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReadError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Session not found")]
    SessionNotFound,

    #[error("Table not found: {0}")]
    TableNotFound(String),

    #[error("No primary key found for table: {0}")]
    NoPrimaryKeyFound(String),
}

/// Ensure a view exists for reading a table in the sandbox
///
/// Views provide the merged view of shadow + prod data:
/// - Rows modified in shadow (takes precedence)
/// - Rows from prod not in shadow and not deleted
pub async fn ensure_read_view(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
    primary_key: &str,
) -> Result<(), ReadError> {
    // Check if view already exists
    let exists = client
        .query_one(
            r#"
        SELECT EXISTS(
            SELECT 1 FROM information_schema.views
            WHERE table_schema = $1 AND table_name = $2
        )
        "#,
            &[&schema_name, &format!("{}_view", table_name)],
        )
        .await?;

    let view_exists: bool = exists.get(0);
    if view_exists {
        return Ok(());
    }

    // Check if shadow table exists
    let shadow_exists = client
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

    let has_shadow: bool = shadow_exists.get(0);

    if has_shadow {
        // Create view with shadow + prod merge
        client
            .execute(
                &format!(
                    r#"
            CREATE OR REPLACE VIEW {schema}.{table}_view AS
            SELECT * FROM {schema}.{table}
            UNION ALL
            SELECT p.* FROM public.{table} p
            WHERE p.{pk} NOT IN (SELECT {pk} FROM {schema}.{table})
              AND p.{pk}::text NOT IN (
                  SELECT row_id FROM {schema}._deletes WHERE table_name = '{table}'
              )
        "#,
                    schema = schema_name,
                    table = table_name,
                    pk = primary_key,
                ),
                &[],
            )
            .await?;
    } else {
        // No shadow yet - view just points to prod minus deletes
        client
            .execute(
                &format!(
                    r#"
            CREATE OR REPLACE VIEW {schema}.{table}_view AS
            SELECT p.* FROM public.{table} p
            WHERE p.{pk}::text NOT IN (
                SELECT row_id FROM {schema}._deletes WHERE table_name = '{table}'
            )
        "#,
                    schema = schema_name,
                    table = table_name,
                    pk = primary_key,
                ),
                &[],
            )
            .await?;
    }

    Ok(())
}

/// Get the primary key column for a table
pub async fn get_primary_key(
    client: &tokio_postgres::Client,
    table_name: &str,
) -> Result<String, ReadError> {
    let row = client
        .query_opt(
            r#"
        SELECT a.attname
        FROM pg_index i
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
        WHERE i.indrelid = $1::regclass AND i.indisprimary
        LIMIT 1
        "#,
            &[&format!("public.{}", table_name)],
        )
        .await?;

    row.map(|r| r.get("attname"))
        .ok_or_else(|| ReadError::NoPrimaryKeyFound(table_name.to_string()))
}
