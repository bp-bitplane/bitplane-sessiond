//! The sidebar model: the planes directory, drafts, `bp list` rows and the
//! gone rows merged into one sorted list, ported from cc's planes.rs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bitplane_protocol::plane::{PlaneKind, PlaneMember, PlaneRow, Project};
use serde::Deserialize;
use serde_json::Value;

use crate::env::EnvMap;

pub const PLANE_FILE: &str = "plane.toml";
pub const BITPLANE_DIR: &str = ".bitplane";

/// What `merge` remembers between runs.
#[derive(Default)]
pub struct Rows {
    /// The last `bp list` that answered.
    pub listed: Vec<PlaneRow>,
    /// Every id this daemon listed, with its directory, for the gone rows.
    known: BTreeMap<String, String>,
}

impl Rows {
    /// Listed planes win over drafts; an id seen before and now in neither is gone.
    pub fn merge(&mut self, drafts: Vec<PlaneRow>) -> Vec<PlaneRow> {
        let mut rows: BTreeMap<String, PlaneRow> = BTreeMap::new();
        for row in self.listed.iter().cloned().chain(drafts) {
            rows.entry(row.id.clone()).or_insert(row);
        }
        for (id, directory) in &self.known {
            rows.entry(id.clone()).or_insert_with(|| PlaneRow {
                id: id.clone(),
                directory: directory.clone(),
                kind: PlaneKind::Gone,
                members: Vec::new(),
                findings: Vec::new(),
            });
        }
        for row in rows.values() {
            if row.kind != PlaneKind::Gone {
                self.known.insert(row.id.clone(), row.directory.clone());
            }
        }
        rows.into_values().collect()
    }
}

/// Resolves the planes directory the way bp does: `$BITPLANE_PLANES_DIR`,
/// then `planes_dir` in bp's config file, then `~/planes`.
pub fn resolve_planes_dir(env: &EnvMap) -> PathBuf {
    let home = env.get("HOME").map(PathBuf::from).unwrap_or_default();
    if let Some(dir) = env.get("BITPLANE_PLANES_DIR").filter(|d| !d.is_empty()) {
        return expand_tilde(dir, &home);
    }
    if let Some(dir) = config_planes_dir(&config_path(env, &home)) {
        return expand_tilde(&dir, &home);
    }
    home.join("planes")
}

/// bp's plane id rules: `[a-z0-9][a-z0-9._-]*`, at most 64 characters, and
/// not starting with the reserved `bp-`.
pub fn validate_plane_id(id: &str) -> Result<(), String> {
    let mut chars = id.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if id.is_empty() {
        Err("Enter an id".into())
    } else if !first_ok || !rest_ok {
        Err(
            "Use lowercase letters, digits, '.', '_' and '-', starting with a letter or digit"
                .into(),
        )
    } else if id.len() > 64 {
        Err("At most 64 characters".into())
    } else if id.starts_with("bp-") {
        Err("Ids starting with 'bp-' are reserved".into())
    } else {
        Ok(())
    }
}

/// Every subfolder without a plane file, skipping dot-folders. A missing
/// planes directory has no drafts.
pub fn scan_drafts(planes_dir: &Path) -> Vec<PlaneRow> {
    let Ok(entries) = std::fs::read_dir(planes_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let path = entry.path();
            (!id.starts_with('.') && !path.join(PLANE_FILE).exists()).then(|| PlaneRow {
                id,
                directory: path.to_string_lossy().into_owned(),
                kind: PlaneKind::Draft,
                members: Vec::new(),
                findings: Vec::new(),
            })
        })
        .collect()
}

/// The rows of a `plane_list` response, decoding only what the model shows.
pub fn listed(response: &Value) -> Result<Vec<PlaneRow>, String> {
    #[derive(Deserialize)]
    struct List {
        planes: Vec<Listed>,
    }
    #[derive(Deserialize)]
    struct Listed {
        id: String,
        directory: PathBuf,
        #[serde(default)]
        members: Vec<Member>,
        #[serde(default)]
        health: Option<Health>,
    }
    #[derive(Deserialize)]
    struct Member {
        member: String,
        path: PathBuf,
    }
    #[derive(Deserialize)]
    struct Health {
        #[serde(default)]
        findings: Vec<Finding>,
    }
    #[derive(Deserialize)]
    struct Finding {
        finding: String,
    }
    let list: List = decode(response, "plane_list")?;
    Ok(list
        .planes
        .into_iter()
        .map(|plane| PlaneRow {
            members: plane
                .members
                .into_iter()
                .map(|member| PlaneMember {
                    member: member.member,
                    path: plane.directory.join(member.path).to_string_lossy().into(),
                })
                .collect(),
            findings: plane
                .health
                .map(|health| health.findings.into_iter().map(|f| f.finding).collect())
                .unwrap_or_default(),
            id: plane.id,
            directory: plane.directory.to_string_lossy().into(),
            kind: PlaneKind::Plane,
        })
        .collect())
}

/// The projects of a `project_list` response, sorted by name. A row that
/// failed to read carries no `source_repo_present`, and counts as absent.
pub fn projects(response: &Value) -> Result<Vec<Project>, String> {
    #[derive(Deserialize)]
    struct Listing {
        projects: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        project: String,
        #[serde(default)]
        detail: Option<Value>,
    }
    let listing: Listing = decode(response, "project_list")?;
    let mut projects: Vec<Project> = listing
        .projects
        .into_iter()
        .map(|row| Project {
            source_repo_present: row
                .detail
                .as_ref()
                .and_then(|detail| detail.get("source_repo_present"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            name: row.project,
        })
        .collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(projects)
}

/// Paths the watcher observes, none recursively: the planes directory (or
/// its parent while it doesn't exist), and each plane's root and `.bitplane/`.
pub fn watch_paths(planes_dir: &Path, rows: &[PlaneRow]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    if planes_dir.is_dir() {
        paths.insert(planes_dir.to_path_buf());
    } else if let Some(parent) = planes_dir.parent().filter(|p| p.is_dir()) {
        paths.insert(parent.to_path_buf());
    }
    for row in rows.iter().filter(|row| row.kind != PlaneKind::Gone) {
        let dir = PathBuf::from(&row.directory);
        for path in [dir.join(BITPLANE_DIR), dir] {
            if path.is_dir() {
                paths.insert(path);
            }
        }
    }
    paths
}

fn decode<T: serde::de::DeserializeOwned>(response: &Value, action: &str) -> Result<T, String> {
    let found = response.get("action").and_then(Value::as_str);
    if found != Some(action) {
        return Err(format!(
            "bp answered a `{action}` request with {}",
            found.map_or_else(|| "no action".into(), |a| format!("`{a}`"))
        ));
    }
    serde_json::from_value(response.clone())
        .map_err(|error| format!("bp's `{action}` response could not be read: {error}"))
}

fn config_path(env: &EnvMap, home: &Path) -> PathBuf {
    if let Some(path) = env.get("BITPLANE_CONFIG").filter(|p| !p.is_empty()) {
        return expand_tilde(path, home);
    }
    let config_home = env
        .get("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config_home.join("bitplane").join("config.toml")
}

fn config_planes_dir(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = text.parse().ok()?;
    table.get("planes_dir")?.as_str().map(str::to_owned)
}

fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn row(id: &str, kind: PlaneKind) -> PlaneRow {
        PlaneRow {
            id: id.into(),
            directory: format!("/p/{id}"),
            kind,
            members: Vec::new(),
            findings: Vec::new(),
        }
    }

    #[test]
    fn validates_plane_ids() {
        for ok in ["a", "0", "feat-x", "v1.2_rc", &"a".repeat(64)] {
            assert_eq!(validate_plane_id(ok), Ok(()), "{ok}");
        }
        for bad in ["", "-a", ".a", "A", "a/b", "a b", "bp-x", &"a".repeat(65)] {
            assert!(validate_plane_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_planes_dir_resolves_env_then_config_then_home() {
        let config = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config.path().join("bitplane")).unwrap();
        std::fs::write(
            config.path().join("bitplane/config.toml"),
            "version = 1\nplanes_dir = \"~/elsewhere\"\n",
        )
        .unwrap();
        let xdg = config.path().to_str().unwrap();

        assert_eq!(
            resolve_planes_dir(&env(&[
                ("HOME", "/h"),
                ("XDG_CONFIG_HOME", xdg),
                ("BITPLANE_PLANES_DIR", "~/work")
            ])),
            PathBuf::from("/h/work")
        );
        assert_eq!(
            resolve_planes_dir(&env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", xdg)])),
            PathBuf::from("/h/elsewhere")
        );
        assert_eq!(
            resolve_planes_dir(&env(&[("HOME", "/nonexistent-home")])),
            PathBuf::from("/nonexistent-home/planes")
        );
    }

    #[test]
    fn drafts_are_folders_without_a_plane_file() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["real", "draft", ".hidden"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("real").join(PLANE_FILE), "").unwrap();
        std::fs::write(dir.path().join("file.txt"), "").unwrap();

        let ids: Vec<_> = scan_drafts(dir.path()).into_iter().map(|d| d.id).collect();

        assert_eq!(ids, ["draft"]);
        assert!(scan_drafts(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn a_listed_plane_has_absolute_members_and_finding_tags() {
        let response = json!({"action": "plane_list", "planes": [{
            "id": "auth", "directory": "/p/auth", "created_at": null,
            "members": [{"member": "@api", "path": "api", "head": null}],
            "health": {"checked": "cheap", "findings": [{"finding": "prunable", "member": "@api", "path": "api"}]},
        }]});

        let rows = listed(&response).unwrap();

        assert_eq!(rows[0].members[0].path, "/p/auth/api");
        assert_eq!(rows[0].findings, ["prunable"]);
        assert!(listed(&json!({"action": "plane_show"})).is_err());
    }

    #[test]
    fn a_vanished_row_is_gone_until_it_comes_back() {
        let mut rows = Rows {
            listed: vec![row("a", PlaneKind::Plane)],
            ..Rows::default()
        };
        rows.merge(vec![row("b", PlaneKind::Draft), row("a", PlaneKind::Draft)]);

        rows.listed.clear();
        let merged = rows.merge(Vec::new());
        let kinds: Vec<_> = merged.iter().map(|r| (r.id.as_str(), r.kind)).collect();
        assert_eq!(kinds, [("a", PlaneKind::Gone), ("b", PlaneKind::Gone)]);

        let merged = rows.merge(vec![row("b", PlaneKind::Draft)]);
        assert_eq!(merged[1].kind, PlaneKind::Draft);
    }
}
