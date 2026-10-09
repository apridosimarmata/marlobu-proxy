//! View SQL generation for sandbox isolation.
//!
//! Generates CREATE VIEW statements that union base table data with shadow table
//! modifications, implementing copy-on-write semantics.

use crate::rewriter::tables::view_name_for_table;

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
}
