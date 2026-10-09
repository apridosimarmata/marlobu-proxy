//! View SQL generation for sandbox isolation.
//!
//! Generates CREATE VIEW statements that union base table data with shadow table
//! modifications, implementing copy-on-write semantics.
//!
//! # Security
//!
//! All identifiers (schema names, table names, column names) are quoted using
//! `quote_ident()` which escapes embedded double-quotes per PostgreSQL standard.
//! Identifier values in this module originate from database catalog queries
//! (pg_catalog, information_schema), which are trusted sources.

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

    #[error("No primary key found for table: {0}.{1}")]
    NoPrimaryKey(String, String),

    #[error("Identifier '{0}' is {1} bytes, exceeds PostgreSQL limit of 63 bytes")]
    IdentifierTooLong(String, usize),
}

pub type ViewResult<T> = Result<T, ViewError>;

/// PostgreSQL maximum identifier length in bytes.
const PG_MAX_IDENTIFIER_LENGTH: usize = 63;

/// Validates that an identifier doesn't exceed PostgreSQL's 63-byte limit.
fn validate_identifier(ident: &str) -> ViewResult<()> {
    let len = ident.len();
    if len > PG_MAX_IDENTIFIER_LENGTH {
        return Err(ViewError::IdentifierTooLong(ident.to_string(), len));
    }
    Ok(())
}

/// Quote a SQL identifier to prevent injection.
///
/// Wraps the identifier in double quotes and escapes embedded quotes
/// by doubling them (PostgreSQL standard).
///
/// # Security boundary
///
/// This function handles escaping but does NOT validate that the input
/// is a legitimate identifier. Callers should ensure identifiers come
/// from trusted sources (e.g., pg_catalog queries, information_schema).
/// All identifier values in this module originate from database catalog
/// queries, which are trusted sources.
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
/// * `pk_columns` - Primary key column(s) for the table (must not be empty)
/// * `columns` - Column names to include (excluding _mlb_* metadata columns)
///
/// # Errors
///
/// Returns `ViewError::NoPrimaryKey` if pk_columns is empty.
/// Returns `ViewError::NoColumnsFound` if columns is empty.
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
) -> ViewResult<String> {
    if pk_columns.is_empty() {
        return Err(ViewError::NoPrimaryKey(
            source_schema.to_string(),
            table_name.to_string(),
        ));
    }
    if columns.is_empty() {
        return Err(ViewError::NoColumnsFound(
            source_schema.to_string(),
            table_name.to_string(),
        ));
    }

    // Validate identifier lengths
    validate_identifier(session_schema)?;
    validate_identifier(source_schema)?;
    validate_identifier(table_name)?;
    let shadow_name = format!("_shadow_{}", table_name);
    validate_identifier(&shadow_name)?;
    let deleted_name = format!("_deleted_{}", table_name);
    validate_identifier(&deleted_name)?;
    for col in columns {
        validate_identifier(col)?;
    }
    for pk in pk_columns {
        validate_identifier(pk)?;
    }

    let quoted_session_schema = quote_ident(session_schema);
    let quoted_source_schema = quote_ident(source_schema);
    let quoted_table = quote_ident(table_name);
    let shadow_table = quote_ident(&format!("_shadow_{}", table_name));
    let deleted_table = quote_ident(&format!("_deleted_{}", table_name));

    // Quote all column names
    let quoted_columns: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    let column_list = quoted_columns.join(", ");

    // Build the NOT IN conditions for composite primary keys
    let pk_not_in_shadow =
        generate_pk_not_in_clause(pk_columns, &quoted_session_schema, &shadow_table);
    let pk_not_in_deleted =
        generate_pk_not_in_clause(pk_columns, &quoted_session_schema, &deleted_table);

    Ok(format!(
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
    ))
}

/// Generates the NOT IN clause for primary key filtering.
/// Handles both single and composite primary keys.
/// Adds IS NOT NULL filters to handle NULL values correctly in NOT IN subqueries.
///
/// # Panics
///
/// Panics if pk_columns is empty. Public functions must validate pk_columns
/// before calling this helper - empty pk_columns is a programming error that
/// must be caught at the API boundary, not silently handled here.
fn generate_pk_not_in_clause(pk_columns: &[String], schema: &str, table: &str) -> String {
    assert!(
        !pk_columns.is_empty(),
        "BUG: pk_columns must be validated by caller before calling generate_pk_not_in_clause"
    );

    if pk_columns.len() == 1 {
        // Simple case: single column PK
        let pk = quote_ident(&pk_columns[0]);
        format!("{pk} NOT IN (SELECT {pk} FROM {schema}.{table} WHERE {pk} IS NOT NULL)")
    } else {
        // Composite PK: use row comparison
        let quoted_pks: Vec<String> = pk_columns.iter().map(|c| quote_ident(c)).collect();
        let pk_tuple = quoted_pks.join(", ");
        let not_null_conditions: Vec<String> = quoted_pks
            .iter()
            .map(|pk| format!("{pk} IS NOT NULL"))
            .collect();
        let not_null_clause = not_null_conditions.join(" AND ");
        format!(
            "({pk_tuple}) NOT IN (SELECT {pk_tuple} FROM {schema}.{table} WHERE {not_null_clause})"
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
///
/// # Errors
///
/// Returns `ViewError::NoPrimaryKey` if the table has no primary key defined.
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

    let pk_columns: Vec<String> = rows.iter().map(|r| r.get("column_name")).collect();

    if pk_columns.is_empty() {
        return Err(ViewError::NoPrimaryKey(
            schema.to_string(),
            table_name.to_string(),
        ));
    }

    debug!(
        schema = schema,
        table = table_name,
        pk_columns = ?pk_columns,
        "Fetched primary key columns"
    );

    Ok(pk_columns)
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
///
/// # Errors
///
/// Returns `ViewError::NoPrimaryKey` if pk_columns is empty.
pub async fn create_union_view(
    pool: &Pool,
    session_schema: &str,
    source_schema: &str,
    table_name: &str,
    pk_columns: &[String],
) -> ViewResult<()> {
    if pk_columns.is_empty() {
        return Err(ViewError::NoPrimaryKey(
            source_schema.to_string(),
            table_name.to_string(),
        ));
    }

    // Fetch columns from the source schema (production table)
    let columns = fetch_table_columns(pool, source_schema, table_name).await?;

    let sql = generate_union_view_sql(
        session_schema,
        source_schema,
        table_name,
        pk_columns,
        &columns,
    )?;

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
pub fn generate_view_sql(schema: &str, base_schema: &str, table: &TableInfo) -> String {
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
pub fn generate_shadow_table_sql(schema: &str, base_schema: &str, table_name: &str) -> String {
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
pub fn generate_deleted_table_sql(schema: &str, table: &TableInfo) -> String {
    let pk_columns: Vec<String> = table
        .primary_key
        .iter()
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
        format!(
            "DROP VIEW IF EXISTS {}._view_{} CASCADE",
            schema, table_name
        ),
        format!(
            "DROP TABLE IF EXISTS {}._shadow_{} CASCADE",
            schema, table_name
        ),
        format!(
            "DROP TABLE IF EXISTS {}._deleted_{} CASCADE",
            schema, table_name
        ),
    ]
}

/// Generates the full setup SQL for a table in a sandbox.
pub fn generate_table_setup_sql(schema: &str, base_schema: &str, table: &TableInfo) -> Vec<String> {
    vec![
        generate_shadow_table_sql(schema, base_schema, &table.name),
        generate_deleted_table_sql(schema, table),
        generate_view_sql(schema, base_schema, table),
    ]
}

/// Helper to generate PK equality conditions between two table aliases.
fn generate_pk_conditions(pk_columns: &[String], left_alias: &str, right_alias: &str) -> String {
    pk_columns
        .iter()
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

        assert!(sql.contains("CREATE OR REPLACE VIEW sandbox_123._view_users"));
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
            columns: vec![
                "order_id".to_string(),
                "item_id".to_string(),
                "quantity".to_string(),
            ],
            primary_key: vec!["order_id".to_string(), "item_id".to_string()],
        };

        let view_sql = generate_view_sql("sandbox_123", "public", &table);
        assert!(
            view_sql.contains("shadow.order_id = base.order_id AND shadow.item_id = base.item_id")
        );

        let deleted_sql = generate_deleted_table_sql("sandbox_123", &table);
        assert!(deleted_sql.contains("PRIMARY KEY (order_id, item_id)"));
    }

    #[test]
    fn test_cleanup_sql() {
        let stmts = generate_cleanup_sql("sandbox_123", "users");

        assert_eq!(stmts.len(), 3);
        assert!(stmts[0].contains("DROP VIEW"));
        assert!(stmts[0].contains("_view_users"));
        assert!(stmts[1].contains("DROP TABLE IF EXISTS sandbox_123._shadow_users"));
        assert!(stmts[2].contains("_deleted_users"));
    }

    // Tests for the new union view generator functions

    #[test]
    fn test_generate_union_view_sql_single_pk() {
        let columns = vec!["id".to_string(), "name".to_string(), "email".to_string()];
        let pk_columns = vec!["id".to_string()];

        let sql = generate_union_view_sql("session_abc", "public", "users", &pk_columns, &columns)
            .unwrap();

        // Check view creation
        assert!(sql.contains(r#"CREATE OR REPLACE VIEW "session_abc"."users""#));

        // Check shadow table select
        assert!(sql.contains(r#"FROM "session_abc"."_shadow_users""#));

        // Check production table select
        assert!(sql.contains(r#"FROM "public"."users""#));

        // Check column list is quoted
        assert!(sql.contains(r#""id", "name", "email""#));

        // Check NOT IN clauses for shadow and deleted tables (with IS NOT NULL)
        assert!(sql.contains(
            r#""id" NOT IN (SELECT "id" FROM "session_abc"."_shadow_users" WHERE "id" IS NOT NULL)"#
        ));
        assert!(sql.contains(r#""id" NOT IN (SELECT "id" FROM "session_abc"."_deleted_users" WHERE "id" IS NOT NULL)"#));
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
        )
        .unwrap();

        // Check composite PK handling with row comparison (with IS NOT NULL)
        assert!(sql.contains(
            r#"("order_id", "product_id") NOT IN (SELECT "order_id", "product_id" FROM "session_xyz"."_shadow_order_items" WHERE "order_id" IS NOT NULL AND "product_id" IS NOT NULL)"#
        ));
        assert!(sql.contains(
            r#"("order_id", "product_id") NOT IN (SELECT "order_id", "product_id" FROM "session_xyz"."_deleted_order_items" WHERE "order_id" IS NOT NULL AND "product_id" IS NOT NULL)"#
        ));
    }

    #[test]
    fn test_generate_union_view_sql_special_characters() {
        let columns = vec![
            "id".to_string(),
            "user name".to_string(),   // space in column name
            "data\"field".to_string(), // quote in column name
        ];
        let pk_columns = vec!["id".to_string()];

        let sql = generate_union_view_sql(
            "session-with-dash",
            "my schema",
            "table\"name",
            &pk_columns,
            &columns,
        )
        .unwrap();

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
            r#""id" NOT IN (SELECT "id" FROM "schema"."table" WHERE "id" IS NOT NULL)"#
        );
    }

    #[test]
    fn test_generate_pk_not_in_clause_composite() {
        let pk_columns = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let clause = generate_pk_not_in_clause(&pk_columns, r#""s""#, r#""t""#);

        assert_eq!(
            clause,
            r#"("a", "b", "c") NOT IN (SELECT "a", "b", "c" FROM "s"."t" WHERE "a" IS NOT NULL AND "b" IS NOT NULL AND "c" IS NOT NULL)"#
        );
    }

    #[test]
    fn test_empty_pk_returns_error() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let pk_columns: Vec<String> = vec![];

        let result = generate_union_view_sql("session", "public", "users", &pk_columns, &columns);

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ViewError::NoPrimaryKey(_, _)));
    }

    #[test]
    fn test_empty_columns_returns_error() {
        let columns: Vec<String> = vec![];
        let pk_columns = vec!["id".to_string()];

        let result = generate_union_view_sql("session", "public", "users", &pk_columns, &columns);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ViewError::NoColumnsFound(_, _)
        ));
    }

    #[test]
    fn test_identifier_too_long_returns_error() {
        let long_name = "a".repeat(64); // 64 bytes exceeds 63-byte limit
        let columns = vec!["id".to_string()];
        let pk_columns = vec!["id".to_string()];

        let result =
            generate_union_view_sql("session", "public", &long_name, &pk_columns, &columns);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ViewError::IdentifierTooLong(_, 64)
        ));
    }

    #[test]
    fn test_long_schema_returns_error() {
        let long_schema = "s".repeat(64);
        let columns = vec!["id".to_string()];
        let pk_columns = vec!["id".to_string()];

        let result =
            generate_union_view_sql(&long_schema, "public", "users", &pk_columns, &columns);

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ViewError::IdentifierTooLong(_, 64)
        ));
    }

    #[test]
    fn test_derived_identifier_too_long_returns_error() {
        // Table name that's valid alone but _shadow_ prefix (8 chars) pushes it over
        let table_name = "t".repeat(56); // 56 + 8 = 64 bytes
        let columns = vec!["id".to_string()];
        let pk_columns = vec!["id".to_string()];

        let result =
            generate_union_view_sql("session", "public", &table_name, &pk_columns, &columns);

        assert!(result.is_err());
        match result.unwrap_err() {
            ViewError::IdentifierTooLong(ident, _) => {
                assert!(ident.starts_with("_shadow_") || ident.starts_with("_deleted_"));
            }
            e => panic!("Expected IdentifierTooLong, got {:?}", e),
        }
    }

    #[test]
    fn test_max_valid_identifier_succeeds() {
        // Table name allowing for _deleted_ prefix (9 chars) within 63-byte limit
        let table_name = "t".repeat(54); // 54 + 9 = 63 bytes, exactly at limit
        let columns = vec!["id".to_string()];
        let pk_columns = vec!["id".to_string()];

        let result = generate_union_view_sql("s", "public", &table_name, &pk_columns, &columns);

        assert!(result.is_ok());
    }
}
