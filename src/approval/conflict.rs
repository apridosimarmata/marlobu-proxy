use serde::Serialize;
use thiserror::Error;

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

    #[error("Invalid data type: {0}")]
    InvalidDataType(String),
}

/// Quote an identifier to prevent SQL injection
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Validate a PostgreSQL data type string to prevent SQL injection.
/// Only allows characters that can appear in valid PostgreSQL type names.
fn validate_pg_data_type(data_type: &str) -> Result<&str, ConflictError> {
    let trimmed = data_type.trim();

    // Reject empty or overly long type names
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(ConflictError::InvalidDataType(data_type.to_string()));
    }

    // Reject single quotes which could be used for SQL injection
    if trimmed.contains('\'') {
        return Err(ConflictError::InvalidDataType(data_type.to_string()));
    }

    // Only allow characters that can appear in valid PostgreSQL type names:
    // - alphanumeric, underscore (type names like int4, varchar)
    // - space (e.g., "character varying", "timestamp with time zone")
    // - parentheses and digits (e.g., "varchar(255)", "numeric(10,2)")
    // - brackets (e.g., "integer[]")
    // - comma (e.g., "numeric(10,2)")
    // - dot (e.g., "pg_catalog.int4")
    let is_valid = trimmed.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || c == '_'
            || c == ' '
            || c == '('
            || c == ')'
            || c == '['
            || c == ']'
            || c == ','
            || c == '.'
    });

    if !is_valid {
        return Err(ConflictError::InvalidDataType(data_type.to_string()));
    }

    Ok(trimmed)
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
            // Validate pk_type to prevent SQL injection
            let validated_pk_type = validate_pg_data_type(&table.primary_key_type)?;

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
                pk_type = validated_pk_type,
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
    fn test_validate_pg_data_type_valid() {
        assert!(validate_pg_data_type("integer").is_ok());
        assert!(validate_pg_data_type("bigint").is_ok());
        assert!(validate_pg_data_type("character varying(255)").is_ok());
        assert!(validate_pg_data_type("numeric(10,2)").is_ok());
        assert!(validate_pg_data_type("timestamp with time zone").is_ok());
        assert!(validate_pg_data_type("integer[]").is_ok());
        assert!(validate_pg_data_type("pg_catalog.int4").is_ok());
    }

    #[test]
    fn test_validate_pg_data_type_invalid() {
        // SQL injection attempts
        assert!(validate_pg_data_type("integer; DROP TABLE users").is_err());
        assert!(validate_pg_data_type("text' OR '1'='1").is_err());
        assert!(validate_pg_data_type("").is_err());
    }
}
