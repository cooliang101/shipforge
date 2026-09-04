use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::{ArtifactSpec, BuildCommand, ComponentSetup},
    domain::ComponentName,
};

const MAX_SCAN_DEPTH: usize = 3;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryConfidence {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentCandidate {
    pub name: ComponentName,
    pub setup: ComponentSetup,
    pub source: PathBuf,
    pub confidence: DiscoveryConfidence,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub components: Vec<ComponentCandidate>,
    pub notices: Vec<String>,
}

/// Discovers deployable Component candidates without executing project code or
/// changing the selected directory.
///
/// # Errors
///
/// Returns an error for an invalid root, unreadable manifests, oversized
/// manifests, or malformed JSON/TOML required for a candidate.
pub fn discover_components(project_root: &Path) -> Result<DiscoveryReport, DiscoveryError> {
    if !project_root.is_dir() {
        return Err(DiscoveryError::NotDirectory(project_root.to_owned()));
    }
    let project_root = std::fs::canonicalize(project_root)
        .map_err(|source| DiscoveryError::io(project_root, source))?;
    let directories = manifest_directories(&project_root)?;
    let root_is_cargo_workspace = cargo_workspace_manifest(&project_root.join("Cargo.toml"))?;
    let mut report = DiscoveryReport::default();
    let mut names = BTreeSet::new();

    for directory in directories {
        let working_directory = relative_directory(&project_root, &directory);
        let package_json = directory.join("package.json");
        if package_json.is_file() {
            discover_node(
                &package_json,
                &working_directory,
                &directory,
                &mut report,
                &mut names,
            )?;
        }
        let cargo_toml = directory.join("Cargo.toml");
        if cargo_toml.is_file() {
            discover_cargo(
                &cargo_toml,
                &working_directory,
                &directory,
                root_is_cargo_workspace && directory != project_root,
                &mut report,
                &mut names,
            )?;
        }
        let go_mod = directory.join("go.mod");
        if go_mod.is_file() {
            discover_go(&go_mod, &working_directory, &mut report, &mut names)?;
        }
        if directory.join("Dockerfile").is_file() {
            report.notices.push(format!(
                "{}: Dockerfile detected, but Docker image builds are outside the MVP",
                relative_directory(&project_root, &directory).display()
            ));
        }
    }
    report
        .components
        .sort_by(|left, right| left.name.cmp(&right.name));
    report.notices.sort();
    Ok(report)
}

#[must_use]
pub fn suggest_project_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "project".into(), canonical_name)
}

fn manifest_directories(root: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
    let mut queue = VecDeque::from([(root.to_owned(), 0)]);
    let mut directories = Vec::new();
    while let Some((directory, depth)) = queue.pop_front() {
        if contains_manifest(&directory) {
            directories.push(directory.clone());
        }
        if depth == MAX_SCAN_DEPTH {
            continue;
        }
        let entries = std::fs::read_dir(&directory)
            .map_err(|source| DiscoveryError::io(&directory, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| DiscoveryError::io(&directory, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| DiscoveryError::io(&entry.path(), source))?;
            if file_type.is_dir() && !ignored_directory(&entry.file_name().to_string_lossy()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    directories.sort();
    Ok(directories)
}

fn contains_manifest(directory: &Path) -> bool {
    ["package.json", "Cargo.toml", "go.mod", "Dockerfile"]
        .iter()
        .any(|name| directory.join(name).is_file())
}

fn ignored_directory(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "vendor" | "dist" | "build" | "out"
        )
}

fn relative_directory(root: &Path, directory: &Path) -> PathBuf {
    directory
        .strip_prefix(root)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_owned)
}

#[derive(Deserialize)]
struct NodePackage {
    name: Option<String>,
    #[serde(default)]
    private: bool,
    scripts: Option<std::collections::BTreeMap<String, String>>,
    workspaces: Option<serde_json::Value>,
}

fn discover_node(
    manifest: &Path,
    working_directory: &Path,
    directory: &Path,
    report: &mut DiscoveryReport,
    names: &mut BTreeSet<ComponentName>,
) -> Result<(), DiscoveryError> {
    let contents = read_manifest(manifest)?;
    let package: NodePackage =
        serde_json::from_str(&contents).map_err(|source| DiscoveryError::Json {
            path: manifest.to_owned(),
            source,
        })?;
    let Some(build_script) = package
        .scripts
        .as_ref()
        .and_then(|scripts| scripts.get("build"))
    else {
        report.notices.push(format!(
            "{}: package.json has no build script",
            working_directory.display()
        ));
        return Ok(());
    };
    if package.private && package.workspaces.is_some() && working_directory == Path::new(".") {
        report.notices.push(
            ".: private workspace package is treated as an orchestrator, not a Component".into(),
        );
        return Ok(());
    }

    let raw_name = package
        .name
        .as_deref()
        .or_else(|| directory.file_name().and_then(|name| name.to_str()))
        .unwrap_or("frontend");
    let name = unique_name(raw_name, names);
    let (artifact, confidence) = node_artifact(directory, build_script);
    if build_script.contains("next build") {
        report.notices.push(format!(
            "{}: Next.js output requires standalone or static-export configuration before deployment",
            working_directory.display()
        ));
    }
    let (program, args) = if directory.join("pnpm-lock.yaml").is_file() {
        ("pnpm", vec!["run", "build"])
    } else if directory.join("yarn.lock").is_file() {
        ("yarn", vec!["build"])
    } else if directory.join("bun.lock").is_file() || directory.join("bun.lockb").is_file() {
        ("bun", vec!["run", "build"])
    } else {
        ("npm", vec!["run", "build"])
    };
    report.components.push(ComponentCandidate {
        name,
        setup: ComponentSetup {
            working_directory: Some(working_directory.to_owned()),
            build: vec![BuildCommand::argv(program, args)],
            artifact: ArtifactSpec { path: artifact },
        },
        source: working_directory.join("package.json"),
        confidence,
    });
    Ok(())
}

fn node_artifact(directory: &Path, build_script: &str) -> (PathBuf, DiscoveryConfidence) {
    if build_script.contains("vite") {
        return (PathBuf::from("dist"), DiscoveryConfidence::High);
    }
    if build_script.contains("react-scripts") {
        return (PathBuf::from("build"), DiscoveryConfidence::High);
    }
    if build_script.contains("next build") {
        return (PathBuf::from(".next"), DiscoveryConfidence::Low);
    }
    for candidate in ["dist", "build", "out"] {
        if directory.join(candidate).is_dir() {
            return (PathBuf::from(candidate), DiscoveryConfidence::Medium);
        }
    }
    (PathBuf::from("dist"), DiscoveryConfidence::Low)
}

#[derive(Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
    #[serde(default)]
    bin: Vec<CargoBin>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
}

#[derive(Deserialize)]
struct CargoBin {
    name: Option<String>,
    path: Option<PathBuf>,
}

fn discover_cargo(
    manifest: &Path,
    working_directory: &Path,
    directory: &Path,
    workspace_member: bool,
    report: &mut DiscoveryReport,
    names: &mut BTreeSet<ComponentName>,
) -> Result<(), DiscoveryError> {
    let contents = read_manifest(manifest)?;
    let cargo: CargoManifest =
        toml::from_str(&contents).map_err(|source| DiscoveryError::Toml {
            path: manifest.to_owned(),
            source,
        })?;
    let Some(package) = cargo.package else {
        return Ok(());
    };
    let bins = if cargo.bin.is_empty() && directory.join("src/main.rs").is_file() {
        vec![(package.name.clone(), None)]
    } else {
        cargo
            .bin
            .into_iter()
            .filter_map(|binary| {
                let binary_name = binary.name.or_else(|| {
                    binary
                        .path
                        .as_deref()
                        .and_then(Path::file_stem)
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                });
                binary_name.map(|name| (name, Some(())))
            })
            .collect()
    };
    if bins.is_empty() {
        report.notices.push(format!(
            "{}: Cargo package has no binary target",
            working_directory.display()
        ));
    }
    for (binary, explicit) in bins {
        let name = unique_name(&binary, names);
        let mut args = vec!["build".to_owned(), "--release".to_owned()];
        if workspace_member {
            args.extend(["--package".to_owned(), package.name.clone()]);
        }
        if explicit.is_some() {
            args.extend(["--bin".to_owned(), binary.clone()]);
        }
        report.components.push(ComponentCandidate {
            name,
            setup: ComponentSetup {
                working_directory: Some(if workspace_member {
                    PathBuf::from(".")
                } else {
                    working_directory.to_owned()
                }),
                build: vec![BuildCommand::argv("cargo", args)],
                artifact: ArtifactSpec {
                    path: PathBuf::from("target/release").join(&binary),
                },
            },
            source: working_directory.join("Cargo.toml"),
            confidence: DiscoveryConfidence::High,
        });
    }
    Ok(())
}

fn cargo_workspace_manifest(manifest: &Path) -> Result<bool, DiscoveryError> {
    if !manifest.is_file() {
        return Ok(false);
    }
    let contents = read_manifest(manifest)?;
    let value: toml::Value = toml::from_str(&contents).map_err(|source| DiscoveryError::Toml {
        path: manifest.to_owned(),
        source,
    })?;
    Ok(value.get("workspace").is_some())
}

fn discover_go(
    manifest: &Path,
    working_directory: &Path,
    report: &mut DiscoveryReport,
    names: &mut BTreeSet<ComponentName>,
) -> Result<(), DiscoveryError> {
    let contents = read_manifest(manifest)?;
    let module = contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("module ").map(str::trim))
        .filter(|module| !module.is_empty())
        .ok_or_else(|| DiscoveryError::GoModule(manifest.to_owned()))?;
    let binary = module.rsplit('/').next().unwrap_or("app");
    let name = unique_name(binary, names);
    let artifact = PathBuf::from(name.as_str());
    report.components.push(ComponentCandidate {
        name,
        setup: ComponentSetup {
            working_directory: Some(working_directory.to_owned()),
            build: vec![BuildCommand::argv(
                "go",
                [
                    "build".to_owned(),
                    "-o".to_owned(),
                    artifact.to_string_lossy().into_owned(),
                    ".".to_owned(),
                ],
            )],
            artifact: ArtifactSpec { path: artifact },
        },
        source: working_directory.join("go.mod"),
        confidence: DiscoveryConfidence::High,
    });
    Ok(())
}

fn unique_name(raw: &str, used: &mut BTreeSet<ComponentName>) -> ComponentName {
    let base = canonical_name(raw);
    let mut candidate = ComponentName::parse(base.clone()).expect("canonical name is valid");
    let mut suffix = 2_u32;
    while used.contains(&candidate) {
        let suffix_text = format!("-{suffix}");
        let prefix_length = (63 - suffix_text.len()).min(base.len());
        let prefix = base[..prefix_length].trim_end_matches('-');
        candidate = ComponentName::parse(format!("{prefix}{suffix_text}"))
            .expect("suffixed canonical name is valid");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn canonical_name(raw: &str) -> String {
    let unscoped = raw.rsplit('/').next().unwrap_or(raw);
    let mut result = String::new();
    let mut separator = false;
    for character in unscoped.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            result.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
        if result.len() == 63 {
            break;
        }
    }
    let result = result.trim_matches('-');
    if result.is_empty() {
        "app".into()
    } else {
        result.into()
    }
}

fn read_manifest(path: &Path) -> Result<String, DiscoveryError> {
    let metadata = std::fs::metadata(path).map_err(|source| DiscoveryError::io(path, source))?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(DiscoveryError::ManifestTooLarge(path.to_owned()));
    }
    std::fs::read_to_string(path).map_err(|source| DiscoveryError::io(path, source))
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("Project root is not a directory: `{0}`")]
    NotDirectory(PathBuf),
    #[error("project discovery I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("manifest exceeds the 1 MiB discovery limit: `{0}`")]
    ManifestTooLarge(PathBuf),
    #[error("package.json is invalid at `{path}`: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("Cargo.toml is invalid at `{path}`: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("go.mod has no module directive: `{0}`")]
    GoModule(PathBuf),
}

impl DiscoveryError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_vite_component_and_lockfile_command_without_writing() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"@team/web-app","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("pnpm-lock.yaml"),
            "lockfileVersion: 9",
        )
        .unwrap();

        let report = discover_components(directory.path()).unwrap();
        assert_eq!(report.components.len(), 1);
        let candidate = &report.components[0];
        assert_eq!(candidate.name.as_str(), "web-app");
        assert_eq!(candidate.setup.build[0].program, "pnpm");
        assert_eq!(candidate.setup.artifact.path, Path::new("dist"));
        assert_eq!(candidate.confidence, DiscoveryConfidence::High);
        assert!(!directory.path().join("shipforge.yaml").exists());
    }

    #[test]
    fn discovers_explicit_cargo_binaries_as_independent_components() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            r#"
[package]
name = "services"
version = "0.1.0"

[[bin]]
name = "api"
path = "src/api.rs"

[[bin]]
name = "worker"
path = "src/worker.rs"
"#,
        )
        .unwrap();

        let report = discover_components(directory.path()).unwrap();
        assert_eq!(
            report
                .components
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker"]
        );
        assert!(report.components.iter().all(|candidate| {
            candidate.setup.build[0].args.first().map(String::as_str) == Some("build")
        }));
    }

    #[test]
    fn discovers_nested_go_component_and_ignores_generated_directories() {
        let directory = tempdir().unwrap();
        let service = directory.path().join("services/worker");
        let ignored = directory.path().join("node_modules/bad");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(service.join("go.mod"), "module example.com/team/worker\n").unwrap();
        std::fs::write(ignored.join("go.mod"), "module example.com/bad\n").unwrap();

        let report = discover_components(directory.path()).unwrap();
        assert_eq!(report.components.len(), 1);
        assert_eq!(report.components[0].name.as_str(), "worker");
        assert_eq!(
            report.components[0].setup.working_directory,
            Some(PathBuf::from("services/worker"))
        );
        assert_eq!(
            report.components[0].setup.artifact.path,
            Path::new("worker")
        );
        assert_eq!(
            report.components[0].setup.build[0].args,
            ["build", "-o", "worker", "."]
        );
    }

    #[test]
    fn cargo_workspace_member_builds_from_workspace_root() {
        let directory = tempdir().unwrap();
        let member = directory.path().join("crates/api/src");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/api\"]\nresolver = \"2\"\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("crates/api/Cargo.toml"),
            "[package]\nname = \"api-service\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(member.join("main.rs"), "fn main() {}\n").unwrap();

        let report = discover_components(directory.path()).unwrap();
        assert_eq!(report.components.len(), 1);
        let candidate = &report.components[0];
        assert_eq!(candidate.setup.working_directory, Some(PathBuf::from(".")));
        assert_eq!(
            candidate.setup.build[0].args,
            ["build", "--release", "--package", "api-service"]
        );
        assert_eq!(
            candidate.setup.artifact.path,
            Path::new("target/release/api-service")
        );
    }

    #[test]
    fn private_node_workspace_is_not_mistaken_for_a_component() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"root","private":true,"workspaces":["packages/*"],"scripts":{"build":"turbo build"}}"#,
        )
        .unwrap();

        let report = discover_components(directory.path()).unwrap();
        assert!(report.components.is_empty());
        assert_eq!(report.notices.len(), 1);
    }

    #[test]
    fn project_name_is_canonicalized_from_directory_name() {
        assert_eq!(
            suggest_project_name(Path::new("/work/My Web_App")),
            "my-web-app"
        );
    }
}
