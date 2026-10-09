//! Query cache for rewritten SQL.
//!
//! LRU cache to avoid re-parsing and rewriting identical queries.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use super::parser::QueryAnalysis;

/// Thread-safe LRU cache for rewritten queries.
pub struct QueryCache {
    cache: Mutex<LruCache<CacheKey, CachedQuery>>,
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct CacheKey {
    schema: String,
    sql: String,
}

#[derive(Clone)]
struct CachedQuery {
    analysis: CachedAnalysis,
}

/// Cached query analysis (without the full AST, just what we need).
#[derive(Clone)]
pub struct CachedAnalysis {
    pub sql: String,
    pub query_type: super::parser::QueryType,
    pub table_names: Vec<String>,
    pub write_tables: Vec<String>,
}

impl QueryCache {
    /// Create a new query cache with the given capacity.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).expect("1000 is non-zero"));
        Self {
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Get a cached query analysis, if present.
    /// Returns None if the cache is poisoned (another thread panicked while holding the lock).
    pub fn get(&self, schema: &str, sql: &str) -> Option<CachedAnalysis> {
        let key = CacheKey {
            schema: schema.to_string(),
            sql: sql.to_string(),
        };
        // Recover from poisoned mutex - cache data is still usable
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.get(&key).map(|c| c.analysis.clone())
    }

    /// Insert a query analysis into the cache.
    /// Silently skips insertion if the cache is poisoned.
    pub fn insert(&self, schema: &str, sql: &str, analysis: &QueryAnalysis) {
        let key = CacheKey {
            schema: schema.to_string(),
            sql: sql.to_string(),
        };
        let cached = CachedQuery {
            analysis: CachedAnalysis {
                sql: analysis.sql.clone(),
                query_type: analysis.query_type,
                table_names: analysis.tables.iter().map(|t| t.name.clone()).collect(),
                write_tables: analysis
                    .tables
                    .iter()
                    .filter(|t| t.is_write_target)
                    .map(|t| t.name.clone())
                    .collect(),
            },
        };
        // Recover from poisoned mutex - cache data is still usable
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.put(key, cached);
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        // Recover from poisoned mutex - cache data is still usable
        let cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        CacheStats {
            len: cache.len(),
            cap: cache.cap().get(),
        }
    }
}

/// Cache statistics.
#[derive(Debug)]
pub struct CacheStats {
    pub len: usize,
    pub cap: usize,
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewriter::parser::{QueryType, TableRef};

    #[test]
    fn test_cache_insert_get() {
        let cache = QueryCache::new(100);

        let analysis = QueryAnalysis {
            sql: "SELECT * FROM schema._view_users".to_string(),
            query_type: QueryType::Select,
            tables: vec![TableRef {
                name: "users".to_string(),
                is_write_target: false,
            }],
        };

        cache.insert("schema", "SELECT * FROM users", &analysis);

        let cached = cache.get("schema", "SELECT * FROM users").unwrap();
        assert_eq!(cached.sql, "SELECT * FROM schema._view_users");
        assert_eq!(cached.query_type, QueryType::Select);
        assert_eq!(cached.table_names, vec!["users"]);
    }

    #[test]
    fn test_cache_miss() {
        let cache = QueryCache::new(100);
        assert!(cache.get("schema", "SELECT 1").is_none());
    }

    #[test]
    fn test_cache_different_schemas() {
        let cache = QueryCache::new(100);

        let analysis1 = QueryAnalysis {
            sql: "SELECT * FROM s1._view_users".to_string(),
            query_type: QueryType::Select,
            tables: vec![],
        };
        let analysis2 = QueryAnalysis {
            sql: "SELECT * FROM s2._view_users".to_string(),
            query_type: QueryType::Select,
            tables: vec![],
        };

        cache.insert("s1", "SELECT * FROM users", &analysis1);
        cache.insert("s2", "SELECT * FROM users", &analysis2);

        let c1 = cache.get("s1", "SELECT * FROM users").unwrap();
        let c2 = cache.get("s2", "SELECT * FROM users").unwrap();

        assert_eq!(c1.sql, "SELECT * FROM s1._view_users");
        assert_eq!(c2.sql, "SELECT * FROM s2._view_users");
    }

    #[test]
    fn test_cache_lru_eviction() {
        let cache = QueryCache::new(2);

        let analysis = QueryAnalysis {
            sql: "rewritten".to_string(),
            query_type: QueryType::Select,
            tables: vec![],
        };

        cache.insert("s", "q1", &analysis);
        cache.insert("s", "q2", &analysis);
        cache.insert("s", "q3", &analysis); // Should evict q1

        assert!(cache.get("s", "q1").is_none());
        assert!(cache.get("s", "q2").is_some());
        assert!(cache.get("s", "q3").is_some());
    }
}
