//! View SQL generation for sandbox isolation.
//!
//! Generates CREATE VIEW statements that union base table data with shadow table
//! modifications, implementing copy-on-write semantics.

use crate::rewriter::tables::view_name_for_table;
use deadpool_postgres::Pool;
use thiserror::Error;
use tracing::{debug, info};

/// Error type for view operations.
#[derive(Error, Debug)]
pub enum ViewError {
    #[error("Database error: {0}")]
    Database(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("No columns found for table: {0}.{1}")]
    NoColumnsFound(String, String),
}

pub type ViewResult<T> = Result<T, ViewError>;

/// Quote an identifier to prevent SQL injection and handle special characters.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Generates SQL to create a union view that merges shadow table changes with production data.
///
/// The view:
/// 1. Selects all rows from the shadow table (session's modifications)
/// 2. Selects production rows NOT modified or deleted in the session
///
/// # Arguments
/// * `session_schema` - Schema containing the session's shadow/deleted tables
/// * `source_schema` - Schema containing the production tables
/// * `table_name` - Name of the table
/// * `pk_columns` - Primary key column(s) for the table
/// * `columns` - Column names to include (excluding _mlb_* metadata columns)
///
/// # Example output:
/// ```sql
/// CREATE OR REPLACE VIEW "session_abc"."users" AS
/// SELECT "id", "name", "email" FROM "session_abc"."_shadow_users"
/// UNION ALL
/// SELECT "id", "name", "email" FROM "public"."users"
/// WHERE "id" NOT IN (SELECT "id" FROM "session_abc"."_shadow_users")
///   AND "id" NOT IN (SELECT "id" FROM "session_abc"."_deleted_users")
/// ```
pub fn generate_union_view_sql(
    session_schema: &str,
    source_schema: &str,
    table_name: &str,
    pk_columns: &[String],
    columns: &[String],
) -> String {
    let quoted_session_schema = quote_ident(session_schema);
    let quoted_source_schema = quote_ident(source_schema);
    let quoted_table = quote_ident(table_name);
    let shadow_table = quote_ident(&format!("_shadow_{}", table_name));
    let deleted_table = quote_ident(&format!("_deleted_{}", table_name));

    // Quote all column names
    let quoted_columns: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    let column_list = quoted_columns.join(", ");

    // Build the NOT IN conditions for composite primary keys
    let pk_not_in_shadow = generate_pk_not_in_clause(pk_columns, &quoted_session_schema, &shadow_table);
    let pk_not_in_deleted = generate_pk_not_in_clause(pk_columns, &quoted_session_schema, &deleted_table);

    format!(
        r#"CREATE OR REPLACE VIEW {session_schema}.{table_name} AS
-- Shadow table rows (session's changes)
SELECT {columns} FROM {session_schema}.{shadow_table}
UNION ALL
-- Production rows NOT modified or deleted in session
SELECT {columns} FROM {source_schema}.{table_name}
WHERE {pk_not_in_shadow}
  AND {pk_not_in_deleted}"#,
        session_schema = quoted_session_schema,
        source_schema = quoted_source_schema,
        table_name = quoted_table,
        shadow_table = shadow_table,
        columns = column_list,
        pk_not_in_shadow = pk_not_in_shadow,
        pk_not_in_deleted = pk_not_in_deleted,
    )
}

/// Generates the NOT IN clause for primary key filtering.
/// Handles both single and composite primary keys.
fn generate_pk_not_in_clause(pk_columns: &[String], schema: &str, table: &str) -> String {
    if pk_columns.len() == 1 {
        // Simple case: single column PK
        let pk = quote_ident(&pk_columns[0]);
        format!("{pk} NOT IN (SELECT {pk} FROM {schema}.{table})")
    } else {
        // Composite PK: use row comparison
        let quoted_pks: Vec<String> = pk_columns.iter().map(|c| quote_ident(c)).collect();
        let pk_tuple = quoted_pks.join(", ");
        format!(
            "({pk_tuple}) NOT IN (SELECT {pk_tuple} FROM {schema}.{table})"
        )
    }
}

/// Fetches column names for a table from information_schema, excluding _mlb_* metadata columns.
pub async fn fetch_table_columns(
    pool: &Pool,
    schema: &str,
    table_name: &str,
) -> ViewResult<Vec<String>> {
    let client = pool.get().await?;

    let rows = client
        .query(
            r#"
            SELECT column_name
            FROM information_schema.columns
            WHERE table_schema = $1
              AND table_name = $2
              AND column_name NOT LIKE '_mlb_%'
            ORDER BY ordinal_position
            "#,
            &[&schema, &table_name],
        )
        .await?;

    let columns: Vec<String> = rows.iter().map(|r| r.get("column_name")).collect();

    if columns.is_empty() {
        return Err(ViewError::NoColumnsFound(
            schema.to_string(),
            table_name.to_string(),
        ));
    }

    debug!(
        schema = schema,
        table = table_name,
        column_count = columns.len(),
        "Fetched table columns"
    );

    Ok(columns)
}

/// Fetches primary key columns for a table.
pub async fn fetch_primary_key_columns(
    pool: &Pool,
    schema: &str,
    table_name: &str,
) -> ViewResult<Vec<String>> {
    let client = pool.get().await?;

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
            ORDER BY array_position(i.indkey, a.attnum)
            "#,
            &[&schema, &table_name],
        )
        .await?;

    Ok(rows.iter().map(|r| r.get("column_name")).collect())
}

/// Creates a union view that merges shadow table changes with production data.
///
/// This function:
/// 1. Queries information_schema to get column names (excluding _mlb_* columns)
/// 2. Generates the view SQL
/// 3. Executes the CREATE VIEW statement
///
/// # Arguments
/// * `pool` - Database connection pool
/// * `session_schema` - Schema containing the session's shadow/deleted tables
/// * `source_schema` - Schema containing the production tables (to get column info)
/// * `table_name` - Name of the table
/// * `pk_columns` - Primary key column(s)
pub async fn create_union_view(
    pool: &Pool,
    session_schema: &str,
    source_schema: &str,
    table_name: &str,
    pk_columns: &[String],
) -> ViewResult<()> {
    // Fetch columns from the source schema (production table)
    let columns = fetch_table_columns(pool, source_schema, table_name).await?;

    let sql = generate_union_view_sql(
        session_schema,
        source_schema,
        table_name,
        pk_columns,
        &columns,
    );

    debug!(
        session_schema = session_schema,
        table = table_name,
        "Creating union view"
    );

    let client = pool.get().await?;
    client.execute(&sql, &[]).await?;

    info!(
        session_schema = session_schema,
        source_schema = source_schema,
        table = table_name,
        "Created union view"
    );

    Ok(())
}

/// Drops a union view from the session schema.
///
/// # Arguments
/// * `pool` - Database connection pool
/// * `session_schema` - Schema containing the view
/// * `table_name` - Name of the table (view name will be the same)
pub async fn drop_union_view(
    pool: &Pool,
    session_schema: &str,
    table_name: &str,
) -> ViewResult<()> {
    let quoted_schema = quote_ident(session_schema);
    let quoted_table = quote_ident(table_name);

    let sql = format!(
        "DROP VIEW IF EXISTS {}.{} CASCADE",
        quoted_schema, quoted_table
    );

    debug!(
        session_schema = session_schema,
        table = table_name,
        "Dropping union view"
    );

    let client = pool.get().await?;
    client.execute(&sql, &[]).await?;

    info!(
        session_schema = session_schema,
        table = table_name,
        "Dropped union view"
    );

    Ok(())
}

/// Metadata about a table for view generation.
#[derive(Debug, Clone)]
pub struct TableInfo {
    /// Original table name (without schema)
    pub name: String,
    /// Column names in order
    pub columns: Vec<String>,
    /// Primary key column(s)
    pub primary_key: Vec<String>,
}

/// Generates a CREATE VIEW statement for sandbox isolation.
///
/// The view implements copy-on-write by:
/// 1. Selecting from shadow table where rows exist
/// 2. Selecting from base table where no shadow row exists and not soft-deleted
///
/// # Example output:
/// ```sql
/// CREATE OR REPLACE VIEW sandbox_123.users_view AS
/// SELECT id, name, email FROM sandbox_123.users
/// UNION ALL
/// SELECT id, name, email FROM public.users base
/// WHERE NOT EXISTS (
///     SELECT 1 FROM sandbox_123.users shadow
///     WHERE shadow.id = base.id
/// )
/// AND NOT EXISTS (
///     SELECT 1 FROM sandbox_123._deleted_users del
///     WHERE del.id = base.id
/// );
/// ```
pub fn generate_view_sql(
    schema: &str,
    base_schema: &str,
    table: &TableInfo,
) -> String {
    let view_name = view_name_for_table(&table.name);
    let columns = table.columns.join(", ");
    let pk_conditions = generate_pk_conditions(&table.primary_key, "shadow", "base");
    let del_pk_conditions = generate_pk_conditions(&table.primary_key, "del", "base");

    format!(
        r#"CREATE OR REPLACE VIEW {schema}.{view_name} AS
SELECT {columns} FROM {schema}.{table_name}
UNION ALL
SELECT {columns} FROM {base_schema}.{table_name} base
WHERE NOT EXISTS (
    SELECT 1 FROM {schema}.{table_name} shadow
    WHERE {pk_conditions}
)
AND NOT EXISTS (
    SELECT 1 FROM {schema}._deleted_{table_name} del
    WHERE {del_pk_conditions}
)"#,
        schema = schema,
        view_name = view_name,
        table_name = table.name,
        base_schema = base_schema,
        columns = columns,
        pk_conditions = pk_conditions,
        del_pk_conditions = del_pk_conditions,
    )
}

/// Generates a CREATE TABLE statement for the shadow table.
///
/// Shadow tables mirror the base table structure exactly.
pub fn generate_shadow_table_sql(
    schema: &str,
    base_schema: &str,
    table_name: &str,
) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {schema}.{table_name} (LIKE {base_schema}.{table_name} INCLUDING ALL)",
        schema = schema,
        table_name = table_name,
        base_schema = base_schema,
    )
}

/// Generates a CREATE TABLE statement for the deletion tracking table.
///
/// Stores primary keys of rows "deleted" in this sandbox.
pub fn generate_deleted_table_sql(
    schema: &str,
    table: &TableInfo,
) -> String {
    let pk_columns: Vec<String> = table.primary_key.iter()
        .map(|col| format!("{} TEXT NOT NULL", col))
        .collect();

    let pk_constraint = format!("PRIMARY KEY ({})", table.primary_key.join(", "));

    format!(
        "CREATE TABLE IF NOT EXISTS {schema}._deleted_{table_name} (\n    {columns},\n    {pk_constraint}\n)",
        schema = schema,
        table_name = table.name,
        columns = pk_columns.join(",\n    "),
        pk_constraint = pk_constraint,
    )
}

/// Generates DROP statements for sandbox cleanup.
pub fn generate_cleanup_sql(schema: &str, table_name: &str) -> Vec<String> {
    vec![
        format!("DROP VIEW IF EXISTS {}.{}_view CASCADE", schema, table_name),
        format!("DROP TABLE IF EXISTS {}.{} CASCADE", schema, table_name),
        format!("DROP TABLE IF EXISTS {}._deleted_{} CASCADE", schema, table_name),
    ]
}

/// Generates the full setup SQL for a table in a sandbox.
pub fn generate_table_setup_sql(
    schema: &str,
    base_schema: &str,
    table: &TableInfo,
) -> Vec<String> {
    vec![
        generate_shadow_table_sql(schema, base_schema, &table.name),
        generate_deleted_table_sql(schema, table),
        generate_view_sql(schema, base_schema, table),
    ]
}

/// Helper to generate PK equality conditions between two table aliases.
fn generate_pk_conditions(pk_columns: &[String], left_alias: &str, right_alias: &str) -> String {
    pk_columns.iter()
        .map(|col| format!("{}.{} = {}.{}", left_alias, col, right_alias, col))
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_table() -> TableInfo {
        TableInfo {
            name: "users".to_string(),
            columns: vec!["id".to_string(), "name".to_string(), "email".to_string()],
            primary_key: vec!["id".to_string()],
        }
    }

    #[test]
    fn test_generate_view_sql() {
        let table = sample_table();
        let sql = generate_view_sql("sandbox_123", "public", &table);

        assert!(sql.contains("CREATE OR REPLACE VIEW sandbox_123.users_view"));
        assert!(sql.contains("FROM sandbox_123.users"));
        assert!(sql.contains("FROM public.users base"));
        assert!(sql.contains("shadow.id = base.id"));
        assert!(sql.contains("_deleted_users"));
    }

    #[test]
    fn test_generate_shadow_table_sql() {
        let sql = generate_shadow_table_sql("sandbox_123", "public", "users");

        assert!(sql.contains("CREATE TABLE IF NOT EXISTS sandbox_123.users"));
        assert!(sql.contains("LIKE public.users INCLUDING ALL"));
    }

    #[test]
    fn test_generate_deleted_table_sql() {
        let table = sample_table();
        let sql = generate_deleted_table_sql("sandbox_123", &table);

        assert!(sql.contains("_deleted_users"));
        assert!(sql.contains("id TEXT NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (id)"));
    }

    #[test]
    fn test_composite_primary_key() {
        let table = TableInfo {
            name: "order_items".to_string(),
            columns: vec!["order_id".to_string(), "item_id".to_string(), "quantity".to_string()],
            primary_key: vec!["order_id".to_string(), "item_id".to_string()],
        };

        let view_sql = generate_view_sql("sandbox_123", "public", &table);
        assert!(view_sql.contains("shadow.order_id = base.order_id AND shadow.item_id = base.item_id"));

        let deleted_sql = generate_deleted_table_sql("sandbox_123", &table);
        assert!(deleted_sql.contains("PRIMARY KEY (order_id, item_id)"));
    }

    #[test]
    fn test_cleanup_sql() {
        let stmts = generate_cleanup_sql("sandbox_123", "users");

        assert_eq!(stmts.len(), 3);
        assert!(stmts[0].contains("DROP VIEW"));
        assert!(stmts[1].contains("DROP TABLE IF EXISTS sandbox_123.users"));
        assert!(stmts[2].contains("_deleted_users"));
    }

    // Tests for the new union view generator functions

    #[test]
    fn test_generate_union_view_sql_single_pk() {
        let columns = vec![
            "id".to_string(),
            "name".to_string(),
            "email".to_string(),
        ];
        let pk_columns = vec!["id".to_string()];

        let sql = generate_union_view_sql(
            "session_abc",
            "public",
            "users",
            &pk_columns,
            &columns,
        );

        // Check view creation
        assert!(sql.contains(r#"CREATE OR REPLACE VIEW "session_abc"."users""#));

        // Check shadow table select
        assert!(sql.contains(r#"FROM "session_abc"."_shadow_users""#));

        // Check production table select
        assert!(sql.contains(r#"FROM "public"."users""#));

        // Check column list is quoted
        assert!(sql.contains(r#""id", "name", "email""#));

        // Check NOT IN clauses for shadow and deleted tables
        assert!(sql.contains(r#""id" NOT IN (SELECT "id" FROM "session_abc"."_shadow_users")"#));
        assert!(sql.contains(r#""id" NOT IN (SELECT "id" FROM "session_abc"."_deleted_users")"#));
    }

    #[test]
    fn test_generate_union_view_sql_composite_pk() {
        let columns = vec![
            "order_id".to_string(),
            "product_id".to_string(),
            "quantity".to_string(),
            "price".to_string(),
        ];
        let pk_columns = vec!["order_id".to_string(), "product_id".to_string()];

        let sql = generate_union_view_sql(
            "session_xyz",
            "public",
            "order_items",
            &pk_columns,
            &columns,
        );

        // Check composite PK handling with row comparison
        assert!(sql.contains(
            r#"("order_id", "product_id") NOT IN (SELECT "order_id", "product_id" FROM "session_xyz"."_shadow_order_items")"#
        ));
        assert!(sql.contains(
            r#"("order_id", "product_id") NOT IN (SELECT "order_id", "product_id" FROM "session_xyz"."_deleted_order_items")"#
        ));
    }

    #[test]
    fn test_generate_union_view_sql_special_characters() {
        let columns = vec![
            "id".to_string(),
            "user name".to_string(),  // space in column name
            "data\"field".to_string(), // quote in column name
        ];
        let pk_columns = vec!["id".to_string()];

        let sql = generate_union_view_sql(
            "session-with-dash",
            "my schema",
            "table\"name",
            &pk_columns,
            &columns,
        );

        // Check proper quoting of special characters
        assert!(sql.contains(r#""session-with-dash""#));
        assert!(sql.contains(r#""my schema""#));
        assert!(sql.contains(r#""table""name""#)); // double-quote escaped
        assert!(sql.contains(r#""user name""#));
        assert!(sql.contains(r#""data""field""#)); // double-quote escaped
    }

    #[test]
    fn test_quote_ident() {
        assert_eq!(quote_ident("simple"), r#""simple""#);
        assert_eq!(quote_ident("with space"), r#""with space""#);
        assert_eq!(quote_ident("with\"quote"), r#""with""quote""#);
        assert_eq!(quote_ident("multi\"\"quotes"), r#""multi""""quotes""#);
    }

    #[test]
    fn test_generate_pk_not_in_clause_single() {
        let pk_columns = vec!["id".to_string()];
        let clause = generate_pk_not_in_clause(&pk_columns, r#""schema""#, r#""table""#);

        assert_eq!(
            clause,
            r#""id" NOT IN (SELECT "id" FROM "schema"."table")"#
        );
    }

    #[test]
    fn test_generate_pk_not_in_clause_composite() {
        let pk_columns = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let clause = generate_pk_not_in_clause(&pk_columns, r#""s""#, r#""t""#);

        assert_eq!(
            clause,
            r#"("a", "b", "c") NOT IN (SELECT "a", "b", "c" FROM "s"."t")"#
        );
    }
}
