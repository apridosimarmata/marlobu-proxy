use deadpool_postgres::Pool;
use thiserror::Error;
use tracing::{debug, info, instrument};

/// Represents a primary key column with its name and PostgreSQL data type
#[derive(Debug, Clone)]
pub struct PrimaryKeyColumn {
    pub name: String,
    pub data_type: String,
}

/// Represents a primary key value for recording deletions
#[derive(Debug, Clone)]
pub enum PkValue {
    /// Single column primary key
    Single(String),
    /// Composite primary key - values in same order as columns
    Composite(Vec<String>),
}

#[derive(Error, Debug)]
pub enum SchemaError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("Table not found: {0}")]
    TableNotFound(String),

    #[error("Failed to acquire advisory lock for table: {0}")]
    LockFailed(String),

    #[error("Primary key not found for table: {0}")]
    PrimaryKeyNotFound(String),

    #[error("Primary key value count mismatch: {0}")]
    PrimaryKeyValueMismatch(String),

    #[error("Invalid data type from catalog: {0}")]
    InvalidDataType(String),
}

pub type SchemaResult<T> = Result<T, SchemaError>;

/// Handles schema and shadow table creation
pub struct SchemaManager {
    pool: Pool,
}

impl SchemaManager {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Create session schema with tracking tables
    pub async fn create_session_schema(&self, schema_name: &str) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        let create_schema = format!("CREATE SCHEMA {}", quote_ident(schema_name));
        client.execute(&create_schema, &[]).await?;

        info!(schema = schema_name, "Created session schema");

        let create_deletes = format!(
            r#"
            CREATE TABLE {}._deletes (
                table_name TEXT NOT NULL,
                row_id TEXT NOT NULL,
                deleted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (table_name, row_id)
            )
            "#,
            quote_ident(schema_name)
        );
        client.execute(&create_deletes, &[]).await?;

        let create_expected = format!(
            r#"
            CREATE TABLE {}._expected_state (
                table_name TEXT NOT NULL,
                row_id TEXT NOT NULL,
                state_hash TEXT NOT NULL,
                captured_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (table_name, row_id)
            )
            "#,
            quote_ident(schema_name)
        );
        client.execute(&create_expected, &[]).await?;

        debug!(schema = schema_name, "Created tracking tables");

        Ok(())
    }

    pub async fn create_hash_table(&self, schema_name: &str) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        let create_hash_table = format!(
            r#"
            CREATE TABLE IF NOT EXISTS {}._mlb_row_hashes (
                table_name VARCHAR(255) NOT NULL,
                pk_value VARCHAR(255) NOT NULL,
                hash VARCHAR(32) NOT NULL,
                captured_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (table_name, pk_value)
            )
            "#,
            quote_ident(schema_name)
        );
        client.execute(&create_hash_table, &[]).await?;

        info!(schema = schema_name, "Created _mlb_row_hashes table");

        Ok(())
    }

    pub async fn drop_session_schema(&self, schema_name: &str) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        let drop_sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(schema_name));
        client.execute(&drop_sql, &[]).await?;

        info!(schema = schema_name, "Dropped session schema");

        Ok(())
    }

    #[instrument(skip(self), level = "debug")]
    pub async fn shadow_table_exists(
        &self,
        session_schema: &str,
        table_name: &str,
    ) -> SchemaResult<bool> {
        let client = self.pool.get().await?;
        let shadow_name = format!("_shadow_{}", table_name);
        self.table_exists(&client, session_schema, &shadow_name).await
    }

    #[instrument(skip(self), level = "debug")]
    pub async fn create_shadow_table(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<String> {
        let client = self.pool.get().await?;
        let shadow_name = format!("_shadow_{}", table_name);

        let lock_key = Self::advisory_lock_key(session_schema, &shadow_name);

        let lock_acquired: bool = client
            .query_one("SELECT pg_try_advisory_lock($1) as acquired", &[&lock_key])
            .await?
            .get("acquired");

        if !lock_acquired {
            client
                .execute("SELECT pg_advisory_lock($1)", &[&lock_key])
                .await?;
        }

        let shadow_table = format!("{}.{}", quote_ident(session_schema), quote_ident(&shadow_name));
        let exists = match self.table_exists(&client, session_schema, &shadow_name).await {
            Ok(e) => e,
            Err(e) => {
                // Release lock before returning error to prevent deadlocks
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(e);
            }
        };

        if exists {
            debug!(
                session_schema = session_schema,
                shadow_table = shadow_name,
                "Shadow table already exists"
            );
            client
                .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                .await?;
            return Ok(shadow_table);
        }

        let source_table = format!("{}.{}", quote_ident(source_schema), quote_ident(table_name));
        let create_sql = format!(
            r#"CREATE TABLE {} (
                LIKE {} INCLUDING ALL,
                _mlb_op VARCHAR(10) NOT NULL DEFAULT 'WRITE',
                _mlb_ts TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )"#,
            shadow_table, source_table
        );

        match client.execute(&create_sql, &[]).await {
            Ok(_) => {
                info!(
                    session_schema = session_schema,
                    source_schema = source_schema,
                    source_table = table_name,
                    shadow_table = shadow_name,
                    "Created shadow table with tracking columns (_mlb_op, _mlb_ts)"
                );
            }
            Err(e) => {
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(SchemaError::Database(e));
            }
        }

        client
            .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
            .await?;

        Ok(shadow_table)
    }

    pub async fn create_union_view(
        &self,
        schema_name: &str,
        source_schema: &str,
        table_name: &str,
        primary_key: &str,
    ) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        let view_name = format!(
            "{}._view_{}",
            quote_ident(schema_name),
            quote_ident(table_name)
        );
        let shadow_table = format!("{}.{}", quote_ident(schema_name), quote_ident(table_name));
        let source_table = format!("{}.{}", quote_ident(source_schema), quote_ident(table_name));
        let deletes_table = format!("{}._deletes", quote_ident(schema_name));

        let create_view = format!(
            r#"
            CREATE OR REPLACE VIEW {} AS
            SELECT * FROM {}
            UNION ALL
            SELECT s.* FROM {} s
            WHERE NOT EXISTS (
                SELECT 1 FROM {} sh WHERE sh.{pk} = s.{pk}
            )
            AND NOT EXISTS (
                SELECT 1 FROM {} d WHERE d.table_name = '{}' AND d.row_id = s.{pk}::TEXT
            )
            "#,
            view_name,
            shadow_table,
            source_table,
            shadow_table,
            deletes_table,
            table_name,
            pk = quote_ident(primary_key)
        );

        client.execute(&create_view, &[]).await?;

        debug!(
            schema = schema_name,
            table = table_name,
            "Created union view"
        );

        Ok(())
    }

    pub async fn get_primary_key(
        &self,
        schema_name: &str,
        table_name: &str,
    ) -> SchemaResult<String> {
        let client = self.pool.get().await?;

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
            None => Err(SchemaError::PrimaryKeyNotFound(format!(
                "{}.{}",
                schema_name, table_name
            ))),
        }
    }

    async fn table_exists(
        &self,
        client: &deadpool_postgres::Client,
        schema_name: &str,
        table_name: &str,
    ) -> SchemaResult<bool> {
        let row = client
            .query_one(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM information_schema.tables
                    WHERE table_schema = $1 AND table_name = $2
                ) as exists
                "#,
                &[&schema_name, &table_name],
            )
            .await?;

        Ok(row.get("exists"))
    }

    fn advisory_lock_key(schema_name: &str, table_name: &str) -> i64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        schema_name.hash(&mut hasher);
        table_name.hash(&mut hasher);
        hasher.finish() as i64
    }

    pub async fn list_tables(&self, schema_name: &str) -> SchemaResult<Vec<String>> {
        let client = self.pool.get().await?;

        let rows = client
            .query(
                r#"
                SELECT table_name
                FROM information_schema.tables
                WHERE table_schema = $1
                  AND table_type = 'BASE TABLE'
                  AND table_name NOT LIKE '\_%'
                ORDER BY table_name
                "#,
                &[&schema_name],
            )
            .await?;

        Ok(rows.iter().map(|r| r.get("table_name")).collect())
    }

    #[instrument(skip(self), fields(schema = %source_schema, table = %table_name))]
    pub async fn get_primary_key_columns(
        &self,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<Vec<PrimaryKeyColumn>> {
        let client = self.pool.get().await?;

        let rows = client
            .query(
                r#"
                SELECT a.attname as column_name,
                       pg_catalog.format_type(a.atttypid, a.atttypmod) as data_type
                FROM pg_index i
                JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
                JOIN pg_class c ON c.oid = i.indrelid
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE i.indisprimary
                  AND n.nspname = $1
                  AND c.relname = $2
                ORDER BY array_position(i.indkey, a.attnum)
                "#,
                &[&source_schema, &table_name],
            )
            .await?;

        if rows.is_empty() {
            return Err(SchemaError::PrimaryKeyNotFound(format!(
                "{}.{}",
                source_schema, table_name
            )));
        }

        let mut columns = Vec::with_capacity(rows.len());
        for r in rows.iter() {
            let data_type: String = r.get("data_type");
            // Validate data type to prevent SQL injection via compromised catalogs
            validate_pg_data_type(&data_type)?;
            columns.push(PrimaryKeyColumn {
                name: r.get("column_name"),
                data_type,
            });
        }

        debug!(
            schema = source_schema,
            table = table_name,
            column_count = columns.len(),
            "Retrieved primary key columns"
        );

        Ok(columns)
    }

    /// Create a deleted tracking table for a specific source table.
    /// Uses advisory locks to prevent race conditions during concurrent table creation.
    #[instrument(skip(self), fields(session = %session_schema, source = %source_schema, table = %table_name))]
    pub async fn create_deleted_table(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<(String, Vec<PrimaryKeyColumn>)> {
        let client = self.pool.get().await?;

        let deleted_table_name = format!("_deleted_{}", table_name);

        // Use advisory lock to prevent race conditions
        let lock_key = Self::advisory_lock_key(session_schema, &deleted_table_name);

        let lock_acquired: bool = client
            .query_one("SELECT pg_try_advisory_lock($1) as acquired", &[&lock_key])
            .await?
            .get("acquired");

        if !lock_acquired {
            client
                .execute("SELECT pg_advisory_lock($1)", &[&lock_key])
                .await?;
        }

        let exists = match self
            .table_exists(&client, session_schema, &deleted_table_name)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                // Release lock before returning error to prevent deadlocks
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(e);
            }
        };

        let pk_columns = match self.get_primary_key_columns(source_schema, table_name).await {
            Ok(cols) => cols,
            Err(e) => {
                // Release lock before returning error to prevent deadlocks
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(e);
            }
        };

        let full_table_name = format!(
            "{}.{}",
            quote_ident(session_schema),
            quote_ident(&deleted_table_name)
        );

        if exists {
            debug!(
                session_schema = session_schema,
                deleted_table = deleted_table_name,
                "Deleted table already exists"
            );
            client
                .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                .await?;
            return Ok((full_table_name, pk_columns));
        }

        // Revalidate data_type immediately before SQL interpolation
        for col in &pk_columns {
            validate_pg_data_type(&col.data_type)?;
        }

        let pk_column_defs: Vec<String> = pk_columns
            .iter()
            .map(|col| format!("{} {}", quote_ident(&col.name), &col.data_type))
            .collect();

        let pk_column_names: Vec<String> = pk_columns
            .iter()
            .map(|col| quote_ident(&col.name))
            .collect();

        let create_sql = format!(
            r#"
            CREATE TABLE {} (
                {},
                _mlb_ts TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY ({})
            )
            "#,
            full_table_name,
            pk_column_defs.join(", "),
            pk_column_names.join(", ")
        );

        match client.execute(&create_sql, &[]).await {
            Ok(_) => {
                info!(
                    session_schema = session_schema,
                    table = table_name,
                    deleted_table = deleted_table_name,
                    pk_columns = ?pk_column_names,
                    "Created deleted tracking table"
                );
            }
            Err(e) => {
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(SchemaError::Database(e));
            }
        }

        client
            .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
            .await?;

        Ok((full_table_name, pk_columns))
    }

    #[instrument(skip(self), fields(session = %session_schema, table = %table_name))]
    pub async fn deleted_table_exists(
        &self,
        session_schema: &str,
        table_name: &str,
    ) -> SchemaResult<bool> {
        let client = self.pool.get().await?;

        let deleted_table_name = format!("_deleted_{}", table_name);

        let exists = self
            .table_exists(&client, session_schema, &deleted_table_name)
            .await?;

        debug!(
            session_schema = session_schema,
            table = table_name,
            deleted_table = deleted_table_name,
            exists = exists,
            "Checked deleted table existence"
        );

        Ok(exists)
    }

    /// Record a deletion in the deleted tracking table.
    /// Returns the PK columns for potential caching by the caller.
    #[instrument(skip(self, pk_value), fields(session = %session_schema, source = %source_schema, table = %table_name))]
    pub async fn record_deletion(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
        pk_value: &PkValue,
    ) -> SchemaResult<Vec<PrimaryKeyColumn>> {
        let client = self.pool.get().await?;

        let deleted_table_name = format!("_deleted_{}", table_name);

        let table_exists = self
            .table_exists(&client, session_schema, &deleted_table_name)
            .await?;

        // Get PK columns - from create_deleted_table (returns them) or fetch once
        let pk_columns = if !table_exists {
            let (_, cols) = self
                .create_deleted_table(session_schema, source_schema, table_name)
                .await?;
            cols
        } else {
            self.get_primary_key_columns(source_schema, table_name)
                .await?
        };

        let full_table_name = format!(
            "{}.{}",
            quote_ident(session_schema),
            quote_ident(&deleted_table_name)
        );

        let column_names: Vec<String> = pk_columns
            .iter()
            .map(|col| quote_ident(&col.name))
            .collect();

        let values: Vec<&str> = match pk_value {
            PkValue::Single(v) => vec![v.as_str()],
            PkValue::Composite(vs) => vs.iter().map(|s| s.as_str()).collect(),
        };

        if values.len() != pk_columns.len() {
            return Err(SchemaError::PrimaryKeyValueMismatch(format!(
                "Expected {} PK values for {}.{}, got {}",
                pk_columns.len(),
                source_schema,
                table_name,
                values.len()
            )));
        }

        // Revalidate data_type immediately before SQL interpolation
        for col in &pk_columns {
            validate_pg_data_type(&col.data_type)?;
        }

        // Use explicit casts to handle non-text PKs (integers, UUIDs, etc.)
        let placeholders: Vec<String> = pk_columns
            .iter()
            .enumerate()
            .map(|(i, col)| format!("${}::{}", i + 1, col.data_type))
            .collect();

        let insert_sql = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT DO NOTHING",
            full_table_name,
            column_names.join(", "),
            placeholders.join(", ")
        );

        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            values.iter().map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();

        client.execute(&insert_sql, &params).await?;

        info!(
            session_schema = session_schema,
            table = table_name,
            "Recorded deletion"
        );

        Ok(pk_columns)
    }
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Validate that a data type string from pg_catalog is safe to use in SQL.
fn validate_pg_data_type(data_type: &str) -> Result<(), SchemaError> {
    let trimmed = data_type.trim();

    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(SchemaError::InvalidDataType(data_type.to_string()));
    }

    // Check for SQL injection patterns (symbols that shouldn't appear in type names)
    let forbidden_patterns = [";", "--", "/*", "*/"];
    for pattern in &forbidden_patterns {
        if trimmed.contains(pattern) {
            return Err(SchemaError::InvalidDataType(data_type.to_string()));
        }
    }

    // Check for SQL keywords as whole words (word boundaries), not substrings.
    // This allows valid types like tsvector, tsquery which contain "select" as substring.
    let lower = trimmed.to_lowercase();
    let forbidden_keywords = ["drop", "delete", "insert", "update", "select", "exec", "execute", "alter", "create", "grant", "revoke", "union", "from", "join", "where", "having", "group", "order"];
    for keyword in &forbidden_keywords {
        if is_standalone_word(&lower, keyword) {
            return Err(SchemaError::InvalidDataType(data_type.to_string()));
        }
    }

    let valid_chars = |c: char| {
        c.is_ascii_alphanumeric()
            || c == ' '
            || c == '_'
            || c == '('
            || c == ')'
            || c == '['
            || c == ']'
            || c == ','
            || c == '.'
    };

    if !trimmed.chars().all(valid_chars) {
        return Err(SchemaError::InvalidDataType(data_type.to_string()));
    }

    Ok(())
}

/// Check if a keyword appears as a standalone word in the input string.
/// A standalone word is surrounded by non-alphanumeric characters or string boundaries.
fn is_standalone_word(input: &str, keyword: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = input[start..].find(keyword) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + keyword.len();

        // Check character before the match (if any)
        let before_ok = abs_pos == 0 || !input[..abs_pos].chars().last().unwrap_or(' ').is_alphanumeric();

        // Check character after the match (if any)
        let after_ok = end_pos >= input.len() || !input[end_pos..].chars().next().unwrap_or(' ').is_alphanumeric();

        if before_ok && after_ok {
            return true;
        }

        start = abs_pos + 1;
        if start >= input.len() {
            break;
        }
    }
    false
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
    fn test_advisory_lock_key_deterministic() {
        let key1 = SchemaManager::advisory_lock_key("session_123", "users");
        let key2 = SchemaManager::advisory_lock_key("session_123", "users");
        assert_eq!(key1, key2);

        let key3 = SchemaManager::advisory_lock_key("session_123", "orders");
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_pk_value_single() {
        let pk = PkValue::Single("123".to_string());
        match pk {
            PkValue::Single(v) => assert_eq!(v, "123"),
            PkValue::Composite(_) => panic!("Expected Single variant"),
        }
    }

    #[test]
    fn test_pk_value_composite() {
        let pk = PkValue::Composite(vec!["abc".to_string(), "456".to_string()]);
        match pk {
            PkValue::Single(_) => panic!("Expected Composite variant"),
            PkValue::Composite(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], "abc");
                assert_eq!(v[1], "456");
            }
        }
    }

    #[test]
    fn test_primary_key_column() {
        let col = PrimaryKeyColumn {
            name: "user_id".to_string(),
            data_type: "integer".to_string(),
        };
        assert_eq!(col.name, "user_id");
        assert_eq!(col.data_type, "integer");
    }

    #[test]
    fn test_deleted_table_name_format() {
        let table_name = "users";
        let deleted_table_name = format!("_deleted_{}", table_name);
        assert_eq!(deleted_table_name, "_deleted_users");
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
        // Types containing SQL keywords as substrings should be valid
        assert!(validate_pg_data_type("tsvector").is_ok());
        assert!(validate_pg_data_type("tsquery").is_ok());
        assert!(validate_pg_data_type("pg_catalog.tsvector").is_ok());
    }

    #[test]
    fn test_validate_pg_data_type_invalid() {
        assert!(validate_pg_data_type("integer; DROP TABLE users").is_err());
        assert!(validate_pg_data_type("text--comment").is_err());
        assert!(validate_pg_data_type("integer/*comment*/").is_err());
        assert!(validate_pg_data_type("text' OR '1'='1").is_err());
        assert!(validate_pg_data_type("").is_err());
        // Standalone SQL keywords should be rejected
        assert!(validate_pg_data_type("select * from users").is_err());
        assert!(validate_pg_data_type("integer drop").is_err());
        assert!(validate_pg_data_type("delete from t").is_err());
    }

    #[test]
    fn test_validate_pg_data_type_too_long() {
        let long_type = "a".repeat(200);
        assert!(validate_pg_data_type(&long_type).is_err());
    }

    #[test]
    fn test_is_standalone_word() {
        // Keyword as standalone word
        assert!(is_standalone_word("select foo", "select"));
        assert!(is_standalone_word("foo select bar", "select"));
        assert!(is_standalone_word("foo select", "select"));
        assert!(is_standalone_word("select", "select"));

        // Keyword as substring (not standalone)
        assert!(!is_standalone_word("tsvector", "select"));
        assert!(!is_standalone_word("tsquery", "select"));
        assert!(!is_standalone_word("preselected", "select"));
        assert!(!is_standalone_word("selectivity", "select"));
    }
}
