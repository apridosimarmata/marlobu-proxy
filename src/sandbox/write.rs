use thiserror::Error;

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Session not found")]
    SessionNotFound,

    #[error("Table not found: {0}")]
    TableNotFound(String),

    #[error("Write blocked: {0}")]
    Blocked(String),
}

/// Stage an INSERT operation
///
/// Inserts go directly to the shadow table
pub async fn stage_insert(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
    rewritten_sql: &str,
) -> Result<u64, WriteError> {
    // Shadow table should already exist at this point
    // The rewritten SQL already targets the shadow table
    let result = client.execute(rewritten_sql, &[]).await?;
    Ok(result)
}

/// Stage a DELETE operation
///
/// Records the deletion in _deletes table, removes from shadow if present
pub async fn stage_delete(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
    primary_key: &str,
    where_clause: &str,
) -> Result<u64, WriteError> {
    // 1. Capture expected state for conflict detection
    let capture_sql = format!(
        r#"
        INSERT INTO {schema}._expected_state (table_name, row_id, state_hash)
        SELECT
            '{table}',
            {pk}::text,
            md5(concat_ws('|', {pk}::text))
        FROM public.{table}
        WHERE {where_clause}
        ON CONFLICT (table_name, row_id) DO NOTHING
    "#,
        schema = schema_name,
        table = table_name,
        pk = primary_key,
        where_clause = where_clause,
    );
    client.execute(&capture_sql, &[]).await?;

    // 2. Record deletion in _deletes
    let record_sql = format!(
        r#"
        INSERT INTO {schema}._deletes (table_name, row_id)
        SELECT '{table}', {pk}::text
        FROM public.{table}
        WHERE {where_clause}
        ON CONFLICT DO NOTHING
    "#,
        schema = schema_name,
        table = table_name,
        pk = primary_key,
        where_clause = where_clause,
    );
    let deleted = client.execute(&record_sql, &[]).await?;

    // 3. Also delete from shadow if row exists there
    let shadow_delete = format!(
        r#"
        DELETE FROM {schema}.{table}
        WHERE {where_clause}
    "#,
        schema = schema_name,
        table = table_name,
        where_clause = where_clause,
    );
    let _ = client.execute(&shadow_delete, &[]).await;

    Ok(deleted)
}

/// Check if a statement type should be blocked
pub fn should_block(sql_upper: &str) -> Option<&'static str> {
    let blocked = [
        ("CREATE ", "DDL not allowed in sandbox"),
        ("ALTER ", "DDL not allowed in sandbox"),
        ("DROP ", "DDL not allowed in sandbox"),
        ("TRUNCATE", "TRUNCATE not allowed in sandbox"),
        ("GRANT ", "Permission changes not allowed in sandbox"),
        ("REVOKE ", "Permission changes not allowed in sandbox"),
        ("COPY ", "COPY not allowed in sandbox"),
        ("LISTEN ", "LISTEN not allowed in sandbox"),
        ("NOTIFY ", "NOTIFY not allowed in sandbox"),
        ("LOCK ", "LOCK not allowed in sandbox"),
        ("SET ROLE", "SET ROLE not allowed in sandbox"),
        (
            "SET SESSION AUTHORIZATION",
            "Session auth changes not allowed",
        ),
    ];

    for (pattern, reason) in blocked {
        if sql_upper.contains(pattern) {
            return Some(reason);
        }
    }

    // Also block sequence operations
    if sql_upper.contains("NEXTVAL")
        || sql_upper.contains("SETVAL")
        || sql_upper.contains("CURRVAL")
    {
        return Some("Sequence operations not allowed in sandbox");
    }

    None
}
