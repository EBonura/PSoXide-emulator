//! The fleet manifest: library entry to repo to journey.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_MANIFEST: &str = include_str!("../fleet.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    pub games_dir: String,
    pub repos_dir: String,
    #[serde(default, rename = "game")]
    pub games: Vec<Entry>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    /// Disc the player launches, relative to `games_dir`.
    pub library: String,
    /// Repo directory name under `repos_dir`.
    pub repo: String,
    /// Journey file inside the repo.
    #[serde(default = "default_journey")]
    pub journey: String,
}

fn default_journey() -> String {
    "tests/journey.toml".into()
}

pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

impl Fleet {
    pub fn load(path: Option<&Path>) -> Result<Fleet, String> {
        let text = match path {
            Some(p) => {
                std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?
            }
            None => DEFAULT_MANIFEST.to_string(),
        };
        toml::from_str(&text).map_err(|e| format!("fleet manifest: {e}"))
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.games.iter().find(|g| g.name == name)
    }

    pub fn library_disc(&self, e: &Entry) -> PathBuf {
        expand(&self.games_dir).join(&e.library)
    }

    pub fn repo_dir(&self, e: &Entry, overrides: &[(String, PathBuf)]) -> PathBuf {
        overrides
            .iter()
            .find(|(n, _)| *n == e.name)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| expand(&self.repos_dir).join(&e.repo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifest_parses_and_has_unique_names() {
        let fleet = Fleet::load(None).unwrap();
        assert!(fleet.find("oot").is_some());
        let mut names: Vec<&str> = fleet.games.iter().map(|g| g.name.as_str()).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }
}
