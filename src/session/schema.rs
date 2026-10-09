use deadpool_postgres::Pool;
use thiserror::Error;
use tracing::{debug, info};

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
                table_name TEXT NOT NULL,
                pk_value TEXT NOT NULL,
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

    /// Create shadow table for copy-on-write semantics
    /// Uses advisory locks to prevent race conditions
    pub async fn create_shadow_table(
        &self,
        schema_name: &str,
        source_schema: &str,
        table_name: &str,
    ) -> SchemaResult<String> {
        let client = self.pool.get().await?;

        // Generate a deterministic lock key from schema + table
        let lock_key = Self::advisory_lock_key(schema_name, table_name);

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
        let shadow_table = format!("{}.{}", quote_ident(schema_name), quote_ident(table_name));
        let exists = match self.table_exists(&client, schema_name, table_name).await {
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
                schema = schema_name,
                table = table_name,
                "Shadow table already exists"
            );
            // Release lock
            client
                .execute("SELECT pg_advisory_unlock($1)", &[&lock_key])
                .await?;
            return Ok(shadow_table);
        }

        // Create shadow table with same structure as source
        let source_table = format!("{}.{}", quote_ident(source_schema), quote_ident(table_name));
        let create_sql = format!(
            "CREATE TABLE {} (LIKE {} INCLUDING ALL)",
            shadow_table, source_table
        );

        match client.execute(&create_sql, &[]).await {
            Ok(_) => {
                info!(
                    schema = schema_name,
                    table = table_name,
                    "Created shadow table"
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
}
