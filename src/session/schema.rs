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

        // Create the schema
        let create_schema = format!("CREATE SCHEMA {}", quote_ident(schema_name));
        client.execute(&create_schema, &[]).await?;

        info!(schema = schema_name, "Created session schema");

        // Create _deletes tracking table
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

        // Create _expected_state tracking table for optimistic locking
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

    /// Create the _mlb_row_hashes table for conflict detection
    /// This table stores MD5 hashes of rows at the time they were first accessed
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

    /// Drop session schema and all its contents
    pub async fn drop_session_schema(&self, schema_name: &str) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        let drop_sql = format!("DROP SCHEMA IF EXISTS {} CASCADE", quote_ident(schema_name));
        client.execute(&drop_sql, &[]).await?;

        info!(schema = schema_name, "Dropped session schema");

        Ok(())
    }

    /// Check if a shadow table already exists in the session schema
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

    /// Create shadow table for copy-on-write semantics
    /// Uses advisory locks to prevent race conditions
    ///
    /// The shadow table copies the structure from the source table and adds:
    /// - `_mlb_op`: VARCHAR tracking the operation type (INSERT/UPDATE)
    /// - `_mlb_ts`: TIMESTAMPTZ tracking when the operation occurred
    ///
    /// The shadow table is named `_shadow_{table_name}` in the session schema.
    #[instrument(skip(self), level = "debug")]
    pub async fn create_shadow_table(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<String> {
        let client = self.pool.get().await?;
        let shadow_name = format!("_shadow_{}", table_name);

        // Generate a deterministic lock key from schema + shadow table name
        let lock_key = Self::advisory_lock_key(session_schema, &shadow_name);

        // Try to acquire advisory lock (will block if another session is creating same table)
        let lock_acquired: bool = client
            .query_one("SELECT pg_try_advisory_lock($1) as acquired", &[&lock_key])
            .await?
            .get("acquired");

        if !lock_acquired {
            // Wait for lock instead
            client
                .execute("SELECT pg_advisory_lock($1)", &[&lock_key])
                .await?;
        }

        // Check if table already exists (another session may have created it)
        let shadow_table = format!("{}.{}", quote_ident(session_schema), quote_ident(&shadow_name));
        let exists = self.table_exists(&client, session_schema, &shadow_name).await?;

        if exists {
            debug!(
                session_schema = session_schema,
                shadow_table = shadow_name,
                "Shadow table already exists"
            );
            // Release lock
            client
                .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                .await?;
            return Ok(shadow_table);
        }

        // Create shadow table with same structure as source, plus tracking columns
        let source_table = format!("{}.{}", quote_ident(source_schema), quote_ident(table_name));
        let create_sql = format!(
            r#"CREATE TABLE {} (
                LIKE {} INCLUDING ALL,
                _mlb_op VARCHAR(10) NOT NULL,
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
                // Release lock before returning error
                let _ = client
                    .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                    .await;
                return Err(SchemaError::Database(e));
            }
        }

        // Release advisory lock
        client
            .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
            .await?;

        Ok(shadow_table)
    }

    /// Create UNION ALL view combining shadow and source tables
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

        // View that:
        // 1. Shows all rows from shadow table (modified rows)
        // 2. Shows source rows that are NOT in shadow AND NOT deleted
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

    /// Get the primary key column for a table
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

    /// Check if a table exists in a schema
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

    /// Generate a deterministic advisory lock key from schema + table name
    fn advisory_lock_key(schema_name: &str, table_name: &str) -> i64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        schema_name.hash(&mut hasher);
        table_name.hash(&mut hasher);
        hasher.finish() as i64
    }

    /// List all tables in a schema (excluding internal tracking tables)
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

    /// Get all primary key columns for a table (supports composite keys)
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
            return Err(SchemaError::PrimaryKeyValueMismatch(format!(
                "{}.{}",
                source_schema, table_name
            )));
        }

        let columns = rows
            .iter()
            .map(|r| PrimaryKeyColumn {
                name: r.get("column_name"),
                data_type: r.get("data_type"),
            })
            .collect();

        debug!(
            schema = source_schema,
            table = table_name,
            column_count = rows.len(),
            "Retrieved primary key columns"
        );

        Ok(columns)
    }

    /// Create a deleted tracking table for a specific source table.
    /// The deleted table stores just the primary key column(s) to track which rows were deleted.
    #[instrument(skip(self), fields(session = %session_schema, source = %source_schema, table = %table_name))]
    pub async fn create_deleted_table(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<String> {
        let client = self.pool.get().await?;

        // Get primary key columns from source table
        let pk_columns = self.get_primary_key_columns(source_schema, table_name).await?;

        let deleted_table_name = format!("_deleted_{}", table_name);
        let full_table_name = format!(
            "{}.{}",
            quote_ident(session_schema),
            quote_ident(&deleted_table_name)
        );

        // Build column definitions for PK columns
        let pk_column_defs: Vec<String> = pk_columns
            .iter()
            .map(|col| format!("{} {}", quote_ident(&col.name), col.data_type))
            .collect();

        // Build PRIMARY KEY constraint
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

        client.execute(&create_sql, &[]).await?;

        info!(
            session_schema = session_schema,
            table = table_name,
            deleted_table = deleted_table_name,
            pk_columns = ?pk_column_names,
            "Created deleted tracking table"
        );

        Ok(full_table_name)
    }

    /// Check if a deleted tracking table exists for a given source table
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
    /// Creates the deleted table if it doesn't exist.
    #[instrument(skip(self, pk_value), fields(session = %session_schema, source = %source_schema, table = %table_name))]
    pub async fn record_deletion(
        &self,
        session_schema: &str,
        source_schema: &str,
        table_name: &str,
        pk_value: &PkValue,
    ) -> SchemaResult<()> {
        let client = self.pool.get().await?;

        // Ensure deleted table exists
        let deleted_table_name = format!("_deleted_{}", table_name);
        let table_exists = self
            .table_exists(&client, session_schema, &deleted_table_name)
            .await?;

        if !table_exists {
            self.create_deleted_table(session_schema, source_schema, table_name)
                .await?;
        }

        // Get PK columns to know column names for insert
        let pk_columns = self
            .get_primary_key_columns(source_schema, table_name)
            .await?;

        let full_table_name = format!(
            "{}.{}",
            quote_ident(session_schema),
            quote_ident(&deleted_table_name)
        );

        // Build column names list
        let column_names: Vec<String> = pk_columns
            .iter()
            .map(|col| quote_ident(&col.name))
            .collect();

        // Get values based on PkValue type
        let values: Vec<&str> = match pk_value {
            PkValue::Single(v) => vec![v.as_str()],
            PkValue::Composite(vs) => vs.iter().map(|s| s.as_str()).collect(),
        };

        if values.len() != pk_columns.len() {
            return Err(SchemaError::PrimaryKeyValueMismatch(format!(
                "PK value count mismatch: expected {} values for {}.{}, got {}",
                pk_columns.len(),
                source_schema,
                table_name,
                values.len()
            )));
        }

        // Build parameterized INSERT with ON CONFLICT DO NOTHING (idempotent)
        let placeholders: Vec<String> = (1..=values.len()).map(|i| format!("${}", i)).collect();

        let insert_sql = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT DO NOTHING",
            full_table_name,
            column_names.join(", "),
            placeholders.join(", ")
        );

        // Convert values to params - all as TEXT since Postgres will cast
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            values.iter().map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();

        client.execute(&insert_sql, &params).await?;

        info!(
            session_schema = session_schema,
            table = table_name,
            "Recorded deletion"
        );

        Ok(())
    }
}

/// Quote an identifier to prevent SQL injection
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
        // Verify the naming convention for deleted tables
        let table_name = "users";
        let deleted_table_name = format!("_deleted_{}", table_name);
        assert_eq!(deleted_table_name, "_deleted_users");

        let table_name = "order_items";
        let deleted_table_name = format!("_deleted_{}", table_name);
        assert_eq!(deleted_table_name, "_deleted_order_items");
    }

    #[test]
    fn test_quote_ident_with_special_chars() {
        // Table names with special characters should be properly escaped
        assert_eq!(quote_ident("table-name"), "\"table-name\"");
        assert_eq!(quote_ident("123numeric"), "\"123numeric\"");
        assert_eq!(quote_ident("UPPERCASE"), "\"UPPERCASE\"");
        assert_eq!(quote_ident("mixed_Case-Name"), "\"mixed_Case-Name\"");
    }

    #[test]
    fn test_pk_column_sql_generation() {
        // Test that we can build proper SQL column definitions
        let columns = vec![
            PrimaryKeyColumn {
                name: "tenant_id".to_string(),
                data_type: "uuid".to_string(),
            },
            PrimaryKeyColumn {
                name: "user_id".to_string(),
                data_type: "bigint".to_string(),
            },
        ];

        let pk_column_defs: Vec<String> = columns
            .iter()
            .map(|col| format!("{} {}", quote_ident(&col.name), col.data_type))
            .collect();

        assert_eq!(pk_column_defs.len(), 2);
        assert_eq!(pk_column_defs[0], "\"tenant_id\" uuid");
        assert_eq!(pk_column_defs[1], "\"user_id\" bigint");

        let pk_column_names: Vec<String> = columns
            .iter()
            .map(|col| quote_ident(&col.name))
            .collect();

        assert_eq!(pk_column_names.join(", "), "\"tenant_id\", \"user_id\"");
    }

    #[test]
    fn test_insert_placeholders_generation() {
        // Test placeholder generation for parameterized queries
        let num_columns = 3;
        let placeholders: Vec<String> = (1..=num_columns).map(|i| format!("${}", i)).collect();
        assert_eq!(placeholders.join(", "), "$1, $2, $3");

        let num_columns = 1;
        let placeholders: Vec<String> = (1..=num_columns).map(|i| format!("${}", i)).collect();
        assert_eq!(placeholders.join(", "), "$1");
    }

    #[test]
    fn test_shadow_table_naming() {
        // Verify the shadow table naming convention
        let table_name = "users";
        let shadow_name = format!("_shadow_{}", table_name);
        assert_eq!(shadow_name, "_shadow_users");
    }

    #[test]
    fn test_shadow_table_name_with_underscores() {
        // Shadow table name preserves original table name
        let table_name = "user_accounts";
        let shadow_name = format!("_shadow_{}", table_name);
        assert_eq!(shadow_name, "_shadow_user_accounts");
    }

    #[test]
    fn test_shadow_table_full_qualified_name() {
        // Test fully qualified shadow table name generation
        let session_schema = "mlb_session_abc123";
        let table_name = "orders";
        let shadow_name = format!("_shadow_{}", table_name);
        let full_name = format!(
            "{}.{}",
            quote_ident(session_schema),
            quote_ident(&shadow_name)
        );
        assert_eq!(full_name, "\"mlb_session_abc123\".\"_shadow_orders\"");
    }
}
