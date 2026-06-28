//! One-click app templates — pre-configured Docker containers
//! that users can spin up with a single command.
//!
//! Templates are loaded from TOML files in a catalog directory.
//! Each app may define multiple versions (e.g. redis:7-alpine, redis:6-alpine),
//! optional environment variables with descriptions, volume mounts, and ports.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// TOML representation of a single app template file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplateFile {
    pub name: String,
    pub description: String,
    pub category: String,
    pub icon: Option<String>,
    pub versions: Vec<VersionFile>,
    #[serde(default)]
    pub env: HashMap<String, EnvVarFile>,
    #[serde(default)]
    pub volumes: HashMap<String, String>,
    #[serde(default)]
    pub ports: HashMap<String, u16>,
}

/// A single version entry in the TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionFile {
    pub version: String,
    pub image: String,
    #[serde(default)]
    pub default: bool,
}

/// Environment variable definition in the TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EnvVarFile {
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default_value: Option<String>,
}

// ── Runtime types (loaded from TOML files) ──────────────────────────

/// Category of a template (for grouping in UIs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Category {
    Database,
    Cache,
    Proxy,
    Queue,
    Monitoring,
    Cms,
    Storage,
    DevTool,
    /// Catch-all for unknown categories.
    Other(String),
}

impl Category {
    pub fn as_str(&self) -> &str {
        match self {
            Category::Database => "database",
            Category::Cache => "cache",
            Category::Proxy => "proxy",
            Category::Queue => "queue",
            Category::Monitoring => "monitoring",
            Category::Cms => "cms",
            Category::Storage => "storage",
            Category::DevTool => "dev-tool",
            Category::Other(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "database" => Category::Database,
            "cache" => Category::Cache,
            "proxy" => Category::Proxy,
            "queue" => Category::Queue,
            "monitoring" => Category::Monitoring,
            "cms" => Category::Cms,
            "storage" => Category::Storage,
            "dev-tool" => Category::DevTool,
            other => Category::Other(other.to_string()),
        }
    }
}

/// A specific version of an app template.
#[derive(Debug, Clone, Serialize)]
pub struct Version {
    /// Display label (e.g. "7-alpine").
    pub version: String,
    /// Docker image reference (e.g. "redis:7-alpine").
    pub image: String,
    /// Whether this is the default version.
    pub default: bool,
}

/// Definition of an environment variable for a template.
#[derive(Debug, Clone, Serialize)]
pub struct EnvVar {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub default_value: Option<String>,
}

/// A fully loaded app template.
#[derive(Debug, Clone, Serialize)]
pub struct Template {
    /// Short name used as the identifier (e.g. "redis", "postgres").
    pub name: String,
    /// Human-readable description shown in listings.
    pub description: String,
    /// Icon URL for UI display.
    pub icon: Option<String>,
    /// Available versions (sorted, default first).
    pub versions: Vec<Version>,
    /// Default resolved image (from the default version).
    pub default_image: String,
    /// Default container port that the app listens on.
    pub default_port: u16,
    /// Environment variables definitions.
    pub env_vars: Vec<EnvVar>,
    /// Volume mounts: container_path -> description.
    pub volumes: Vec<Volume>,
    /// All port definitions.
    pub ports: HashMap<String, u16>,
    /// Category for grouping.
    pub category: Category,
}

/// Volume mount definition.
#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    pub name: String,
    pub container_path: String,
}

// ── Catalog ─────────────────────────────────────────────────────────

/// The app template catalog, loaded from TOML files.
#[derive(Debug, Clone)]
pub struct Catalog {
    templates: Vec<Template>,
}

impl Catalog {
    /// Load all `.toml` files from a directory into a Catalog.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut templates = Vec::new();

        let entries = std::fs::read_dir(path)?;
        for entry in entries {
            let entry = entry?;
            let file_path = entry.path();

            if file_path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }

            let content = std::fs::read_to_string(&file_path)?;
            let tf: TemplateFile = toml::from_str(&content)?;
            let template = Self::convert_template(&tf);
            templates.push(template);
        }

        // Sort by name for consistent ordering
        templates.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self { templates })
    }

    /// Create an empty catalog (useful for testing).
    pub fn empty() -> Self {
        Self {
            templates: Vec::new(),
        }
    }

    /// Convert a TemplateFile to a runtime Template.
    fn convert_template(tf: &TemplateFile) -> Template {
        // Sort versions: default first, then by label
        let mut versions: Vec<Version> = tf
            .versions
            .iter()
            .map(|v| Version {
                version: v.version.clone(),
                image: v.image.clone(),
                default: v.default,
            })
            .collect();
        versions.sort_by(|a, b| {
            b.default
                .cmp(&a.default)
                .then_with(|| a.version.cmp(&b.version))
        });

        // Resolve default image
        let default_image = versions
            .iter()
            .find(|v| v.default)
            .or_else(|| versions.first())
            .map(|v| v.image.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Resolve default port (first port named "main", or first port)
        let default_port = tf
            .ports
            .get("main")
            .copied()
            .or_else(|| tf.ports.values().next().copied())
            .unwrap_or(0);

        // Convert env vars
        let env_vars: Vec<EnvVar> = tf
            .env
            .iter()
            .map(|(name, ev)| EnvVar {
                name: name.clone(),
                description: ev.description.clone(),
                required: ev.required,
                default_value: ev.default_value.clone(),
            })
            .collect();

        // Convert volumes
        let volumes: Vec<Volume> = tf
            .volumes
            .iter()
            .map(|(name, container_path)| Volume {
                name: name.clone(),
                container_path: container_path.clone(),
            })
            .collect();

        let category = Category::from_str(&tf.category);

        Template {
            name: tf.name.clone(),
            description: tf.description.clone(),
            icon: tf.icon.clone(),
            versions,
            default_image,
            default_port,
            env_vars,
            volumes,
            ports: tf.ports.clone(),
            category,
        }
    }

    /// List all templates in the catalog.
    pub fn list_templates(&self) -> &[Template] {
        &self.templates
    }

    /// Look up a template by name, optionally resolving a specific version.
    ///
    /// Returns `(template, resolved_image)` where `resolved_image` is the Docker
    /// image for the requested version (or the default if none specified).
    pub fn get_template(&self, name: &str, version_opt: Option<&str>) -> Option<(&Template, String)> {
        let template = self.templates.iter().find(|t| t.name == name)?;

        let image = if let Some(req_version) = version_opt {
            template
                .versions
                .iter()
                .find(|v| v.version == req_version)
                .map(|v| v.image.clone())
                .or_else(|| {
                    // Fall back to default version
                    template
                        .versions
                        .iter()
                        .find(|v| v.default)
                        .or_else(|| template.versions.first())
                        .map(|v| v.image.clone())
                })?
        } else {
            template.default_image.clone()
        };

        Some((template, image))
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_temp_catalog() -> (std::path::PathBuf, Catalog) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bosun-test-{}-{}", std::process::id(), ts));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let redis_toml = r#"
name = "redis"
description = "In-memory data structure store"
category = "cache"
icon = "https://example.com/redis.png"

[[versions]]
version = "7-alpine"
image = "redis:7-alpine"
default = true

[[versions]]
version = "6-alpine"
image = "redis:6-alpine"

[env.REDIS_PASSWORD]
description = "Password"
required = false

[volumes]
data = "/data"

[ports]
main = 6379
"#;
        let mut f = std::fs::File::create(dir.join("redis.toml")).unwrap();
        f.write_all(redis_toml.as_bytes()).unwrap();

        let nginx_toml = r#"
name = "nginx"
description = "Web server / reverse proxy"
category = "proxy"
icon = "https://example.com/nginx.png"

[[versions]]
version = "alpine"
image = "nginx:alpine"
default = true

[volumes]
html = "/usr/share/nginx/html"

[ports]
main = 80
"#;
        let mut f = std::fs::File::create(dir.join("nginx.toml")).unwrap();
        f.write_all(nginx_toml.as_bytes()).unwrap();

        let catalog = Catalog::load(&dir).unwrap();
        (dir, catalog)
    }

    #[test]
    fn test_load_catalog() {
        let (_dir, catalog) = create_temp_catalog();
        let templates = catalog.list_templates();
        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].name, "nginx"); // sorted alphabetically
        assert_eq!(templates[1].name, "redis");
    }

    #[test]
    fn test_get_template_default_version() {
        let (_dir, catalog) = create_temp_catalog();
        let (template, image) = catalog.get_template("redis", None).unwrap();
        assert_eq!(template.name, "redis");
        assert_eq!(image, "redis:7-alpine");
        assert_eq!(template.default_port, 6379);
        assert_eq!(template.versions.len(), 2);
        assert_eq!(template.category, Category::Cache);
    }

    #[test]
    fn test_get_template_specific_version() {
        let (_dir, catalog) = create_temp_catalog();
        let (_template, image) = catalog.get_template("redis", Some("6-alpine")).unwrap();
        assert_eq!(image, "redis:6-alpine");
    }

    #[test]
    fn test_get_template_nonexistent() {
        let (_dir, catalog) = create_temp_catalog();
        assert!(catalog.get_template("nonexistent", None).is_none());
    }

    #[test]
    fn test_get_template_bad_version_falls_back() {
        let (_dir, catalog) = create_temp_catalog();
        // Unknown version should fall back to default
        let (_template, image) = catalog.get_template("redis", Some("99-alpine")).unwrap();
        assert_eq!(image, "redis:7-alpine");
    }

    #[test]
    fn test_env_vars() {
        let (_dir, catalog) = create_temp_catalog();
        let (template, _) = catalog.get_template("redis", None).unwrap();
        assert_eq!(template.env_vars.len(), 1);
        assert_eq!(template.env_vars[0].name, "REDIS_PASSWORD");
        assert_eq!(template.env_vars[0].description, "Password");
        assert!(!template.env_vars[0].required);
    }

    #[test]
    fn test_volumes() {
        let (_dir, catalog) = create_temp_catalog();
        let (template, _) = catalog.get_template("redis", None).unwrap();
        assert_eq!(template.volumes.len(), 1);
        assert_eq!(template.volumes[0].name, "data");
        assert_eq!(template.volumes[0].container_path, "/data");
    }

    #[test]
    fn test_icon() {
        let (_dir, catalog) = create_temp_catalog();
        let (template, _) = catalog.get_template("redis", None).unwrap();
        assert_eq!(
            template.icon.as_deref(),
            Some("https://example.com/redis.png")
        );
    }
}
