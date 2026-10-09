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
            schema = schema_name,
            table = table.table_name,
            pk = table.primary_key,
            columns = table.hash_columns.join(", "),
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
                schema = schema_name,
                table = table.table_name,
                pk = table.primary_key,
                pk_type = table.primary_key_type,
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

        // Handle NULL values and type coercion for stable hashing
        match dtype.as_str() {
            "jsonb" | "json" => format!("COALESCE({}::text, '\\x00')", col),
            "timestamp with time zone" | "timestamp without time zone" => {
                format!("COALESCE(to_char({}, 'YYYY-MM-DD HH24:MI:SS.US'), '\\x00')", col)
            }
            "numeric" | "decimal" | "real" | "double precision" => {
                format!("COALESCE({}::numeric::text, '\\x00')", col)
            }
            _ => format!("COALESCE({}::text, '\\x00')", col),
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
}
