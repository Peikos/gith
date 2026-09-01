use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
}

impl Config {
    pub fn new(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = match data_dir {
            Some(d) => d,
            None => default_data_dir().context("failed to determine default data directory")?,
        };
        Ok(Self { data_dir })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("gith.db")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    pub fn user_repo_path(&self, user_id: i64) -> PathBuf {
        self.repos_dir().join(user_id.to_string()).join("repo.git")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data directory {:?}", self.data_dir))?;
        std::fs::create_dir_all(self.repos_dir())
            .with_context(|| format!("creating repos directory {:?}", self.repos_dir()))?;
        Ok(())
    }
}

fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("gith"))
}

pub fn resolve_data_dir<P: AsRef<Path>>(path: P) -> PathBuf {
    let p = path.as_ref();
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}
