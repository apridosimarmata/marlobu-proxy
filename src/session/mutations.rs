use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use serde::Serialize;
use thiserror::Error;
use tracing::{debug, instrument};

#[derive(Error, Debug)]
pub enum MutationError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("Invalid identifier: {0}")]
    InvalidIdentifier(String),
}

pub type MutationResult<T> = Result<T, MutationError>;

/// A single mutation record
#[derive(Debug, Clone, Serialize)]
pub struct MutationRecord {
    pub table: String,
    pub operation: String,
    pub row_id: String,
    pub timestamp: DateTime<Utc>,
}

/// Validates that an identifier contains only safe characters for SQL identifiers.
/// Allows alphanumeric characters, underscores, and hyphens.
/// This prevents SQL injection by rejecting identifiers with special characters
/// that could break out of quoted context.
fn validate_identifier(ident: &str) -> Result<(), MutationError> {
    if ident.is_empty() {
        return Err(MutationError::InvalidIdentifier(
            "identifier cannot be empty".to_string(),
        ));
    }

    // Allow only alphanumeric, underscore, and hyphen (common in UUIDs)
    // This is stricter than PostgreSQL's identifier rules but safe
    let is_valid = ident
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

    if !is_valid {
        return Err(MutationError::InvalidIdentifier(format!(
            "identifier contains invalid characters: {}",
            ident
        )));
    }

    Ok(())
}

/// Safely quotes a SQL identifier after validating it contains only safe characters.
/// Returns an error if the identifier contains potentially dangerous characters.
fn safe_quote_ident(ident: &str) -> Result<String, MutationError> {
    validate_identifier(ident)?;
    // After validation, we know the identifier is safe, but we still quote it
    // for correctness with reserved words and case sensitivity
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

/// Query all mutations for a session by scanning shadow and deleted tables
#[instrument(skip(pool), fields(schema = %schema_name))]
pub async fn get_session_mutations(
    pool: &Pool,
    schema_name: &str,
) -> MutationResult<Vec<MutationRecord>> {
    // Validate schema name upfront - this is user-controlled input
    validate_identifier(schema_name)?;

    let client = pool.get().await?;
    let mut mutations = Vec::new();

    // 1. Find all shadow tables (_shadow_*) and query INSERT/UPDATE operations
    let shadow_tables = client
        .query(
            r#"
            SELECT table_name
            FROM information_schema.tables
            WHERE table_schema = $1
              AND table_type = 'BASE TABLE'
              AND table_name LIKE '_shadow_%'
            "#,
            &[&schema_name],
        )
        .await?;

    for row in shadow_tables {
        let shadow_table_name: String = row.get("table_name");
        let source_table = shadow_table_name
            .strip_prefix("_shadow_")
            .unwrap_or(&shadow_table_name);

        // Get primary key column for this table
        let pk_column = get_primary_key_column(&client, schema_name, &shadow_table_name).await?;

        if let Some(pk_col) = pk_column {
            // Query shadow table for INSERT/UPDATE records
            // All identifiers are validated before interpolation to prevent SQL injection
            let query = format!(
                r#"
                SELECT {pk}::text as row_id, _mlb_op as operation, _mlb_ts as timestamp
                FROM {schema}.{table}
                ORDER BY _mlb_ts
                "#,
                pk = safe_quote_ident(&pk_col)?,
                schema = safe_quote_ident(schema_name)?,
                table = safe_quote_ident(&shadow_table_name)?,
            );

            let records = client.query(&query, &[]).await?;

            for record in records {
                let row_id: String = record.get("row_id");
                let operation: String = record.get("operation");
                let timestamp: DateTime<Utc> = record.get("timestamp");

                mutations.push(MutationRecord {
                    table: source_table.to_string(),
                    operation,
                    row_id,
                    timestamp,
                });
            }
        }
    }

    // 2. Find all deleted tables (_deleted_*) and query DELETE operations
    let deleted_tables = client
        .query(
            r#"
            SELECT table_name
            FROM information_schema.tables
            WHERE table_schema = $1
              AND table_type = 'BASE TABLE'
              AND table_name LIKE '_deleted_%'
            "#,
            &[&schema_name],
        )
        .await?;

    for row in deleted_tables {
        let deleted_table_name: String = row.get("table_name");
        let source_table = deleted_table_name
            .strip_prefix("_deleted_")
            .unwrap_or(&deleted_table_name);

        // Get primary key column(s) for deleted table
        let pk_columns =
            get_pk_columns_for_deleted_table(&client, schema_name, &deleted_table_name).await?;

        if !pk_columns.is_empty() {
            // Build row_id as concatenation of PK columns for composite keys
            // All column names are validated before interpolation
            let row_id_expr = if pk_columns.len() == 1 {
                format!("{}::text", safe_quote_ident(&pk_columns[0])?)
            } else {
                let parts: Result<Vec<String>, MutationError> = pk_columns
                    .iter()
                    .map(|col| Ok(format!("{}::text", safe_quote_ident(col)?)))
                    .collect();
                format!("concat_ws(':', {})", parts?.join(", "))
            };

            let query = format!(
                r#"
                SELECT {row_id_expr} as row_id, _mlb_ts as timestamp
                FROM {schema}.{table}
                ORDER BY _mlb_ts
                "#,
                row_id_expr = row_id_expr,
                schema = safe_quote_ident(schema_name)?,
                table = safe_quote_ident(&deleted_table_name)?,
            );

            let records = client.query(&query, &[]).await?;

            for record in records {
                let row_id: String = record.get("row_id");
                let timestamp: DateTime<Utc> = record.get("timestamp");

                mutations.push(MutationRecord {
                    table: source_table.to_string(),
                    operation: "DELETE".to_string(),
                    row_id,
                    timestamp,
                });
            }
        }
    }

    // Sort all mutations by timestamp
    mutations.sort_by_key(|a| a.timestamp);

    debug!(
        schema = schema_name,
        mutation_count = mutations.len(),
        "Retrieved session mutations"
    );

    Ok(mutations)
}

/// Get the primary key column for a shadow table
async fn get_primary_key_column(
    client: &deadpool_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> MutationResult<Option<String>> {
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

    Ok(row.map(|r| r.get("column_name")))
}

/// Get all primary key columns for a deleted table (supports composite keys)
async fn get_pk_columns_for_deleted_table(
    client: &deadpool_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> MutationResult<Vec<String>> {
    let rows = client
        .query(
            r#"
            SELECT a.attname as column_name
            FROM pg_index i
            JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
            JOIN pg_class c ON c.oid = i.indrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE i.indisprimary
              AND n.nspname = $1
              AND c.relname = $2
              AND a.attname != '_mlb_ts'
            ORDER BY array_position(i.indkey, a.attnum)
            "#,
            &[&schema_name, &table_name],
        )
        .await?;

    Ok(rows.iter().map(|r| r.get("column_name")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_identifier_valid() {
        assert!(validate_identifier("simple").is_ok());
        assert!(validate_identifier("with_underscore").is_ok());
        assert!(validate_identifier("with-hyphen").is_ok());
        assert!(validate_identifier("CamelCase").is_ok());
        assert!(validate_identifier("123numeric").is_ok());
        assert!(validate_identifier("uuid-a1b2c3d4-e5f6").is_ok());
        assert!(validate_identifier("_shadow_users").is_ok());
    }

    #[test]
    fn test_validate_identifier_invalid() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("with space").is_err());
        assert!(validate_identifier("with\"quote").is_err());
        assert!(validate_identifier("with;semicolon").is_err());
        assert!(validate_identifier("with'apostrophe").is_err());
        assert!(validate_identifier("drop table--").is_err());
        assert!(validate_identifier("schema.table").is_err());
    }

    #[test]
    fn test_safe_quote_ident() {
        assert_eq!(safe_quote_ident("simple").unwrap(), "\"simple\"");
        assert_eq!(
            safe_quote_ident("with_underscore").unwrap(),
            "\"with_underscore\""
        );
        assert!(safe_quote_ident("with\"quote").is_err());
        assert!(safe_quote_ident("mal;icious").is_err());
    }

    #[test]
    fn test_mutation_record_serialization() {
        let record = MutationRecord {
            table: "users".to_string(),
            operation: "INSERT".to_string(),
            row_id: "123".to_string(),
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"table\":\"users\""));
        assert!(json.contains("\"operation\":\"INSERT\""));
        assert!(json.contains("\"row_id\":\"123\""));
    }
}
