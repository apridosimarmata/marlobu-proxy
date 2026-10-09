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

fn validate_identifier(ident: &str) -> Result<(), MutationError> {
    if ident.is_empty() {
        return Err(MutationError::InvalidIdentifier("identifier cannot be empty".to_string()));
    }
    if !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(MutationError::InvalidIdentifier(format!("identifier contains invalid characters: {}", ident)));
    }
    Ok(())
}

fn safe_quote_ident(ident: &str) -> Result<String, MutationError> {
    validate_identifier(ident)?;
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationRecord {
    pub table: String,
    pub operation: String,
    pub row_id: String,
    pub timestamp: DateTime<Utc>,
}

#[instrument(skip(pool), fields(schema = %schema_name))]
pub async fn get_session_mutations(pool: &Pool, schema_name: &str) -> MutationResult<Vec<MutationRecord>> {
    validate_identifier(schema_name)?;
    let client = pool.get().await?;
    let mut mutations = Vec::new();

    let shadow_tables = client.query(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = $1 AND table_type = 'BASE TABLE' AND table_name LIKE '_shadow_%'",
        &[&schema_name],
    ).await?;

    for row in shadow_tables {
        let shadow_table_name: String = row.get("table_name");
        let source_table = shadow_table_name.strip_prefix("_shadow_").unwrap_or(&shadow_table_name);
        if let Some(pk_col) = get_primary_key_column(&client, schema_name, &shadow_table_name).await? {
            let query = format!(
                "SELECT {}::text as row_id, _mlb_op as operation, _mlb_ts as timestamp FROM {}.{} ORDER BY _mlb_ts",
                safe_quote_ident(&pk_col)?, safe_quote_ident(schema_name)?, safe_quote_ident(&shadow_table_name)?
            );
            for record in client.query(&query, &[]).await? {
                mutations.push(MutationRecord {
                    table: source_table.to_string(),
                    operation: record.get("operation"),
                    row_id: record.get("row_id"),
                    timestamp: record.get("timestamp"),
                });
            }
        }
    }

    let deleted_tables = client.query(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = $1 AND table_type = 'BASE TABLE' AND table_name LIKE '_deleted_%'",
        &[&schema_name],
    ).await?;

    for row in deleted_tables {
        let deleted_table_name: String = row.get("table_name");
        let source_table = deleted_table_name.strip_prefix("_deleted_").unwrap_or(&deleted_table_name);
        let pk_columns = get_pk_columns_for_deleted_table(&client, schema_name, &deleted_table_name).await?;
        if !pk_columns.is_empty() {
            let row_id_expr = if pk_columns.len() == 1 {
                format!("{}::text", safe_quote_ident(&pk_columns[0])?)
            } else {
                let parts: Result<Vec<_>, _> = pk_columns.iter().map(|c| Ok(format!("{}::text", safe_quote_ident(c)?))).collect();
                format!("concat_ws(':', {})", parts?.join(", "))
            };
            let query = format!(
                "SELECT {} as row_id, _mlb_ts as timestamp FROM {}.{} ORDER BY _mlb_ts",
                row_id_expr, safe_quote_ident(schema_name)?, safe_quote_ident(&deleted_table_name)?
            );
            for record in client.query(&query, &[]).await? {
                mutations.push(MutationRecord {
                    table: source_table.to_string(),
                    operation: "DELETE".to_string(),
                    row_id: record.get("row_id"),
                    timestamp: record.get("timestamp"),
                });
            }
        }
    }

    mutations.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    debug!(schema = schema_name, mutation_count = mutations.len(), "Retrieved session mutations");
    Ok(mutations)
}

async fn get_primary_key_column(client: &deadpool_postgres::Client, schema_name: &str, table_name: &str) -> MutationResult<Option<String>> {
    let row = client.query_opt(
        "SELECT a.attname as column_name FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2 LIMIT 1",
        &[&schema_name, &table_name],
    ).await?;
    Ok(row.map(|r| r.get("column_name")))
}

async fn get_pk_columns_for_deleted_table(client: &deadpool_postgres::Client, schema_name: &str, table_name: &str) -> MutationResult<Vec<String>> {
    let rows = client.query(
        "SELECT a.attname as column_name FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2 AND a.attname != '_mlb_ts' ORDER BY array_position(i.indkey, a.attnum)",
        &[&schema_name, &table_name],
    ).await?;
    Ok(rows.iter().map(|r| r.get("column_name")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_validate_identifier() {
        assert!(validate_identifier("simple").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("with;semi").is_err());
    }
    #[test]
    fn test_safe_quote_ident() {
        assert_eq!(safe_quote_ident("simple").unwrap(), "\"simple\"");
        assert!(safe_quote_ident("bad;char").is_err());
    }
}
