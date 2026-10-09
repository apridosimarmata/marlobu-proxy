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

/// Query all mutations for a session by scanning shadow and deleted tables
#[instrument(skip(pool), fields(schema = %schema_name))]
pub async fn get_session_mutations(
    pool: &Pool,
    schema_name: &str,
) -> MutationResult<Vec<MutationRecord>> {
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
            let query = format!(
                r#"
                SELECT {pk}::text as row_id, _mlb_op as operation, _mlb_ts as timestamp
                FROM {schema}.{table}
                ORDER BY _mlb_ts
                "#,
                pk = quote_ident(&pk_col),
                schema = quote_ident(schema_name),
                table = quote_ident(&shadow_table_name),
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
        let pk_columns = get_pk_columns_for_deleted_table(&client, schema_name, &deleted_table_name).await?;

        if !pk_columns.is_empty() {
            // Build row_id as concatenation of PK columns for composite keys
            let row_id_expr = if pk_columns.len() == 1 {
                format!("{}::text", quote_ident(&pk_columns[0]))
            } else {
                let parts: Vec<String> = pk_columns
                    .iter()
                    .map(|col| format!("{}::text", quote_ident(col)))
                    .collect();
                format!("concat_ws(':', {})", parts.join(", "))
            };

            let query = format!(
                r#"
                SELECT {row_id_expr} as row_id, _mlb_ts as timestamp
                FROM {schema}.{table}
                ORDER BY _mlb_ts
                "#,
                row_id_expr = row_id_expr,
                schema = quote_ident(schema_name),
                table = quote_ident(&deleted_table_name),
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
    mutations.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

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

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_ident() {
        assert_eq!(quote_ident("simple"), "\"simple\"");
        assert_eq!(quote_ident("with space"), "\"with space\"");
        assert_eq!(quote_ident("with\"quote"), "\"with\"\"quote\"");
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
