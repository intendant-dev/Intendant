use crate::error::CallerError;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub memory: MemoryConfig,
}

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    pub fn detect() -> Result<Self, CallerError> {
        let root = detect_project_root()?;
        let config_path = root.join("agent.toml");
        let config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)
                .map_err(|e| CallerError::Config(format!("Failed to read agent.toml: {}", e)))?;
            toml::from_str(&content)
                .map_err(|e| CallerError::Toml(format!("Failed to parse agent.toml: {}", e)))?
        } else {
            ProjectConfig::default()
        };
        Ok(Self { root, config })
    }

    pub fn memory_path(&self) -> PathBuf {
        self.root.join(".agent").join("memory.json")
    }

    pub fn agent_dir(&self) -> PathBuf {
        self.root.join(".agent")
    }
}

fn detect_project_root() -> Result<PathBuf, CallerError> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    std::env::current_dir()
        .map_err(|e| CallerError::Config(format!("Failed to get current directory: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_config() {
        let config = ProjectConfig::default();
        assert!(config.memory.enabled);
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
[memory]
enabled = true
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.memory.enabled);
    }

    #[test]
    fn parse_empty_config() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.memory.enabled); // default_true
    }

    #[test]
    fn parse_partial_config() {
        let toml_str = r#"
[memory]
enabled = false
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.memory.enabled);
    }

    #[test]
    fn project_paths() {
        let project = Project {
            root: PathBuf::from("/tmp/myproject"),
            config: ProjectConfig::default(),
        };
        assert_eq!(project.memory_path(), PathBuf::from("/tmp/myproject/.agent/memory.json"));
        assert_eq!(project.agent_dir(), PathBuf::from("/tmp/myproject/.agent"));
    }
}
