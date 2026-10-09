use serde::Serialize;
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub table: String,
    pub row_id: String,
    pub expected_hash: String,
    pub actual_hash: Option<String>,
    pub conflict_type: ConflictType,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictType {
    RowModified,
    RowDeleted,
    InsertCollision,
}

#[derive(Debug, Error)]
pub enum ConflictError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Conflicts detected")]
    ConflictsDetected(Vec<Conflict>),

    #[error("Primary key not found for table: {0}")]
    PrimaryKeyNotFound(String),
}

/// Check for conflicts between expected state and current production state
///
/// Uses explicit column concatenation for stable hashing (not row_to_json which is unstable)
pub async fn check_conflicts(
    client: &tokio_postgres::Client,
    schema_name: &str,
    tables: &[TableConflictCheck],
) -> Result<Vec<Conflict>, ConflictError> {
    let mut conflicts = Vec::new();

    for table in tables {
        // Check for modified/deleted rows
        let rows = client.query(&format!(r#"
            SELECT
                es.row_id,
                es.state_hash as expected_hash,
                CASE
                    WHEN p.{pk} IS NULL THEN NULL
                    ELSE md5(concat_ws('|', {columns}))
                END as actual_hash
            FROM {schema}._expected_state es
            LEFT JOIN public.{table} p ON p.{pk}::text = es.row_id
            WHERE es.table_name = $1
        "#,
            schema = quote_ident(schema_name),
            table = quote_ident(&table.table_name),
            pk = quote_ident(&table.primary_key),
            columns = table.hash_columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", "),
        ), &[&table.table_name]).await?;

        for row in rows {
            let row_id: String = row.get("row_id");
            let expected_hash: String = row.get("expected_hash");
            let actual_hash: Option<String> = row.get("actual_hash");

            match &actual_hash {
                None => {
                    // Row was deleted in prod
                    conflicts.push(Conflict {
                        table: table.table_name.clone(),
                        row_id,
                        expected_hash,
                        actual_hash: None,
                        conflict_type: ConflictType::RowDeleted,
                    });
                }
                Some(actual) if actual != &expected_hash => {
                    // Row was modified in prod
                    conflicts.push(Conflict {
                        table: table.table_name.clone(),
                        row_id,
                        expected_hash,
                        actual_hash: Some(actual.clone()),
                        conflict_type: ConflictType::RowModified,
                    });
                }
                _ => {
                    // No conflict
                }
            }
        }

        // Check for insert collisions (if we have inserts)
        if table.check_insert_collisions {
            let collision_rows = client.query(&format!(r#"
                SELECT s.{pk}::text as row_id
                FROM {schema}.{table} s
                INNER JOIN public.{table} p ON p.{pk} = s.{pk}
                WHERE s.{pk} NOT IN (
                    SELECT row_id::{pk_type} FROM {schema}._expected_state
                    WHERE table_name = $1
                )
            "#,
                schema = quote_ident(schema_name),
                table = quote_ident(&table.table_name),
                pk = quote_ident(&table.primary_key),
                pk_type = quote_ident(&table.primary_key_type),
            ), &[&table.table_name]).await?;

            for row in collision_rows {
                let row_id: String = row.get("row_id");
                conflicts.push(Conflict {
                    table: table.table_name.clone(),
                    row_id,
                    expected_hash: String::new(),
                    actual_hash: None,
                    conflict_type: ConflictType::InsertCollision,
                });
            }
        }
    }

    Ok(conflicts)
}

#[derive(Debug, Clone)]
pub struct TableConflictCheck {
    pub table_name: String,
    pub primary_key: String,
    pub primary_key_type: String,
    pub hash_columns: Vec<String>,
    pub check_insert_collisions: bool,
}

/// Build hash column list for a table
/// Explicitly lists columns to avoid row_to_json instability
pub async fn get_hash_columns(
    client: &tokio_postgres::Client,
    table_name: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let rows = client.query(r#"
        SELECT column_name, data_type
        FROM information_schema.columns
        WHERE table_schema = 'public' AND table_name = $1
        ORDER BY ordinal_position
    "#, &[&table_name]).await?;

    let columns: Vec<String> = rows.iter().map(|row| {
        let col: String = row.get("column_name");
        let dtype: String = row.get("data_type");
        let quoted_col = quote_ident(&col);

        // Handle NULL values and type coercion for stable hashing
        match dtype.as_str() {
            "jsonb" | "json" => format!("COALESCE({}::text, '\\x00')", quoted_col),
            "timestamp with time zone" | "timestamp without time zone" => {
                format!("COALESCE(to_char({}, 'YYYY-MM-DD HH24:MI:SS.US'), '\\x00')", quoted_col)
            }
            "numeric" | "decimal" | "real" | "double precision" => {
                format!("COALESCE({}::numeric::text, '\\x00')", quoted_col)
            }
            _ => format!("COALESCE({}::text, '\\x00')", quoted_col),
        }
    }).collect();

    Ok(columns)
}

/// Quote an identifier to prevent SQL injection
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Capture the MD5 hash of a row at the time of first read/write
/// This is used for conflict detection during approval
pub async fn capture_row_hash(
    client: &tokio_postgres::Client,
    session_schema: &str,
    table_name: &str,
    pk_column: &str,
    pk_value: &str,
    source_schema: &str,
) -> Result<Option<String>, ConflictError> {
    debug!(
        session_schema = session_schema,
        table = table_name,
        pk_column = pk_column,
        pk_value = pk_value,
        "Capturing row hash"
    );

    // Check if we already have a hash for this row (avoid duplicate captures)
    let existing = client
        .query_opt(
            &format!(
                r#"
                SELECT hash FROM {}._mlb_row_hashes
                WHERE table_name = $1 AND pk_value = $2
                "#,
                quote_ident(session_schema)
            ),
            &[&table_name, &pk_value],
        )
        .await?;

    if let Some(row) = existing {
        let hash: String = row.get("hash");
        debug!(
            table = table_name,
            pk_value = pk_value,
            hash = %hash,
            "Row hash already captured"
        );
        return Ok(Some(hash));
    }

    // Get hash columns for stable, deterministic hashing
    let hash_columns = get_hash_columns(client, table_name).await?;
    if hash_columns.is_empty() {
        warn!(table = table_name, "No columns found for hash computation");
        return Ok(None);
    }

    // Calculate MD5 hash using explicit column concatenation (stable across schema changes)
    let hash_query = format!(
        r#"
        SELECT MD5(concat_ws('|', {})) as hash
        FROM {}.{} t
        WHERE t.{} = $1
        "#,
        hash_columns.join(", "),
        quote_ident(source_schema),
        quote_ident(table_name),
        quote_ident(pk_column)
    );

    let hash_result = client.query_opt(&hash_query, &[&pk_value]).await?;

    let hash = match hash_result {
        Some(row) => {
            let h: String = row.get("hash");
            h
        }
        None => {
            // Row doesn't exist in source - this is valid for new inserts
            debug!(
                table = table_name,
                pk_value = pk_value,
                "Row not found in source schema (may be a new insert)"
            );
            return Ok(None);
        }
    };

    // Store the hash in _mlb_row_hashes
    let insert_query = format!(
        r#"
        INSERT INTO {}._mlb_row_hashes (table_name, pk_value, hash, captured_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (table_name, pk_value) DO NOTHING
        "#,
        quote_ident(session_schema)
    );

    client
        .execute(&insert_query, &[&table_name, &pk_value, &hash])
        .await?;

    info!(
        table = table_name,
        pk_value = pk_value,
        hash = %hash,
        "Captured row hash"
    );

    Ok(Some(hash))
}

/// Row hash conflict information
#[derive(Debug, Clone, Serialize)]
pub struct RowHashConflict {
    pub table: String,
    pub pk_value: String,
    pub captured_hash: String,
    pub current_hash: Option<String>,
    pub conflict_type: ConflictType,
}

/// Check for conflicts between captured row hashes and current production state
/// Returns a list of conflicts where rows have been modified or deleted since capture
pub async fn check_row_hash_conflicts(
    client: &tokio_postgres::Client,
    session_schema: &str,
    source_schema: &str,
) -> Result<Vec<RowHashConflict>, ConflictError> {
    info!(
        session_schema = session_schema,
        source_schema = source_schema,
        "Checking for row hash conflicts"
    );

    let mut conflicts = Vec::new();

    // Get all tracked row hashes
    let tracked_rows = client
        .query(
            &format!(
                r#"
                SELECT table_name, pk_value, hash as captured_hash
                FROM {}._mlb_row_hashes
                ORDER BY table_name, pk_value
                "#,
                quote_ident(session_schema)
            ),
            &[],
        )
        .await?;

    debug!(count = tracked_rows.len(), "Found tracked row hashes");

    // Group by table for efficient querying
    let mut table_rows: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();

    for row in &tracked_rows {
        let table_name: String = row.get("table_name");
        let pk_value: String = row.get("pk_value");
        let captured_hash: String = row.get("captured_hash");

        table_rows
            .entry(table_name)
            .or_default()
            .push((pk_value, captured_hash));
    }

    // Check each table's rows
    for (table_name, rows) in table_rows {
        // Get primary key column for this table
        let pk_col = get_primary_key_column(client, source_schema, &table_name).await?;

        // Get hash columns for stable, deterministic hashing
        let hash_columns = get_hash_columns(client, &table_name).await
            .map_err(ConflictError::Database)?;

        if hash_columns.is_empty() {
            warn!(table = %table_name, "No columns found for hash computation, skipping");
            continue;
        }

        for (pk_value, captured_hash) in rows {
            // Calculate current hash using explicit column concatenation (stable across schema changes)
            let current_hash_query = format!(
                r#"
                SELECT MD5(concat_ws('|', {})) as hash
                FROM {}.{} t
                WHERE t.{}::TEXT = $1
                "#,
                hash_columns.join(", "),
                quote_ident(source_schema),
                quote_ident(&table_name),
                quote_ident(&pk_col)
            );

            let current_result = client.query_opt(&current_hash_query, &[&pk_value]).await?;

            match current_result {
                Some(row) => {
                    let current_hash: String = row.get("hash");
                    if current_hash != captured_hash {
                        warn!(
                            table = %table_name,
                            pk_value = %pk_value,
                            captured_hash = %captured_hash,
                            current_hash = %current_hash,
                            "Row modified conflict detected"
                        );
                        conflicts.push(RowHashConflict {
                            table: table_name.clone(),
                            pk_value,
                            captured_hash,
                            current_hash: Some(current_hash),
                            conflict_type: ConflictType::RowModified,
                        });
                    }
                }
                None => {
                    warn!(
                        table = %table_name,
                        pk_value = %pk_value,
                        "Row deleted conflict detected"
                    );
                    conflicts.push(RowHashConflict {
                        table: table_name.clone(),
                        pk_value,
                        captured_hash,
                        current_hash: None,
                        conflict_type: ConflictType::RowDeleted,
                    });
                }
            }
        }
    }

    info!(
        conflict_count = conflicts.len(),
        "Completed row hash conflict check"
    );

    Ok(conflicts)
}

/// Get the primary key column name for a table
async fn get_primary_key_column(
    client: &tokio_postgres::Client,
    schema_name: &str,
    table_name: &str,
) -> Result<String, ConflictError> {
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
        None => Err(ConflictError::PrimaryKeyNotFound(format!(
            "{}.{}",
            schema_name, table_name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_serialization() {
        let conflict = Conflict {
            table: "users".to_string(),
            row_id: "5".to_string(),
            expected_hash: "abc123".to_string(),
            actual_hash: Some("def456".to_string()),
            conflict_type: ConflictType::RowModified,
        };

        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("row_modified"));
    }

    #[test]
    fn test_row_hash_conflict_serialization() {
        let conflict = RowHashConflict {
            table: "orders".to_string(),
            pk_value: "123".to_string(),
            captured_hash: "abc123def456".to_string(),
            current_hash: Some("789xyz000111".to_string()),
            conflict_type: ConflictType::RowModified,
        };

        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("row_modified"));
        assert!(json.contains("orders"));
        assert!(json.contains("123"));
        assert!(json.contains("abc123def456"));
        assert!(json.contains("789xyz000111"));
    }

    #[test]
    fn test_row_hash_conflict_deleted_serialization() {
        let conflict = RowHashConflict {
            table: "users".to_string(),
            pk_value: "42".to_string(),
            captured_hash: "deadbeef".to_string(),
            current_hash: None,
            conflict_type: ConflictType::RowDeleted,
        };

        let json = serde_json::to_string(&conflict).unwrap();
        assert!(json.contains("row_deleted"));
        assert!(json.contains("\"current_hash\":null"));
    }

    #[test]
    fn test_quote_ident_simple() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("my_table"), "\"my_table\"");
    }

    #[test]
    fn test_quote_ident_with_special_chars() {
        assert_eq!(quote_ident("with space"), "\"with space\"");
        assert_eq!(quote_ident("with\"quote"), "\"with\"\"quote\"");
    }

    #[test]
    fn test_conflict_error_display() {
        let err = ConflictError::PrimaryKeyNotFound("public.users".to_string());
        assert_eq!(
            err.to_string(),
            "Primary key not found for table: public.users"
        );
    }
}
