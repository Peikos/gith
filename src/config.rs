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

    pub fn classroom_template_path(&self, classroom_slug: &str, repo_name: &str) -> PathBuf {
        self.repos_dir()
            .join(classroom_slug)
            .join(repo_name)
            .with_extension("git")
    }

    pub fn student_repo_path(
        &self,
        classroom_slug: &str,
        repo_name: &str,
        user_id: i64,
    ) -> PathBuf {
        self.repos_dir()
            .join(classroom_slug)
            .join(repo_name)
            .join("students")
            .join(user_id.to_string())
            .with_extension("git")
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
