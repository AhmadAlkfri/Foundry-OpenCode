use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{self, Context};
use serde_json::{Value, json};

const PACKAGE: &str = r#"{"private":true,"type":"module","scripts":{"postinstall":"node patch-opencode-rag.mjs"},"dependencies":{"@opencode-ai/plugin":"1.17.10","opencode-rag-plugin":"1.22.1"}}"#;
const LEGACY_PLUGIN: &str = include_str!("legacy-rag-plugin.txt");
const PATCH: &str = include_str!("rag-patch.mjs");
const CLI: &str = include_str!("rag-cli.mjs");
const LEGACY_CLI: &str = include_str!("rag-cli-legacy.mjs");
const IGNORE: &str = "node_modules/\nrag_db/\nrag_db.backup-*/\nrag_db_tmp/\nopencode-rag.log\nruntime-overrides.json\n";
// Build caches, local ontology state and generated SDKs churn on every build and crowd out source code.
const EXCLUDE_DIRS: [&str; 11] = [
    "node_modules",
    ".git",
    ".opencode",
    "build",
    "dist",
    ".venv",
    "__pycache__",
    ".nx",
    ".palantir",
    "osdk-output",
    "generated-imports",
];
/// Earlier installs wrote this list; their rag.json is upgraded in place.
const LEGACY_EXCLUDE_DIRS: [&str; 7] = [
    "node_modules",
    ".git",
    ".opencode",
    "build",
    "dist",
    ".venv",
    "__pycache__",
];

fn rag_config(base_url: &str, exclude_dirs: &[&str]) -> String {
    format!(
        "{}\n",
        json!({
            "embedding": {"provider": "openai", "baseUrl": format!("{base_url}/api/v2/llm/proxy/openai/v1"), "model": "ri.language-model-service..language-model.text-embedding-3-large_azure", "timeoutMs": 60000, "vectorDimension": 3072},
            "vectorStore": {"path": "./.opencode/rag_db"},
            "description": {"enabled": false},
            "imageDescription": {"enabled": false},
            "indexing": {"excludeDirs": exclude_dirs, "excludeFiles": ["**/*.png", "**/*.jpg", "**/*.jpeg", "**/*.pdf", "package-lock.json", "pnpm-lock.yaml"], "embedBatchSize": 32, "embedConcurrency": 2}
        })
    )
}

pub fn ready(project: &Path, base_url: &str) -> bool {
    files(project, base_url)
        .iter()
        .all(|(path, expected)| fs::read_to_string(path).is_ok_and(|text| text == *expected))
        && project.join(".opencode/rag_db").is_dir()
        && project
            .join(".opencode/node_modules/opencode-rag-plugin/dist/cli/index.js")
            .is_file()
}

pub fn legacy_plugin(project: &Path) -> eyre::Result<Option<PathBuf>> {
    let open = project.join(".opencode");
    let dir = open.join("plugins");
    let path = dir.join("rag-plugin.js");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    eyre::ensure!(
        fs::symlink_metadata(&open)?.is_dir()
            && !fs::symlink_metadata(&open)?.file_type().is_symlink()
            && fs::symlink_metadata(&dir)?.is_dir()
            && !fs::symlink_metadata(&dir)?.file_type().is_symlink()
            && metadata.is_file()
            && !metadata.file_type().is_symlink(),
        "Unsafe legacy RAG plugin path"
    );
    eyre::ensure!(
        fs::read_to_string(&path)? == LEGACY_PLUGIN,
        "{} is not the known V1 RAG wrapper; leave it untouched and migrate it manually",
        path.display()
    );
    Ok(Some(path))
}

fn files(project: &Path, base_url: &str) -> Vec<(std::path::PathBuf, String)> {
    vec![
        (
            project.join(".opencode/package.json"),
            format!("{PACKAGE}\n"),
        ),
        (
            project.join(".opencode/patch-opencode-rag.mjs"),
            PATCH.to_owned(),
        ),
        (project.join(".opencode/rag-cli.mjs"), CLI.to_owned()),
        (project.join(".opencode/.gitignore"), IGNORE.to_owned()),
        (
            project.join(".opencode/rag.json"),
            rag_config(base_url, &EXCLUDE_DIRS),
        ),
    ]
}

fn setup_files(
    project: &Path,
    base_url: &str,
    dry_run: bool,
) -> eyre::Result<(String, PathBuf, Option<String>, Value)> {
    eyre::ensure!(
        crate::ops::valid_url(base_url),
        "Initialize a stack with an HTTPS Foundry host first"
    );
    let files = files(project, base_url);
    let directory = project.join(".opencode");
    if let Ok(metadata) = fs::symlink_metadata(&directory) {
        eyre::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{} must be a real directory",
            directory.display()
        );
    }
    let mut reused = 0;
    let mut package_owned = false;
    let mut rag_owned = false;
    let mut missing = Vec::new();
    let mut legacy_cli = false;
    let mut legacy_rag = false;
    let legacy_config = rag_config(base_url, &LEGACY_EXCLUDE_DIRS);
    for (index, (path, expected)) in files.iter().enumerate() {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                eyre::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "Unsafe RAG file {}",
                    path.display()
                );
                let existing = fs::read_to_string(path)?;
                eyre::ensure!(
                    existing == *expected
                        || index == 2 && existing == LEGACY_CLI
                        || index == 4 && existing == legacy_config,
                    "{} differs from the installer RAG file; not changing it",
                    path.display()
                );
                if index == 2 && existing == LEGACY_CLI {
                    legacy_cli = true;
                }
                if index == 4 && existing == legacy_config {
                    legacy_rag = true;
                }
                reused += 1;
                if index == 0 {
                    package_owned = true;
                }
                if index == 4 {
                    rag_owned = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.push(path),
            Err(error) => return Err(error.into()),
        }
    }
    for name in ["package-lock.json", "node_modules", "rag_db"] {
        let path = directory.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        eyre::ensure!(
            package_owned
                && (name != "rag_db" || rag_owned)
                && if name == "package-lock.json" {
                    metadata.is_file() && !metadata.file_type().is_symlink()
                } else {
                    metadata.is_dir() && !metadata.file_type().is_symlink()
                },
            "{} already exists and is not verified installer RAG data; not changing it",
            path.display()
        );
        if name == "package-lock.json" {
            let lock: Value = serde_json::from_slice(&fs::read(&path)?)
                .wrap_err_with(|| format!("Invalid {}; not changing it", path.display()))?;
            let package: Value = serde_json::from_str(PACKAGE)?;
            eyre::ensure!(
                lock["packages"][""]["dependencies"] == package["dependencies"],
                "{} does not match the installer dependencies; not changing it",
                path.display()
            );
        }
    }
    let jsonc = project.join(".opencode/opencode.jsonc");
    let json = project.join(".opencode/opencode.json");
    eyre::ensure!(
        !(json.exists() && jsonc.exists()),
        "Both OpenCode config formats exist in {}; review them manually",
        directory.display()
    );
    let config = if jsonc.exists() { jsonc } else { json };
    if let Ok(metadata) = fs::symlink_metadata(&config) {
        eyre::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "{} must be a regular file",
            config.display()
        );
    }
    let original = match fs::read_to_string(&config) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut doc: Value = match &original {
        Some(text) => jsonc_parser::parse_to_serde_value(text, &Default::default())
            .wrap_err("Invalid OpenCode JSON; not changing it")?,
        None => json!({"$schema": "https://opencode.ai/config.json"}),
    };
    eyre::ensure!(doc.is_object(), "OpenCode config must be a JSON object");
    let mcp = doc
        .as_object_mut()
        .unwrap()
        .entry("mcp")
        .or_insert_with(|| json!({}));
    eyre::ensure!(mcp.is_object(), "OpenCode mcp config must be an object");
    let servers = mcp
        .as_object_mut()
        .unwrap()
        .entry("servers")
        .or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| eyre::eyre!("OpenCode mcp.servers must be an object"))?;
    eyre::ensure!(
        !servers.contains_key("rag"),
        "mcp.servers.rag already exists; not replacing it"
    );
    servers.insert(
        "rag".into(),
        json!({"type": "local", "command": ["node", ".opencode/rag-cli.mjs", "mcp"]}),
    );
    if let Some(reference) = doc["providers"]["palantir-openai"]["settings"]["apiKey"].as_str()
        && reference.starts_with("{file:/")
    {
        doc["mcp"]["servers"]["rag"]["environment"] = json!({"OPENAI_API_KEY": reference});
    }
    let paths = files
        .iter()
        .map(|(p, _)| p.display().to_string())
        .chain(std::iter::once(config.display().to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    if dry_run {
        return Ok((
            format!(
                "Would {} RAG files and add the MCP server to OpenCode config:\n{paths}\nWould run npm install in .opencode and index this project.",
                if reused > 0 {
                    "reuse matching or create missing"
                } else {
                    "create"
                }
            ),
            config,
            original,
            doc,
        ));
    }
    fs::create_dir_all(project.join(".opencode"))?;
    let mut created = Vec::new();
    let result = (|| -> eyre::Result<()> {
        for (path, text) in &files {
            if !missing.contains(&path) {
                continue;
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            created.push(path.clone());
            file.write_all(text.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        for path in created {
            let _ = fs::remove_file(path);
        }
        return Err(e);
    }
    for (upgrade, index) in [(legacy_cli, 2), (legacy_rag, 4)] {
        if !upgrade {
            continue;
        }
        let (path, text) = &files[index];
        let backup = crate::ops::backup_path(path);
        fs::rename(path, &backup)?;
        if let Err(error) = fs::write(path, text) {
            fs::rename(&backup, path)?;
            return Err(error.into());
        }
    }
    Ok((paths, config, original, doc))
}

fn activate_config(config: &Path, original: Option<&str>, doc: &Value) -> eyre::Result<()> {
    // Keep the MCP server disabled until installation and indexing have succeeded.
    eyre::ensure!(
        fs::read_to_string(config).ok().as_deref() == original,
        "{} changed during setup",
        config.display()
    );
    let staged = crate::ops::backup_path(config);
    let text = if config.extension().is_some_and(|ext| ext == "jsonc") {
        let root = jsonc_parser::cst::CstRootNode::parse(original.unwrap(), &Default::default())?;
        let object = root.object_value_or_set();
        let mcp = if let Some(prop) = object.get("mcp") {
            prop.object_value().unwrap()
        } else {
            object.append("mcp", jsonc_parser::json!({}));
            object.get("mcp").unwrap().object_value().unwrap()
        };
        let servers = if let Some(prop) = mcp.get("servers") {
            prop.object_value().unwrap()
        } else {
            mcp.append("servers", jsonc_parser::json!({}));
            mcp.get("servers").unwrap().object_value().unwrap()
        };
        servers.append(
            "rag",
            jsonc_parser::cst::CstInputValue::Object(vec![
                ("type".into(), "local".into()),
                (
                    "command".into(),
                    vec!["node", ".opencode/rag-cli.mjs", "mcp"].into(),
                ),
            ]),
        );
        root.to_string()
    } else {
        format!("{}\n", serde_json::to_string_pretty(doc)?)
    };
    fs::write(&staged, text)?;
    if original.is_some() {
        fs::set_permissions(&staged, fs::metadata(config)?.permissions())?;
    }
    let write = if original.is_some() {
        fs::rename(&staged, config)
    } else {
        fs::hard_link(&staged, config)
    };
    let _ = fs::remove_file(staged);
    write?;
    Ok(())
}

pub fn setup(
    project: &Path,
    base_url: &str,
    dry_run: bool,
    mut run: impl FnMut(&str, Command) -> eyre::Result<()>,
) -> eyre::Result<String> {
    let (paths, config, original, doc) = setup_files(project, base_url, dry_run)?;
    if dry_run {
        return Ok(paths);
    }
    let mut npm = Command::new("npm");
    npm.args(["install", "--registry=https://registry.npmjs.org"])
        .current_dir(project.join(".opencode"));
    run("Installing RAG dependencies (npm install)", npm)
        .wrap_err("RAG setup files remain; npm is required")?;
    let mut node = Command::new("node");
    node.args([".opencode/rag-cli.mjs", "index"])
        .current_dir(project);
    run("Indexing project (node .opencode/rag-cli.mjs index)", node)
        .wrap_err("RAG setup files remain; retry with node .opencode/rag-cli.mjs index")?;
    activate_config(&config, original.as_deref(), &doc)?;
    Ok(format!("RAG indexed. Project files:\n{paths}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_legacy_rag_helper_is_backed_up_without_touching_the_index() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-helper-upgrade"));
        fs::create_dir(&root).unwrap();
        setup_files(&root, "https://example.com", false).unwrap();
        let helper = root.join(".opencode/rag-cli.mjs");
        fs::write(&helper, LEGACY_CLI).unwrap();
        fs::create_dir(root.join(".opencode/rag_db")).unwrap();
        fs::write(root.join(".opencode/rag_db/index"), "preserve").unwrap();
        setup_files(&root, "https://example.com", true).unwrap();
        assert_eq!(fs::read_to_string(&helper).unwrap(), LEGACY_CLI);
        setup_files(&root, "https://example.com", false).unwrap();
        assert_eq!(fs::read_to_string(&helper).unwrap(), CLI);
        assert_eq!(
            fs::read_to_string(root.join(".opencode/rag_db/index")).unwrap(),
            "preserve"
        );
        assert!(
            fs::read_dir(root.join(".opencode"))
                .unwrap()
                .map(Result::unwrap)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rag-cli.mjs.backup-"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn earlier_rag_config_is_upgraded_to_skip_generated_folders() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-config-upgrade"));
        fs::create_dir(&root).unwrap();
        setup_files(&root, "https://example.com", false).unwrap();
        let config = root.join(".opencode/rag.json");
        let legacy = rag_config("https://example.com", &LEGACY_EXCLUDE_DIRS);
        fs::write(&config, &legacy).unwrap();
        fs::create_dir(root.join(".opencode/rag_db")).unwrap();
        fs::write(root.join(".opencode/rag_db/index"), "preserve").unwrap();
        setup_files(&root, "https://example.com", true).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), legacy);
        setup_files(&root, "https://example.com", false).unwrap();
        let upgraded: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(upgraded["indexing"]["excludeDirs"], json!(EXCLUDE_DIRS));
        assert_eq!(
            fs::read_to_string(root.join(".opencode/rag_db/index")).unwrap(),
            "preserve"
        );
        assert!(
            fs::read_dir(root.join(".opencode"))
                .unwrap()
                .map(Result::unwrap)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rag.json.backup-"))
        );
        fs::write(&config, "{}").unwrap();
        assert!(setup_files(&root, "https://example.com", false).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_index_can_be_reused_without_reindexing() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-reuse-test"));
        fs::create_dir(&root).unwrap();
        setup_files(&root, "https://example.com", false).unwrap();
        fs::create_dir_all(root.join(".opencode/rag_db")).unwrap();
        fs::create_dir_all(root.join(".opencode/node_modules/opencode-rag-plugin/dist/cli"))
            .unwrap();
        fs::write(
            root.join(".opencode/node_modules/opencode-rag-plugin/dist/cli/index.js"),
            "cli",
        )
        .unwrap();
        assert!(ready(&root, "https://example.com"));
        assert!(!ready(&root, "https://different.example.com"));
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let stack = crate::projects::Stack {
            id: "default".into(),
            name: "Global default".into(),
            url: "https://example.com".into(),
        };
        let project = crate::projects::Project {
            path: root.canonicalize().unwrap(),
            stack: "default".into(),
            rag: true,
        };
        crate::projects::write_project(&root, &project, &stack).unwrap();
        let config = crate::stack::read_config(&root.join(".opencode/opencode.json")).unwrap();
        assert_eq!(
            config["mcp"]["servers"]["rag"]["environment"]["OPENAI_API_KEY"],
            config["providers"]["palantir-openai"]["settings"]["apiKey"]
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn only_the_known_legacy_plugin_can_be_retired() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("legacy-rag-test"));
        fs::create_dir_all(root.join(".opencode/plugins")).unwrap();
        let path = root.join(".opencode/plugins/rag-plugin.js");
        fs::write(&path, LEGACY_PLUGIN).unwrap();
        assert_eq!(legacy_plugin(&root).unwrap(), Some(path.clone()));
        fs::write(&path, "custom plugin").unwrap();
        assert!(legacy_plugin(&root).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "custom plugin");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preview_is_read_only_and_preserves_conflicts() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-setup-test"));
        fs::create_dir(&root).unwrap();
        let preview = setup(&root, "https://example.com", true, |_, _| unreachable!()).unwrap();
        assert!(preview.contains(".opencode/rag.json"));
        assert!(!root.join(".opencode").exists());
        fs::create_dir(root.join(".opencode")).unwrap();
        fs::write(root.join(".opencode/rag.json"), "keep").unwrap();
        assert!(setup(&root, "https://example.com", true, |_, _| unreachable!()).is_err());
        assert_eq!(
            fs::read_to_string(root.join(".opencode/rag.json")).unwrap(),
            "keep"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_preserves_other_mcp_servers_and_refuses_reruns() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-setup-test"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(".opencode")).unwrap();
        fs::write(root.join(".opencode/opencode.json"), r#"{"model":"test/model","mcp":{"servers":{"other":{"type":"local","command":["other"]}}}}"#).unwrap();
        let (_, config, original, doc) = setup_files(&root, "https://example.com", false).unwrap();
        assert!(serde_json::from_str::<Value>(&fs::read_to_string(&config).unwrap()).unwrap()["mcp"]["servers"].get("rag").is_none());
        activate_config(&config, original.as_deref(), &doc).unwrap();
        let doc: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["model"], "test/model");
        assert_eq!(doc["mcp"]["servers"]["other"]["command"][0], "other");
        assert_eq!(doc["mcp"]["servers"]["rag"]["command"][2], "mcp");
        assert!(setup_files(&root, "https://example.com", false).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jsonc_config_keeps_comments_when_rag_is_added() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-jsonc-test"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(".opencode")).unwrap();
        let path = root.join(".opencode/opencode.jsonc");
        fs::write(
            &path,
            "{\n  // keep this note\n  \"model\": \"test/model\",\n}\n",
        )
        .unwrap();
        let (_, config, original, doc) = setup_files(&root, "https://example.com", false).unwrap();
        assert_eq!(config, path);
        activate_config(&config, original.as_deref(), &doc).unwrap();
        let result = fs::read_to_string(&path).unwrap();
        assert!(result.contains("// keep this note"));
        assert!(result.contains("rag-cli.mjs"));
        assert!(!root.join(".opencode/opencode.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retries_verified_partial_setup_without_discarding_index() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-retry-test"));
        fs::create_dir(&root).unwrap();
        setup_files(&root, "https://example.com", false).unwrap();
        let directory = root.join(".opencode");
        fs::create_dir(directory.join("node_modules")).unwrap();
        fs::create_dir(directory.join("rag_db")).unwrap();
        fs::write(directory.join("rag_db/existing"), "keep").unwrap();
        let lock = json!({"packages": {"": {"dependencies": serde_json::from_str::<Value>(PACKAGE).unwrap()["dependencies"]}}});
        fs::write(directory.join("package-lock.json"), lock.to_string()).unwrap();
        fs::remove_file(directory.join("rag-cli.mjs")).unwrap();
        let preview = setup(&root, "https://example.com", true, |_, _| unreachable!()).unwrap();
        assert!(preview.contains("reuse matching"));
        assert!(!directory.join("rag-cli.mjs").exists());
        assert!(!directory.join("opencode.json").exists());
        let mut commands = Vec::new();
        assert!(
            setup(&root, "https://example.com", false, |label, _| {
                commands.push(label.to_string());
                if commands.len() == 2 {
                    eyre::bail!("index unavailable")
                }
                Ok(())
            })
            .is_err()
        );
        assert!(!directory.join("opencode.json").exists());
        assert_eq!(
            fs::read_to_string(directory.join("rag-cli.mjs")).unwrap(),
            CLI
        );
        assert_eq!(
            fs::read_to_string(directory.join("rag_db/existing")).unwrap(),
            "keep"
        );
        commands.clear();
        setup(&root, "https://example.com", false, |label, _| {
            commands.push(label.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            fs::read_to_string(directory.join("rag_db/existing")).unwrap(),
            "keep"
        );
        assert!(
            fs::read_to_string(directory.join("opencode.json"))
                .unwrap()
                .contains("rag-cli.mjs")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_rejects_unrelated_or_modified_artifacts() {
        let root = crate::ops::backup_path(&std::env::temp_dir().join("rag-conflict-test"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(".opencode")).unwrap();
        let directory = root.join(".opencode");
        let lock = directory.join("package-lock.json");
        fs::write(&lock, "not ours").unwrap();
        assert!(setup_files(&root, "https://example.com", false).is_err());
        assert!(!directory.join("package.json").exists());
        fs::remove_file(&lock).unwrap();
        setup_files(&root, "https://example.com", false).unwrap();
        fs::write(&lock, r#"{"packages":{"":{"dependencies":{"other":"1"}}}}"#).unwrap();
        assert!(setup_files(&root, "https://example.com", false).is_err());
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            r#"{"packages":{"":{"dependencies":{"other":"1"}}}}"#
        );
        fs::remove_file(&lock).unwrap();
        fs::write(directory.join("rag.json"), "custom").unwrap();
        assert!(setup_files(&root, "https://example.com", true).is_err());
        assert_eq!(
            fs::read_to_string(directory.join("rag.json")).unwrap(),
            "custom"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
