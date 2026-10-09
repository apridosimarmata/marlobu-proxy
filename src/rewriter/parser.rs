//! SQL query rewriter for sandbox isolation.
//!
//! Parses incoming SQL, classifies query type, and rewrites table references
//! to route reads through views and writes to shadow tables.

use sqlparser::ast::{
    CopySource, DoUpdate, Expr, ObjectName, OnConflict, OnConflictAction, OnInsert, Query,
    Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use thiserror::Error;

use crate::rewriter::tables::{rewrite_table_name, should_skip_rewrite, RewriteContext};

/// Errors that can occur during query rewriting.
#[derive(Error, Debug)]
pub enum RewriterError {
    #[error("Failed to parse SQL: {0}")]
    ParseError(String),

    #[error("Blocked statement type: {0}")]
    BlockedStatement(String),

    #[error("Empty query")]
    EmptyQuery,

    #[error("Multiple statements not supported")]
    MultipleStatements,
}

/// Classification of SQL query types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Select,
    Insert,
    Update,
    Delete,
    Ddl,
    Other,
}

impl QueryType {
    /// Returns true if this query type modifies data.
    pub fn is_write(&self) -> bool {
        matches!(self, QueryType::Insert | QueryType::Update | QueryType::Delete)
    }
}

/// Table reference with its context (read or write)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// Table name (without schema)
    pub name: String,
    /// Whether this is a write target
    pub is_write_target: bool,
}

/// Result of query analysis
#[derive(Debug)]
pub struct QueryAnalysis {
    /// Rewritten SQL
    pub sql: String,
    /// Query type
    pub query_type: QueryType,
    /// Tables referenced (for infrastructure setup)
    pub tables: Vec<TableRef>,
}

/// SQL query rewriter for sandbox isolation.
pub struct Rewriter {
    /// Target schema for rewritten queries
    schema: String,
    /// Dialect for parsing
    dialect: PostgreSqlDialect,
}

impl Rewriter {
    /// Creates a new rewriter targeting the given schema.
    pub fn new(schema: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            dialect: PostgreSqlDialect {},
        }
    }

    /// Parses and rewrites a SQL query for sandbox isolation.
    ///
    /// Returns the rewritten SQL and its query type.
    pub fn rewrite(&self, sql: &str) -> Result<(String, QueryType), RewriterError> {
        let analysis = self.analyze(sql)?;
        Ok((analysis.sql, analysis.query_type))
    }

    /// Parses, rewrites, and analyzes a SQL query.
    ///
    /// Returns full analysis including tables referenced (for infrastructure setup).
    pub fn analyze(&self, sql: &str) -> Result<QueryAnalysis, RewriterError> {
        let mut statements = Parser::parse_sql(&self.dialect, sql)
            .map_err(|e| RewriterError::ParseError(e.to_string()))?;

        if statements.is_empty() {
            return Err(RewriterError::EmptyQuery);
        }

        if statements.len() > 1 {
            return Err(RewriterError::MultipleStatements);
        }

        let mut stmt = statements.remove(0);
        let query_type = self.classify(&stmt);

        // Block dangerous statements
        self.check_blocked(&stmt)?;

        // Extract tables before rewriting (to get original names)
        let tables = self.extract_tables(&stmt);

        // Rewrite table references
        self.rewrite_statement(&mut stmt)?;

        Ok(QueryAnalysis {
            sql: stmt.to_string(),
            query_type,
            tables,
        })
    }

    /// Extract table references from a statement
    fn extract_tables(&self, stmt: &Statement) -> Vec<TableRef> {
        let mut tables = Vec::new();

        match stmt {
            Statement::Query(query) => {
                self.extract_tables_from_query(query, &mut tables, false);
            }
            Statement::Insert { table_name, source, .. } => {
                // Target table is a write target
                if let Some(name) = extract_table_name_str(table_name) {
                    if !is_system_table(&name) {
                        tables.push(TableRef { name, is_write_target: true });
                    }
                }
                // Source query tables are read targets
                if let Some(src) = source {
                    self.extract_tables_from_query(src, &mut tables, false);
                }
            }
            Statement::Update { table, from, selection, .. } => {
                // Target table is a write target
                self.extract_tables_from_table_with_joins(table, &mut tables, true);
                // FROM clause is read
                if let Some(from_table) = from {
                    self.extract_tables_from_table_with_joins(from_table, &mut tables, false);
                }
                // Subqueries in WHERE are read
                if let Some(where_expr) = selection {
                    self.extract_tables_from_expr(where_expr, &mut tables);
                }
            }
            Statement::Delete { from, using, selection, .. } => {
                // Target table(s) are write targets
                for table_with_joins in from {
                    self.extract_tables_from_table_with_joins(table_with_joins, &mut tables, true);
                }
                // USING clause is read
                if let Some(using_clause) = using {
                    for table_with_joins in using_clause {
                        self.extract_tables_from_table_with_joins(table_with_joins, &mut tables, false);
                    }
                }
                // Subqueries in WHERE are read
                if let Some(where_expr) = selection {
                    self.extract_tables_from_expr(where_expr, &mut tables);
                }
            }
            _ => {}
        }

        // Deduplicate tables (keep first occurrence)
        let mut seen = std::collections::HashSet::new();
        tables.retain(|t| seen.insert(t.name.clone()));

        tables
    }

    fn extract_tables_from_query(&self, query: &Query, tables: &mut Vec<TableRef>, is_write: bool) {
        // Handle CTEs
        if let Some(ref with) = query.with {
            for cte in &with.cte_tables {
                self.extract_tables_from_query(&cte.query, tables, false);
            }
        }
        self.extract_tables_from_set_expr(&query.body, tables, is_write);
    }

    fn extract_tables_from_set_expr(&self, set_expr: &SetExpr, tables: &mut Vec<TableRef>, is_write: bool) {
        match set_expr {
            SetExpr::Select(select) => {
                self.extract_tables_from_select(select, tables, is_write);
            }
            SetExpr::Query(query) => {
                self.extract_tables_from_query(query, tables, is_write);
            }
            SetExpr::SetOperation { left, right, .. } => {
                self.extract_tables_from_set_expr(left, tables, is_write);
                self.extract_tables_from_set_expr(right, tables, is_write);
            }
            _ => {}
        }
    }

    fn extract_tables_from_select(&self, select: &Select, tables: &mut Vec<TableRef>, is_write: bool) {
        for table_with_joins in &select.from {
            self.extract_tables_from_table_with_joins(table_with_joins, tables, is_write);
        }
        if let Some(ref selection) = select.selection {
            self.extract_tables_from_expr(selection, tables);
        }
    }

    fn extract_tables_from_table_with_joins(&self, table: &TableWithJoins, tables: &mut Vec<TableRef>, is_write: bool) {
        self.extract_tables_from_table_factor(&table.relation, tables, is_write);
        for join in &table.joins {
            self.extract_tables_from_table_factor(&join.relation, tables, false);
        }
    }

    fn extract_tables_from_table_factor(&self, factor: &TableFactor, tables: &mut Vec<TableRef>, is_write: bool) {
        match factor {
            TableFactor::Table { name, .. } => {
                if let Some(table_name) = extract_table_name_str(name) {
                    if !is_system_table(&table_name) {
                        tables.push(TableRef { name: table_name, is_write_target: is_write });
                    }
                }
            }
            TableFactor::Derived { subquery, .. } => {
                self.extract_tables_from_query(subquery, tables, false);
            }
            TableFactor::NestedJoin { table_with_joins, .. } => {
                self.extract_tables_from_table_with_joins(table_with_joins, tables, is_write);
            }
            _ => {}
        }
    }

    fn extract_tables_from_expr(&self, expr: &Expr, tables: &mut Vec<TableRef>) {
        match expr {
            Expr::Subquery(query) => {
                self.extract_tables_from_query(query, tables, false);
            }
            Expr::InSubquery { subquery, .. } => {
                self.extract_tables_from_query(subquery, tables, false);
            }
            Expr::Exists { subquery, .. } => {
                self.extract_tables_from_query(subquery, tables, false);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.extract_tables_from_expr(left, tables);
                self.extract_tables_from_expr(right, tables);
            }
            Expr::UnaryOp { expr: inner, .. } => {
                self.extract_tables_from_expr(inner, tables);
            }
            Expr::Nested(inner) => {
                self.extract_tables_from_expr(inner, tables);
            }
            _ => {}
        }
    }

    /// Classifies a statement by query type.
    pub fn classify(&self, stmt: &Statement) -> QueryType {
        match stmt {
            Statement::Query(_) => QueryType::Select,
            Statement::Insert { .. } => QueryType::Insert,
            Statement::Update { .. } => QueryType::Update,
            Statement::Delete { .. } => QueryType::Delete,
            Statement::CreateTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::CreateView { .. }
            | Statement::AlterTable { .. }
            | Statement::Drop { .. }
            | Statement::Truncate { .. } => QueryType::Ddl,
            _ => QueryType::Other,
        }
    }

    /// Checks if a statement should be blocked.
    fn check_blocked(&self, stmt: &Statement) -> Result<(), RewriterError> {
        match stmt {
            // Block DDL
            Statement::CreateTable { .. } => {
                Err(RewriterError::BlockedStatement("CREATE TABLE".to_string()))
            }
            Statement::CreateIndex { .. } => {
                Err(RewriterError::BlockedStatement("CREATE INDEX".to_string()))
            }
            Statement::CreateView { .. } => {
                Err(RewriterError::BlockedStatement("CREATE VIEW".to_string()))
            }
            Statement::AlterTable { .. } => {
                Err(RewriterError::BlockedStatement("ALTER TABLE".to_string()))
            }
            Statement::Drop { .. } => {
                Err(RewriterError::BlockedStatement("DROP".to_string()))
            }
            Statement::Truncate { .. } => {
                Err(RewriterError::BlockedStatement("TRUNCATE".to_string()))
            }

            // Block dangerous operations
            Statement::SetRole { .. } => {
                Err(RewriterError::BlockedStatement("SET ROLE".to_string()))
            }

            // Allow everything else (will be rewritten)
            _ => Ok(()),
        }
    }

    /// Rewrites table references in a statement.
    fn rewrite_statement(&self, stmt: &mut Statement) -> Result<(), RewriterError> {
        match stmt {
            Statement::Query(query) => {
                self.rewrite_query(query, RewriteContext::Read);
            }
            Statement::Insert {
                table_name,
                source,
                on,
                returning,
                ..
            } => {
                // Target table goes to shadow
                if !should_skip_rewrite(table_name) {
                    *table_name = rewrite_table_name(
                        table_name,
                        &self.schema,
                        RewriteContext::Write,
                    );
                }
                // Source query (INSERT ... SELECT) goes to views
                if let Some(ref mut src) = source {
                    self.rewrite_query(src, RewriteContext::Read);
                }
                // ON CONFLICT clause may have subqueries in DO UPDATE
                if let Some(ref mut on_insert) = on {
                    self.rewrite_on_insert(on_insert);
                }
                // RETURNING clause may have subqueries
                if let Some(ref mut ret) = returning {
                    self.rewrite_select_items(ret);
                }
            }
            Statement::Update {
                table,
                from,
                selection,
                returning,
                ..
            } => {
                // Target table goes to shadow
                self.rewrite_table_with_joins(table, RewriteContext::Write);

                // FROM clause goes to views (singular TableWithJoins in sqlparser 0.41)
                if let Some(ref mut from_table) = from {
                    self.rewrite_table_with_joins(from_table, RewriteContext::Read);
                }

                // Subqueries in WHERE go to views
                if let Some(ref mut where_expr) = selection {
                    self.rewrite_expr(where_expr, RewriteContext::Read);
                }

                // RETURNING clause may have subqueries
                if let Some(ref mut ret) = returning {
                    self.rewrite_select_items(ret);
                }
            }
            Statement::Delete {
                from,
                using,
                selection,
                returning,
                ..
            } => {
                // Target table(s) go to shadow
                for table_with_joins in from.iter_mut() {
                    self.rewrite_table_with_joins(table_with_joins, RewriteContext::Write);
                }

                // USING clause goes to views
                if let Some(ref mut using_clause) = using {
                    for table_with_joins in using_clause.iter_mut() {
                        self.rewrite_table_with_joins(table_with_joins, RewriteContext::Read);
                    }
                }

                // Subqueries in WHERE go to views
                if let Some(ref mut where_expr) = selection {
                    self.rewrite_expr(where_expr, RewriteContext::Read);
                }

                // RETURNING clause may have subqueries
                if let Some(ref mut ret) = returning {
                    self.rewrite_select_items(ret);
                }
            }
            Statement::Copy {
                source,
                to,
                ..
            } => {
                // COPY TO (export): reads from view
                // COPY FROM (import): writes directly to shadow (bypasses triggers)
                let context = if *to {
                    RewriteContext::Read
                } else {
                    RewriteContext::DirectWrite
                };

                match source {
                    CopySource::Table { table_name, .. } => {
                        if !should_skip_rewrite(table_name) {
                            *table_name = rewrite_table_name(table_name, &self.schema, context);
                        }
                    }
                    CopySource::Query(query) => {
                        // COPY (SELECT ...) TO - rewrite the query
                        self.rewrite_query(query, RewriteContext::Read);
                    }
                }
            }
            _ => {
                // For other statement types, we don't rewrite
                // (they should have been blocked or are safe as-is)
            }
        }
        Ok(())
    }

    /// Rewrites all relations in a query to use views (for read context).
    fn rewrite_query(&self, query: &mut Query, context: RewriteContext) {
        // Handle CTEs
        if let Some(ref mut with) = query.with {
            for cte in with.cte_tables.iter_mut() {
                self.rewrite_query(&mut cte.query, context);
            }
        }

        // Handle main query body
        self.rewrite_set_expr(&mut query.body, context);
    }

    /// Rewrites a SET expression (handles UNION, INTERSECT, etc.).
    fn rewrite_set_expr(&self, set_expr: &mut SetExpr, context: RewriteContext) {
        match set_expr {
            SetExpr::Select(select) => {
                self.rewrite_select(select, context);
            }
            SetExpr::Query(query) => {
                self.rewrite_query(query, context);
            }
            SetExpr::SetOperation { left, right, .. } => {
                self.rewrite_set_expr(left, context);
                self.rewrite_set_expr(right, context);
            }
            SetExpr::Values(_) => {
                // VALUES clause has no table references
            }
            SetExpr::Insert(stmt) => {
                // Nested INSERT - stmt is Statement directly (not boxed)
                if let Statement::Insert { ref mut table_name, .. } = stmt {
                    if !should_skip_rewrite(table_name) {
                        *table_name = rewrite_table_name(
                            table_name,
                            &self.schema,
                            RewriteContext::Write,
                        );
                    }
                }
            }
            SetExpr::Update(_) => {
                // Nested UPDATE - handle gracefully
            }
            SetExpr::Table(table) => {
                if let Some(ref name) = table.table_name {
                    // table_name is Option<String> in SetExpr::Table
                    // We can't easily rewrite this without more context
                    // This is a rare edge case (TABLE tablename syntax)
                    let _ = name; // Acknowledge but skip
                }
            }
        }
    }

    /// Rewrites a SELECT statement.
    fn rewrite_select(&self, select: &mut Select, context: RewriteContext) {
        // Rewrite FROM clause
        for table_with_joins in select.from.iter_mut() {
            self.rewrite_table_with_joins(table_with_joins, context);
        }

        // Rewrite WHERE subqueries
        if let Some(ref mut selection) = select.selection {
            self.rewrite_expr(selection, context);
        }

        // Rewrite HAVING subqueries
        if let Some(ref mut having) = select.having {
            self.rewrite_expr(having, context);
        }
    }

    /// Rewrites a table with its joins.
    fn rewrite_table_with_joins(&self, table: &mut TableWithJoins, context: RewriteContext) {
        self.rewrite_table_factor(&mut table.relation, context);

        for join in table.joins.iter_mut() {
            self.rewrite_table_factor(&mut join.relation, context);
        }
    }

    /// Rewrites a table factor (table reference, subquery, etc.).
    fn rewrite_table_factor(&self, factor: &mut TableFactor, context: RewriteContext) {
        match factor {
            TableFactor::Table { name, .. } => {
                if !should_skip_rewrite(name) {
                    *name = rewrite_table_name(name, &self.schema, context);
                }
            }
            TableFactor::Derived { subquery, .. } => {
                self.rewrite_query(subquery, context);
            }
            TableFactor::TableFunction { .. } => {
                // Table functions don't need rewriting
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                self.rewrite_table_with_joins(table_with_joins, context);
            }
            _ => {
                // Other table factors (UNNEST, etc.) - no table name to rewrite
            }
        }
    }

    /// Rewrites expressions, looking for subqueries.
    fn rewrite_expr(&self, expr: &mut Expr, context: RewriteContext) {
        match expr {
            Expr::Subquery(query) => {
                self.rewrite_query(query, context);
            }
            Expr::InSubquery { subquery, .. } => {
                self.rewrite_query(subquery, context);
            }
            Expr::Exists { subquery, .. } => {
                self.rewrite_query(subquery, context);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.rewrite_expr(left, context);
                self.rewrite_expr(right, context);
            }
            Expr::UnaryOp { expr: inner, .. } => {
                self.rewrite_expr(inner, context);
            }
            Expr::Nested(inner) => {
                self.rewrite_expr(inner, context);
            }
            Expr::Between { expr: inner, low, high, .. } => {
                self.rewrite_expr(inner, context);
                self.rewrite_expr(low, context);
                self.rewrite_expr(high, context);
            }
            Expr::Case { operand, conditions, results, else_result, .. } => {
                if let Some(op) = operand {
                    self.rewrite_expr(op, context);
                }
                for cond in conditions {
                    self.rewrite_expr(cond, context);
                }
                for result in results {
                    self.rewrite_expr(result, context);
                }
                if let Some(else_expr) = else_result {
                    self.rewrite_expr(else_expr, context);
                }
            }
            Expr::InList { expr: inner, list, .. } => {
                self.rewrite_expr(inner, context);
                for item in list {
                    self.rewrite_expr(item, context);
                }
            }
            _ => {
                // Other expressions don't contain table references we need to rewrite
            }
        }
    }

    /// Rewrites ON INSERT clause (ON CONFLICT for Postgres)
    fn rewrite_on_insert(&self, on_insert: &mut OnInsert) {
        match on_insert {
            OnInsert::OnConflict(on_conflict) => {
                self.rewrite_on_conflict(on_conflict);
            }
            OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL ON DUPLICATE KEY UPDATE - rewrite expressions in assignments
                for assignment in assignments {
                    self.rewrite_expr(&mut assignment.value, RewriteContext::Read);
                }
            }
            _ => {
                // Future OnInsert variants - no rewriting needed
            }
        }
    }

    /// Rewrites ON CONFLICT clause
    fn rewrite_on_conflict(&self, on_conflict: &mut OnConflict) {
        match &mut on_conflict.action {
            OnConflictAction::DoNothing => {
                // Nothing to rewrite
            }
            OnConflictAction::DoUpdate(do_update) => {
                self.rewrite_do_update(do_update);
            }
        }
    }

    /// Rewrites DO UPDATE clause in ON CONFLICT
    fn rewrite_do_update(&self, do_update: &mut DoUpdate) {
        // Rewrite expressions in assignments (e.g., SET col = (SELECT ...))
        for assignment in &mut do_update.assignments {
            self.rewrite_expr(&mut assignment.value, RewriteContext::Read);
        }
        // Rewrite WHERE clause if present
        if let Some(ref mut selection) = do_update.selection {
            self.rewrite_expr(selection, RewriteContext::Read);
        }
    }

    /// Rewrites RETURNING clause (list of select items)
    fn rewrite_select_items(&self, items: &mut Vec<SelectItem>) {
        for item in items {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    self.rewrite_expr(expr, RewriteContext::Read);
                }
                SelectItem::ExprWithAlias { expr, .. } => {
                    self.rewrite_expr(expr, RewriteContext::Read);
                }
                SelectItem::QualifiedWildcard(_, _) | SelectItem::Wildcard(_) => {
                    // Wildcards don't need rewriting
                }
            }
        }
    }
}

/// Extract the base table name from an ObjectName (last component)
fn extract_table_name_str(name: &sqlparser::ast::ObjectName) -> Option<String> {
    name.0.last().map(|ident| ident.value.clone())
}

/// Check if a table name is a system table that should not be rewritten
fn is_system_table(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("pg_")
        || lower.starts_with("information_schema")
        || matches!(lower.as_str(), "dual" | "sqlite_master" | "sqlite_sequence")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewriter() -> Rewriter {
        Rewriter::new("sandbox_123")
    }

    #[test]
    fn test_simple_select() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite("SELECT * FROM users").unwrap();

        assert_eq!(query_type, QueryType::Select);
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_select_with_join() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id"
        ).unwrap();

        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("sandbox_123._view_orders"));
    }

    #[test]
    fn test_select_with_subquery() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders)"
        ).unwrap();

        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("sandbox_123._view_orders"));
    }

    #[test]
    fn test_select_with_cte() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "WITH active AS (SELECT * FROM users WHERE active = true) SELECT * FROM active"
        ).unwrap();

        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_insert_simple() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "INSERT INTO users (name, email) VALUES ('John', 'john@example.com')"
        ).unwrap();

        assert_eq!(query_type, QueryType::Insert);
        // Target should be shadow table
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_insert_select() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "INSERT INTO users_archive SELECT * FROM users WHERE created_at < '2024-01-01'"
        ).unwrap();

        // Target is shadow table
        assert!(sql.contains("sandbox_123._view_users_archive"));
        // Source is view
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_update_simple() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "UPDATE users SET name = 'Jane' WHERE id = 1"
        ).unwrap();

        assert_eq!(query_type, QueryType::Update);
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_update_with_from() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "UPDATE users SET total = orders.sum FROM orders WHERE users.id = orders.user_id"
        ).unwrap();

        // Target is shadow table
        assert!(sql.contains("UPDATE sandbox_123._view_users"));
        // FROM clause is view
        assert!(sql.contains("sandbox_123._view_orders"));
    }

    #[test]
    fn test_delete_simple() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite("DELETE FROM users WHERE id = 1").unwrap();

        assert_eq!(query_type, QueryType::Delete);
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_delete_with_using() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "DELETE FROM users USING orders WHERE users.id = orders.user_id AND orders.total = 0"
        ).unwrap();

        // Target is shadow table
        assert!(sql.contains("FROM sandbox_123._view_users"));
        // USING is view
        assert!(sql.contains("sandbox_123._view_orders"));
    }

    #[test]
    fn test_block_ddl() {
        let r = rewriter();

        assert!(r.rewrite("CREATE TABLE foo (id INT)").is_err());
        assert!(r.rewrite("DROP TABLE users").is_err());
        assert!(r.rewrite("ALTER TABLE users ADD COLUMN foo INT").is_err());
        assert!(r.rewrite("TRUNCATE users").is_err());
    }

    #[test]
    fn test_block_dangerous() {
        let r = rewriter();

        assert!(r.rewrite("SET ROLE admin").is_err());
    }

    #[test]
    fn test_copy_to_uses_view() {
        let r = rewriter();
        let (sql, qt) = r.rewrite("COPY users TO STDOUT").unwrap();

        assert_eq!(qt, QueryType::Other);
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_copy_from_uses_shadow() {
        let r = rewriter();
        // COPY FROM with file path
        let (sql, qt) = r.rewrite("COPY users FROM '/tmp/data.csv'").unwrap();

        assert_eq!(qt, QueryType::Other);
        assert!(sql.contains("_shadow_users"), "Expected shadow table in: {}", sql);
    }

    #[test]
    fn test_copy_query_to_stdout() {
        let r = rewriter();
        let (sql, _) = r.rewrite("COPY (SELECT * FROM users) TO STDOUT").unwrap();

        // The subquery should use view
        assert!(sql.contains("sandbox_123._view_users"));
    }

    #[test]
    fn test_skip_pg_catalog() {
        let r = rewriter();
        let (sql, _) = r.rewrite("SELECT * FROM pg_class").unwrap();

        // Should not rewrite system tables
        assert!(sql.contains("pg_class"));
        assert!(!sql.contains("sandbox_123"));
    }

    #[test]
    fn test_query_type_classification() {
        let r = rewriter();

        let (_, qt) = r.rewrite("SELECT 1").unwrap();
        assert_eq!(qt, QueryType::Select);

        let (_, qt) = r.rewrite("INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(qt, QueryType::Insert);

        let (_, qt) = r.rewrite("UPDATE t SET x = 1").unwrap();
        assert_eq!(qt, QueryType::Update);

        let (_, qt) = r.rewrite("DELETE FROM t").unwrap();
        assert_eq!(qt, QueryType::Delete);
    }

    #[test]
    fn test_is_write() {
        assert!(!QueryType::Select.is_write());
        assert!(QueryType::Insert.is_write());
        assert!(QueryType::Update.is_write());
        assert!(QueryType::Delete.is_write());
        assert!(!QueryType::Ddl.is_write());
        assert!(!QueryType::Other.is_write());
    }

    #[test]
    fn test_complex_query() {
        let r = rewriter();
        let (sql, _) = r.rewrite(r#"
            WITH recent_orders AS (
                SELECT user_id, SUM(total) as total
                FROM orders
                WHERE created_at > '2024-01-01'
                GROUP BY user_id
            )
            SELECT u.name, u.email, ro.total
            FROM users u
            LEFT JOIN recent_orders ro ON u.id = ro.user_id
            WHERE u.active = true
            AND EXISTS (SELECT 1 FROM payments p WHERE p.user_id = u.id)
            ORDER BY ro.total DESC
        "#).unwrap();

        assert!(sql.contains("sandbox_123._view_orders"));
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("sandbox_123._view_payments"));
    }

    #[test]
    fn test_insert_on_conflict_do_nothing() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "INSERT INTO users (id, name) VALUES (1, 'John') ON CONFLICT (id) DO NOTHING"
        ).unwrap();

        assert_eq!(query_type, QueryType::Insert);
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("ON CONFLICT"));
        assert!(sql.contains("DO NOTHING"));
    }

    #[test]
    fn test_insert_on_conflict_do_update() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "INSERT INTO users (id, name) VALUES (1, 'John') ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name"
        ).unwrap();

        assert_eq!(query_type, QueryType::Insert);
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("ON CONFLICT"));
        assert!(sql.contains("DO UPDATE"));
    }

    #[test]
    fn test_insert_on_conflict_with_subquery() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "INSERT INTO users (id, name) VALUES (1, 'John') ON CONFLICT (id) DO UPDATE SET name = (SELECT name FROM defaults WHERE id = 1)"
        ).unwrap();

        // Target is shadow
        assert!(sql.contains("sandbox_123._view_users"));
        // Subquery in DO UPDATE should use view
        assert!(sql.contains("sandbox_123._view_defaults"));
    }

    #[test]
    fn test_insert_returning() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "INSERT INTO users (name) VALUES ('John') RETURNING id, name"
        ).unwrap();

        assert_eq!(query_type, QueryType::Insert);
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("RETURNING"));
    }

    #[test]
    fn test_insert_returning_star() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "INSERT INTO users (name) VALUES ('John') RETURNING *"
        ).unwrap();

        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("RETURNING *"));
    }

    #[test]
    fn test_update_returning() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "UPDATE users SET name = 'Jane' WHERE id = 1 RETURNING id, name, updated_at"
        ).unwrap();

        assert_eq!(query_type, QueryType::Update);
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("RETURNING"));
    }

    #[test]
    fn test_delete_returning() {
        let r = rewriter();
        let (sql, query_type) = r.rewrite(
            "DELETE FROM users WHERE id = 1 RETURNING *"
        ).unwrap();

        assert_eq!(query_type, QueryType::Delete);
        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("RETURNING"));
    }

    #[test]
    fn test_returning_with_subquery() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "INSERT INTO users (name) VALUES ('John') RETURNING id, (SELECT COUNT(*) FROM orders WHERE user_id = users.id) as order_count"
        ).unwrap();

        // Target is shadow
        assert!(sql.contains("sandbox_123._view_users"));
        // Subquery in RETURNING should use view
        assert!(sql.contains("sandbox_123._view_orders"));
    }

    #[test]
    fn test_insert_on_conflict_do_update_with_where() {
        let r = rewriter();
        let (sql, _) = r.rewrite(
            "INSERT INTO users (id, name) VALUES (1, 'John') ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name WHERE users.active = true"
        ).unwrap();

        assert!(sql.contains("sandbox_123._view_users"));
        assert!(sql.contains("DO UPDATE"));
        assert!(sql.contains("WHERE"));
    }

    #[test]
    fn test_begin_transaction() {
        let r = rewriter();
        let result = r.rewrite("BEGIN");
        // BEGIN should pass through (classified as Other)
        assert!(result.is_ok());
        let (sql, query_type) = result.unwrap();
        assert_eq!(query_type, QueryType::Other);
        assert!(sql.to_uppercase().contains("BEGIN"));
    }

    #[test]
    fn test_commit_transaction() {
        let r = rewriter();
        let result = r.rewrite("COMMIT");
        assert!(result.is_ok());
        let (sql, query_type) = result.unwrap();
        assert_eq!(query_type, QueryType::Other);
        assert!(sql.to_uppercase().contains("COMMIT"));
    }

    #[test]
    fn test_rollback_transaction() {
        let r = rewriter();
        let result = r.rewrite("ROLLBACK");
        assert!(result.is_ok());
        let (sql, query_type) = result.unwrap();
        assert_eq!(query_type, QueryType::Other);
        assert!(sql.to_uppercase().contains("ROLLBACK"));
    }

    #[test]
    fn test_savepoint() {
        let r = rewriter();
        let result = r.rewrite("SAVEPOINT my_savepoint");
        assert!(result.is_ok());
        let (sql, _) = result.unwrap();
        assert!(sql.to_uppercase().contains("SAVEPOINT"));
    }

    #[test]
    fn test_rollback_to_savepoint() {
        let r = rewriter();
        let result = r.rewrite("ROLLBACK TO SAVEPOINT my_savepoint");
        assert!(result.is_ok());
        let (sql, _) = result.unwrap();
        assert!(sql.to_uppercase().contains("ROLLBACK"));
        assert!(sql.contains("my_savepoint"));
    }

    #[test]
    fn test_release_savepoint() {
        let r = rewriter();
        let result = r.rewrite("RELEASE SAVEPOINT my_savepoint");
        assert!(result.is_ok());
        let (sql, _) = result.unwrap();
        assert!(sql.to_uppercase().contains("RELEASE"));
    }

    #[test]
    fn test_start_transaction() {
        let r = rewriter();
        let result = r.rewrite("START TRANSACTION");
        assert!(result.is_ok());
        let (sql, _) = result.unwrap();
        assert!(sql.to_uppercase().contains("START TRANSACTION"));
    }

    #[test]
    fn test_begin_with_isolation_level() {
        let r = rewriter();
        let result = r.rewrite("BEGIN ISOLATION LEVEL SERIALIZABLE");
        assert!(result.is_ok());
        let (sql, _) = result.unwrap();
        assert!(sql.to_uppercase().contains("BEGIN"));
        assert!(sql.to_uppercase().contains("SERIALIZABLE"));
    }
}

#[cfg(test)]
mod edge_case_tests {
    use super::*;

    fn test_parse(sql: &str) -> bool {
        let r = Rewriter::new("test_schema");
        r.rewrite(sql).is_ok()
    }

    #[test]
    fn test_array_any() {
        assert!(test_parse("SELECT * FROM users WHERE id = ANY(ARRAY[1,2,3])"));
    }

    #[test]
    fn test_array_literal() {
        assert!(test_parse("SELECT ARRAY[1,2,3]"));
    }

    #[test]
    fn test_json_arrow() {
        assert!(test_parse("SELECT data->>'name' FROM users"));
    }

    #[test]
    fn test_json_arrow_single() {
        assert!(test_parse("SELECT data->'nested' FROM users"));
    }

    #[test]
    fn test_type_cast_double_colon() {
        assert!(test_parse("SELECT '2024-01-01'::date"));
    }

    #[test]
    fn test_lateral_join() {
        assert!(test_parse("SELECT * FROM users u, LATERAL (SELECT * FROM orders WHERE user_id = u.id) o"));
    }

    #[test]
    fn test_window_function() {
        assert!(test_parse("SELECT ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) FROM emp"));
    }

    #[test]
    fn test_distinct_on() {
        assert!(test_parse("SELECT DISTINCT ON (dept) * FROM emp ORDER BY dept, salary DESC"));
    }

    #[test]
    fn test_filter_clause() {
        assert!(test_parse("SELECT COUNT(*) FILTER (WHERE active) FROM users"));
    }

    #[test]
    fn test_recursive_cte() {
        assert!(test_parse("WITH RECURSIVE cte AS (SELECT 1 AS n UNION ALL SELECT n+1 FROM cte WHERE n < 10) SELECT * FROM cte"));
    }

    #[test]
    fn test_for_update() {
        assert!(test_parse("SELECT * FROM users WHERE id = 1 FOR UPDATE"));
    }

    #[test]
    fn test_interval() {
        assert!(test_parse("SELECT NOW() + INTERVAL '1 day'"));
    }

    #[test]
    fn test_excluded_in_upsert() {
        assert!(test_parse("INSERT INTO t (a,b) VALUES (1,2) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b"));
    }
}
