use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum DiffError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("Primary key not found for table: {0}")]
    PrimaryKeyNotFound(String),

    #[error("Session schema not found: {0}")]
    SchemaNotFound(String),

    #[error("Invalid identifier: {0}")]
    InvalidIdentifier(String),
}

/// A single row change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowChange {
    pub row_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<serde_json::Value>,
}

/// Changes for a single table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDiff {
    pub name: String,
    pub inserts: Vec<serde_json::Value>,
    pub updates: Vec<RowChange>,
    pub deletes: Vec<serde_json::Value>,
}

/// Full diff for a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDiff {
    pub session_id: String,
    pub tables: Vec<TableDiff>,
}

/// Generate a human-readable diff for all changes in a session
pub async fn generate_session_diff(
    pool: &deadpool_postgres::Pool,
    schema_name: &str,
    source_schema: &str,
) -> Result<Vec<TableDiff>, DiffError> {
    let client = pool.get().await?;

    // Verify session schema exists
    let schema_exists: bool = client
        .query_one(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM information_schema.schemata
                WHERE schema_name = $1
            ) as exists
            "#,
            &[&schema_name],
        )
        .await?
        .get("exists");

    if !schema_exists {
        return Err(DiffError::SchemaNotFound(schema_name.to_string()));
    }

    // Find all shadow tables in the session schema
    let shadow_tables = client
        .query(
            r#"
            SELECT table_name
            FROM information_schema.tables
            WHERE table_schema = $1
              AND table_type = 'BASE TABLE'
              AND table_name LIKE '_shadow_%'
            ORDER BY table_name
            "#,
            &[&schema_name],
        )
        .await?;

    let mut diffs = Vec::new();

    for row in shadow_tables {
        let shadow_table_name: String = row.get("table_name");
        // Extract original table name from _shadow_<table_name>
        let table_name = shadow_table_name
            .strip_prefix("_shadow_")
            .unwrap_or(&shadow_table_name);

        let table_diff =
            generate_table_diff(&client, schema_name, source_schema, table_name).await?;

        // Only include tables with actual changes
        if !table_diff.inserts.is_empty()
            || !table_diff.updates.is_empty()
            || !table_diff.deletes.is_empty()
        {
            diffs.push(table_diff);
        }
    }

    info!(
        schema = schema_name,
        table_count = diffs.len(),
        "Generated session diff"
    );

    Ok(diffs)
}

/// Generate diff for a single table
async fn generate_table_diff(
    client: &tokio_postgres::Client,
    schema_name: &str,
    source_schema: &str,
    table_name: &str,
) -> Result<TableDiff, DiffError> {
    let pk_column = get_primary_key_column(client, source_schema, table_name).await?;
    let shadow_table = format!(
        "{}.{}",
        safe_quote_ident(schema_name)?,
        safe_quote_ident(&format!("_shadow_{}", table_name))?
    );
    let source_table = format!(
        "{}.{}",
        safe_quote_ident(source_schema)?,
        safe_quote_ident(table_name)?
    );

    // Get columns for the table (excluding _mlb_* tracking columns)
    let columns = get_table_columns(client, source_schema, table_name).await?;
    let column_list: Result<Vec<String>, DiffError> = columns
        .iter()
        .map(|c| safe_quote_ident(c))
        .collect();
    let column_list = column_list?.join(", ");

    // 1. Find INSERTs: rows in shadow that don't exist in source
    let insert_query = format!(
        r#"
        SELECT row_to_json(t.*) as data
        FROM (
            SELECT {columns}
            FROM {shadow} s
            WHERE NOT EXISTS (
                SELECT 1 FROM {source} p WHERE p.{pk} = s.{pk}
            )
        ) t
        "#,
        columns = column_list,
        shadow = shadow_table,
        source = source_table,
        pk = safe_quote_ident(&pk_column)?,
    );

    let insert_rows = client.query(&insert_query, &[]).await?;
    let inserts: Vec<serde_json::Value> = insert_rows.iter().map(|r| r.get("data")).collect();

    debug!(
        table = table_name,
        insert_count = inserts.len(),
        "Found inserts"
    );

    // 2. Find UPDATEs: rows in shadow that also exist in source (with different data)
    let update_query = format!(
        r#"
        SELECT
            s.{pk}::text as row_id,
            row_to_json(p_sel.*) as before,
            row_to_json(s_sel.*) as after
        FROM {shadow} s
        INNER JOIN {source} p ON p.{pk} = s.{pk}
        CROSS JOIN LATERAL (SELECT {columns} FROM {source} WHERE {pk} = s.{pk}) p_sel
        CROSS JOIN LATERAL (SELECT {columns} FROM {shadow} WHERE {pk} = s.{pk}) s_sel
        WHERE s._mlb_op = 'UPDATE' OR s._mlb_op = 'UPSERT'
        "#,
        columns = column_list,
        shadow = shadow_table,
        source = source_table,
        pk = safe_quote_ident(&pk_column)?,
    );

    let update_rows = client.query(&update_query, &[]).await?;
    let updates: Vec<RowChange> = update_rows
        .iter()
        .map(|r| RowChange {
            row_id: r.get("row_id"),
            before: Some(r.get("before")),
            after: Some(r.get("after")),
        })
        .collect();

    debug!(
        table = table_name,
        update_count = updates.len(),
        "Found updates"
    );

    // 3. Find DELETEs: check _deleted_<table> table if it exists
    let deletes = get_deletes(client, schema_name, table_name).await?;

    debug!(
        table = table_name,
        delete_count = deletes.len(),
        "Found deletes"
    );

    Ok(TableDiff {
        name: table_name.to_string(),
        inserts,
        updates,
        deletes,
    })
}

/// Get deleted rows for a table.
/// Reads directly from the _deleted_ table which stores the full row data at deletion time.
async fn get_deletes(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<Vec<serde_json::Value>, DiffError> {
    let deleted_table_name = format!("_deleted_{}", table_name);

    // Check if deleted table exists
    let exists: bool = client
        .query_one(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM information_schema.tables
                WHERE table_schema = $1 AND table_name = $2
            ) as exists
            "#,
            &[&schema_name, &deleted_table_name],
        )
        .await?
        .get("exists");

    if !exists {
        return Ok(Vec::new());
    }

    let deleted_table = format!(
        "{}.{}",
        safe_quote_ident(schema_name)?,
        safe_quote_ident(&deleted_table_name)?
    );

    // Get the deleted row data directly from the _deleted_ table.
    // The _deleted_ table stores the full row snapshot at deletion time,
    // so we don't need to join with source (which may no longer have the row).
    let delete_query = format!(
        r#"
        SELECT row_to_json(d.*) as data
        FROM {deleted} d
        "#,
        deleted = deleted_table,
    );

    let delete_rows = client.query(&delete_query, &[]).await?;
    let deletes: Vec<serde_json::Value> = delete_rows.iter().map(|r| r.get("data")).collect();

    Ok(deletes)
}

/// Get all non-system columns for a table (excluding _mlb_* tracking columns)
async fn get_table_columns(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<Vec<String>, DiffError> {
    let rows = client
        .query(
            r#"
            SELECT column_name
            FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = $2
              AND column_name NOT LIKE '_mlb_%'
            ORDER BY ordinal_position
            "#,
            &[&schema_name, &table_name],
        )
        .await?;

    Ok(rows.iter().map(|r| r.get("column_name")).collect())
}

/// Get the primary key column name for a table
async fn get_primary_key_column(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<String, DiffError> {
    let row = client
        .query_opt(
            r#"
            SELECT a.attname as column_name
            FROM pg_index i
            JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
            JOIN pg_class c ON c.oid = i.indrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE i.indisprimary
              AND n.nspname = $1
              AND c.relname = $2
            LIMIT 1
            "#,
            &[&schema_name, &table_name],
        )
        .await?;

    match row {
        Some(r) => Ok(r.get("column_name")),
        None => Err(DiffError::PrimaryKeyNotFound(format!(
            "{}.{}",
            schema_name, table_name
        ))),
    }
}

/// Validates that an identifier contains only safe characters for SQL identifiers.
fn validate_identifier(ident: &str) -> Result<(), DiffError> {
    if ident.is_empty() {
        return Err(DiffError::InvalidIdentifier("empty string".to_string()));
    }
    if ident.contains('\0') {
        return Err(DiffError::InvalidIdentifier(
            "contains null byte".to_string(),
        ));
    }
    if ident.len() > 63 {
        return Err(DiffError::InvalidIdentifier(
            "exceeds maximum length of 63 characters".to_string(),
        ));
    }
    // Allow only alphanumeric, underscore, and hyphen
    let is_valid = ident
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !is_valid {
        return Err(DiffError::InvalidIdentifier(format!(
            "identifier contains invalid characters: {}",
            ident
        )));
    }
    Ok(())
}

/// Safely quote an identifier after validating it contains only safe characters.
fn safe_quote_ident(ident: &str) -> Result<String, DiffError> {
    validate_identifier(ident)?;
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_diff_serialization() {
        let diff = TableDiff {
            name: "users".to_string(),
            inserts: vec![serde_json::json!({"id": 99, "name": "new user"})],
            updates: vec![RowChange {
                row_id: "5".to_string(),
                before: Some(serde_json::json!({"id": 5, "name": "old"})),
                after: Some(serde_json::json!({"id": 5, "name": "new"})),
            }],
            deletes: vec![serde_json::json!({"id": 10, "name": "deleted user"})],
        };

        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("users"));
        assert!(json.contains("new user"));
        assert!(json.contains("row_id"));
    }

    #[test]
    fn test_session_diff_serialization() {
        let diff = SessionDiff {
            session_id: "abc-123".to_string(),
            tables: vec![TableDiff {
                name: "users".to_string(),
                inserts: vec![],
                updates: vec![],
                deletes: vec![],
            }],
        };

        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("abc-123"));
        assert!(json.contains("users"));
    }

    #[test]
    fn test_row_change_serialization_omits_none() {
        let change = RowChange {
            row_id: "1".to_string(),
            before: None,
            after: Some(serde_json::json!({"id": 1})),
        };

        let json = serde_json::to_string(&change).unwrap();
        assert!(!json.contains("before"));
        assert!(json.contains("after"));
    }

    #[test]
    fn test_safe_quote_ident() {
        assert_eq!(safe_quote_ident("users").unwrap(), "\"users\"");
        assert_eq!(safe_quote_ident("my_table").unwrap(), "\"my_table\"");
        assert_eq!(safe_quote_ident("with-hyphen").unwrap(), "\"with-hyphen\"");
    }

    #[test]
    fn test_safe_quote_ident_rejects_invalid() {
        assert!(safe_quote_ident("with\"quote").is_err());
        assert!(safe_quote_ident("with;semicolon").is_err());
        assert!(safe_quote_ident("").is_err());
    }

    #[test]
    fn test_validate_identifier() {
        assert!(validate_identifier("users").is_ok());
        assert!(validate_identifier("_shadow_users").is_ok());
        assert!(validate_identifier("uuid-123-abc").is_ok());
        assert!(validate_identifier("with space").is_err());
        assert!(validate_identifier("with\"quote").is_err());
        assert!(validate_identifier("").is_err());
    }
}
