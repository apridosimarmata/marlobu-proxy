//! Table name rewriting logic for schema isolation.
//!
//! Transforms table references based on query context:
//! - Read operations (SELECT, FROM, JOIN) → `{schema}.{table}_view`
//! - Write operations (INSERT, UPDATE, DELETE targets) → `{schema}.{table}`

use sqlparser::ast::{ObjectName, Ident};

/// Context for table rewriting - determines view vs shadow table routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteContext {
    /// Reading data - route to view (sees base + shadow)
    Read,
    /// Writing data - route to shadow table directly
    Write,
}

/// Rewrites a table name for the given schema and context.
///
/// # Examples
/// - `users` with Read context → `schema_123.users_view`
/// - `users` with Write context → `schema_123.users`
/// - `public.orders` with Read context → `schema_123.orders_view`
pub fn rewrite_table_name(
    table: &ObjectName,
    schema: &str,
    context: RewriteContext,
) -> ObjectName {
    let table_name = extract_table_name(table);

    let rewritten_name = match context {
        RewriteContext::Read => format!("{}_view", table_name),
        RewriteContext::Write => table_name.to_string(),
    };

    ObjectName(vec![
        Ident::new(schema),
        Ident::new(rewritten_name),
    ])
}

/// Extracts the base table name, stripping any existing schema prefix.
fn extract_table_name(table: &ObjectName) -> &str {
    // ObjectName is a Vec<Ident>, last element is the table name
    table.0.last()
        .map(|ident| ident.value.as_str())
        .unwrap_or("unknown")
}

/// Checks if a table name should be excluded from rewriting.
/// System tables and pg_* tables are passed through unchanged.
pub fn should_skip_rewrite(table: &ObjectName) -> bool {
    let name = extract_table_name(table).to_lowercase();

    // Skip PostgreSQL system catalogs
    if name.starts_with("pg_") || name.starts_with("information_schema") {
        return true;
    }

    // Skip common system tables
    matches!(name.as_str(),
        "dual" | "sqlite_master" | "sqlite_sequence"
    )
}

/// Creates a fully qualified table reference.
pub fn qualify_table(schema: &str, table: &str) -> ObjectName {
    ObjectName(vec![
        Ident::new(schema),
        Ident::new(table),
    ])
}

/// Creates a view name for a table.
pub fn view_name_for_table(table: &str) -> String {
    format!("{}_view", table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_simple_table_read() {
        let table = ObjectName(vec![Ident::new("users")]);
        let result = rewrite_table_name(&table, "sandbox_123", RewriteContext::Read);

        assert_eq!(result.0.len(), 2);
        assert_eq!(result.0[0].value, "sandbox_123");
        assert_eq!(result.0[1].value, "users_view");
    }

    #[test]
    fn test_rewrite_simple_table_write() {
        let table = ObjectName(vec![Ident::new("users")]);
        let result = rewrite_table_name(&table, "sandbox_123", RewriteContext::Write);

        assert_eq!(result.0.len(), 2);
        assert_eq!(result.0[0].value, "sandbox_123");
        assert_eq!(result.0[1].value, "users");
    }

    #[test]
    fn test_rewrite_qualified_table() {
        let table = ObjectName(vec![Ident::new("public"), Ident::new("orders")]);
        let result = rewrite_table_name(&table, "sandbox_456", RewriteContext::Read);

        assert_eq!(result.0[0].value, "sandbox_456");
        assert_eq!(result.0[1].value, "orders_view");
    }

    #[test]
    fn test_skip_pg_catalog() {
        let table = ObjectName(vec![Ident::new("pg_class")]);
        assert!(should_skip_rewrite(&table));
    }

    #[test]
    fn test_skip_information_schema() {
        let table = ObjectName(vec![Ident::new("information_schema_tables")]);
        assert!(should_skip_rewrite(&table));
    }

    #[test]
    fn test_no_skip_regular_table() {
        let table = ObjectName(vec![Ident::new("users")]);
        assert!(!should_skip_rewrite(&table));
    }
}
