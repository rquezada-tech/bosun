//! One-click app templates — pre-configured Docker containers
//! that users can spin up with a single command.
//!
//! Each template defines the Docker image, default port,
//! environment variables, and persistent volume mounts.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Category of a template (for grouping in UIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Database,
    Cache,
    Proxy,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Database => "database",
            Category::Cache => "cache",
            Category::Proxy => "proxy",
        }
    }
}

/// A pre-configured app template.
#[derive(Debug, Clone)]
pub struct Template {
    /// Short name used as the identifier (e.g. "redis", "postgres").
    pub name: &'static str,
    /// Human-readable description shown in listings.
    pub description: &'static str,
    /// Docker image reference (e.g. "redis:7-alpine").
    pub image: &'static str,
    /// Default container port that the app listens on.
    pub default_port: u16,
    /// Environment variables passed to the container.
    /// The placeholder `{name}` is replaced with the app name at deploy time.
    pub env_vars: HashMap<&'static str, &'static str>,
    /// Volume mounts: `host_path:container_path` pairs.
    /// The placeholder `{name}` is replaced with the app name at deploy time.
    pub volumes: Vec<(&'static str, &'static str)>,
    /// Category for grouping.
    pub category: Category,
}

/// All built-in templates.
static BUILTIN_TEMPLATES: LazyLock<Vec<Template>> = LazyLock::new(|| {
    vec![
        // ── Redis ───────────────────────────────────────────────
        Template {
            name: "redis",
            description: "Redis 7 (Alpine) — in-memory data structure store",
            image: "redis:7-alpine",
            default_port: 6379,
            env_vars: HashMap::new(),
            volumes: vec![("/var/lib/bosun/data/{name}", "/data")],
            category: Category::Cache,
        },
        // ── PostgreSQL ─────────────────────────────────────────
        Template {
            name: "postgres",
            description: "PostgreSQL 16 (Alpine) — relational database",
            image: "postgres:16-alpine",
            default_port: 5432,
            env_vars: HashMap::from([
                ("POSTGRES_PASSWORD", "bosun"),
                ("POSTGRES_USER", "bosun"),
                ("POSTGRES_DB", "{name}"),
            ]),
            volumes: vec![(
                "/var/lib/bosun/data/{name}",
                "/var/lib/postgresql/data",
            )],
            category: Category::Database,
        },
        // ── MySQL ──────────────────────────────────────────────
        Template {
            name: "mysql",
            description: "MySQL 8.4 — relational database",
            image: "mysql:8.4",
            default_port: 3306,
            env_vars: HashMap::from([
                ("MYSQL_ROOT_PASSWORD", "bosun"),
                ("MYSQL_DATABASE", "{name}"),
            ]),
            volumes: vec![("/var/lib/bosun/data/{name}", "/var/lib/mysql")],
            category: Category::Database,
        },
        // ── MongoDB ────────────────────────────────────────────
        Template {
            name: "mongo",
            description: "MongoDB 7 — document database",
            image: "mongo:7",
            default_port: 27017,
            env_vars: HashMap::new(),
            volumes: vec![("/var/lib/bosun/data/{name}", "/data/db")],
            category: Category::Database,
        },
        // ── Nginx ──────────────────────────────────────────────
        Template {
            name: "nginx",
            description: "Nginx (Alpine) — web server / reverse proxy",
            image: "nginx:alpine",
            default_port: 80,
            env_vars: HashMap::new(),
            volumes: vec![(
                "/var/lib/bosun/data/{name}/html",
                "/usr/share/nginx/html",
            )],
            category: Category::Proxy,
        },
    ]
});

/// Look up a built-in template by name.
pub fn get_template(name: &str) -> Option<&'static Template> {
    BUILTIN_TEMPLATES.iter().find(|t| t.name == name)
}

/// Return all built-in templates (as borrowed references).
pub fn list_templates() -> &'static [Template] {
    &BUILTIN_TEMPLATES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_templates_present() {
        let templates = list_templates();
        assert_eq!(templates.len(), 5);
    }

    #[test]
    fn test_get_template_by_name() {
        assert!(get_template("redis").is_some());
        assert!(get_template("postgres").is_some());
        assert!(get_template("mysql").is_some());
        assert!(get_template("mongo").is_some());
        assert!(get_template("nginx").is_some());
        assert!(get_template("nonexistent").is_none());
    }

    #[test]
    fn test_template_redis() {
        let t = get_template("redis").unwrap();
        assert_eq!(t.name, "redis");
        assert_eq!(t.image, "redis:7-alpine");
        assert_eq!(t.default_port, 6379);
        assert!(t.env_vars.is_empty());
        assert_eq!(t.volumes.len(), 1);
        assert_eq!(t.volumes[0].1, "/data");
    }

    #[test]
    fn test_template_postgres() {
        let t = get_template("postgres").unwrap();
        assert_eq!(t.default_port, 5432);
        assert_eq!(t.env_vars.get("POSTGRES_USER"), Some(&"bosun"));
        assert_eq!(t.env_vars.get("POSTGRES_PASSWORD"), Some(&"bosun"));
        assert_eq!(t.env_vars.get("POSTGRES_DB"), Some(&"{name}"));
    }

    #[test]
    fn test_template_categories() {
        for name in &["postgres", "mysql", "mongo"] {
            assert_eq!(get_template(name).unwrap().category, Category::Database);
        }
        assert_eq!(get_template("redis").unwrap().category, Category::Cache);
        assert_eq!(get_template("nginx").unwrap().category, Category::Proxy);
    }
}
