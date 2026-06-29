//! Shell-based hook runner for pre/post deploy hooks.
//!
//! Hooks are shell commands that run on the *host* (not inside the container)
//! before build (pre_deploy) and after deploy + health check (post_deploy).
//! They can be defined in `bosun.hooks.toml` in the app directory or passed
//! via `--pre`/`--post` CLI flags.

use std::collections::HashMap;
use std::path::Path;

/// Parsed representation of a `bosun.hooks.toml` file.
#[derive(Debug, Default, serde::Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_deploy: Option<HookSection>,
    #[serde(default)]
    pub post_deploy: Option<HookSection>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct HookSection {
    #[serde(default)]
    pub commands: Vec<String>,
}

/// Load hooks configuration from a `bosun.hooks.toml` file in the given directory.
///
/// Returns `None` if the file does not exist, or a parsed `HooksConfig` on
/// success. Returns an error if the file exists but can't be parsed.
pub fn load_hooks_from_dir(dir: &Path) -> anyhow::Result<Option<HooksConfig>> {
    let hooks_path = dir.join("bosun.hooks.toml");
    if !hooks_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&hooks_path)?;
    let config: HooksConfig = toml::from_str(&content)?;
    Ok(Some(config))
}

/// Run a list of shell hook commands.
///
/// Each command is executed via `sh -c "<command>"`. Working directory and
/// optional environment variables are set for the child process.
///
/// If any hook fails (non-zero exit code), execution stops immediately and
/// the error is returned with the combined stdout/stderr output.
pub async fn run_hooks(
    hooks: &[String],
    workdir: &Path,
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }

    for (i, hook) in hooks.iter().enumerate() {
        tracing::info!(
            "Running hook {}/{}: {}",
            i + 1,
            hooks.len(),
            hook
        );

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(hook)
            .current_dir(workdir)
            .envs(env)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            let combined = format!(
                "stdout:\n{}\nstderr:\n{}",
                stdout.trim(),
                stderr.trim()
            );
            tracing::error!(
                "Hook {}/{} failed with exit code {:?}: {}",
                i + 1,
                hooks.len(),
                output.status.code(),
                hook
            );
            return Err(anyhow::anyhow!(
                "Hook '{}' failed (exit {:?}):\n{}",
                hook,
                output.status.code(),
                combined
            ));
        }

        // Log output at debug level so it doesn't clutter production logs
        if !stdout.trim().is_empty() {
            tracing::debug!("Hook {} stdout: {}", i + 1, stdout.trim());
        }
        if !stderr.trim().is_empty() {
            tracing::debug!("Hook {} stderr: {}", i + 1, stderr.trim());
        }

        tracing::info!(
            "Hook {}/{} completed successfully",
            i + 1,
            hooks.len()
        );
    }

    Ok(())
}
