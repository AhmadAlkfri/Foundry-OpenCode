//! Git worktrees share their main checkout's `.opencode` configuration through links.
//! Only the main checkout is a configured project; each worktree keeps its own RAG index.

use std::fs::{self, File};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use color_eyre::eyre::{self, Context};

/// The global Git alias this installer can manage.
pub const ALIAS: &str = "!foundry-opencode sync-worktrees";
const MARK: &str = "# foundry-opencode: worktrees share the main checkout's .opencode (local only)";

pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    bare: bool,
}

/// Entries that stay per worktree: the RAG index, logs, runtime state and backups.
fn per_worktree(name: &str) -> bool {
    generated(name) || name.contains(".backup-")
}

/// Per-worktree entries an uninstall may delete; config backups are kept for recovery.
fn generated(name: &str) -> bool {
    name.starts_with("rag_db") || name.ends_with(".log") || name == "runtime-overrides.json"
}

fn git(dir: &Path, args: &[&str]) -> eyre::Result<String> {
    // Git exports these to aliases; they would override the directory each call targets.
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX")
        .output()
        .wrap_err("git is required")?;
    eyre::ensure!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

fn absolute(dir: &Path, which: &str) -> eyre::Result<PathBuf> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--path-format=absolute", which])?).canonicalize()?)
}

/// Every checkout of the repository containing `dir`; Git lists the main checkout first.
pub fn list(dir: &Path) -> eyre::Result<Vec<Worktree>> {
    let mut list: Vec<Worktree> = Vec::new();
    for line in git(dir, &["worktree", "list", "--porcelain"])?.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            list.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                bare: false,
            });
        } else if let Some(last) = list.last_mut() {
            if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                last.branch = Some(branch.to_owned());
            } else if line == "bare" {
                last.bare = true;
            }
        }
    }
    Ok(list)
}

/// The main checkout when `dir` is a linked worktree, or `None` for a main checkout.
pub fn main_of(dir: &Path) -> eyre::Result<Option<PathBuf>> {
    if absolute(dir, "--git-dir")? == absolute(dir, "--git-common-dir")? {
        return Ok(None);
    }
    Ok(list(dir)?.into_iter().next().map(|main| main.path))
}

/// Links each shared entry of `main/.opencode` into `worktree/.opencode`. Nothing existing is
/// replaced, and a worktree whose `.opencode` is itself a link or a file is left alone.
pub fn link(main: &Path, worktree: &Path) -> eyre::Result<usize> {
    let source = main.join(".opencode");
    if !fs::symlink_metadata(&source)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
    {
        return Ok(0);
    }
    let target = worktree.join(".opencode");
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&target)?,
        Err(error) => return Err(error.into()),
    }
    let mut names = fs::read_dir(&source)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    let mut linked = 0;
    for name in names {
        let path = target.join(&name);
        if per_worktree(&name.to_string_lossy()) || fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(source.join(&name), &path)
            .wrap_err_with(|| format!("Could not link {}", path.display()))?;
        linked += 1;
    }
    Ok(linked)
}

/// Removes the links `link` made and each worktree's own index and logs, then the folder if empty.
pub fn unlink(main: &Path, worktree: &Path) -> eyre::Result<()> {
    let source = main.join(".opencode");
    let dir = worktree.join(".opencode");
    if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
    {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            if fs::read_link(&path)?.starts_with(&source) {
                fs::remove_file(&path)?;
            }
        } else if generated(&path.file_name().unwrap().to_string_lossy()) {
            if meta.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    if fs::read_dir(&dir)?.next().is_none() {
        fs::remove_dir(&dir)?;
    }
    Ok(())
}

/// Linked worktrees of a configured main checkout that hold this installer's links.
pub fn linked(main: &Path) -> Vec<PathBuf> {
    let source = main.join(".opencode");
    list(main)
        .unwrap_or_default()
        .into_iter()
        .skip(1)
        .map(|worktree| worktree.path)
        .filter(|path| {
            fs::read_dir(path.join(".opencode")).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    fs::read_link(entry.path()).is_ok_and(|target| target.starts_with(&source))
                })
            })
        })
        .collect()
}

fn exclude_file(repo: &Path) -> eyre::Result<PathBuf> {
    let path = absolute(repo, "--git-common-dir")?.join("info/exclude");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        eyre::ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Unsafe {}",
            path.display()
        );
    }
    Ok(path)
}

/// Hides `.opencode` in every worktree and branch through the repository's local, never-pushed
/// exclude file. Repositories that track files under `.opencode` keep their own rules.
pub fn exclude(repo: &Path) -> eyre::Result<bool> {
    if !git(repo, &["ls-files", "--", ".opencode"])?.is_empty() {
        return Ok(false);
    }
    let path = exclude_file(repo)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if text.lines().any(|line| line == MARK) {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let separator = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&path, format!("{text}{separator}{MARK}\n.opencode\n"))?;
    Ok(true)
}

/// Removes only the entry `exclude` added.
pub fn unexclude(repo: &Path) -> eyre::Result<()> {
    let path = exclude_file(repo)?;
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let mut kept = String::new();
    let mut changed = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line == MARK {
            changed = true;
            lines.next_if_eq(&".opencode");
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if changed {
        fs::write(&path, kept)?;
    }
    Ok(())
}

/// Prepares a configured main checkout for worktrees: hides `.opencode` and links every worktree.
pub fn share(repo: &Path) -> eyre::Result<Vec<String>> {
    let mut messages = Vec::new();
    if exclude(repo)? {
        messages.push(format!(
            "Hid .opencode in all worktrees of {} (.git/info/exclude)",
            repo.display()
        ));
    }
    for worktree in list(repo)?
        .into_iter()
        .skip(1)
        .filter(|item| !item.bare && item.path.is_dir())
    {
        let count = link(repo, &worktree.path)?;
        if count > 0 {
            messages.push(format!(
                "Linked {count} .opencode entries into worktree {}",
                worktree.path.display()
            ));
        }
    }
    Ok(messages)
}

fn paint(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// `foundry-opencode sync-worktrees`: moves every worktree of the current repository to the
/// latest default branch, links the shared configuration and refreshes each RAG index.
/// Worktrees on another branch or with uncommitted work are reported and not moved.
pub fn sync(dir: &Path, dry_run: bool) -> eyre::Result<bool> {
    if !dry_run {
        git(dir, &["fetch", "-q", "origin"]).wrap_err("git fetch origin failed")?;
    }
    let upstream = git(
        dir,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    )
    .unwrap_or_else(|_| "origin/master".into());
    let default = upstream
        .strip_prefix("origin/")
        .unwrap_or(&upstream)
        .to_owned();
    let target = git(dir, &["rev-parse", "--short", &upstream])?;
    let worktrees = list(dir)?;
    let main = worktrees
        .first()
        .map(|item| item.path.clone())
        .ok_or_else(|| eyre::eyre!("No worktrees found"))?;
    let would = if dry_run { "would move" } else { "->" };
    let mut ok = true;
    for worktree in worktrees.iter().filter(|item| !item.bare) {
        let path = &worktree.path;
        let shown = path.display();
        if !path.is_dir() {
            println!(
                "{}",
                paint(
                    "1;31",
                    &format!("FAILED  {shown} is missing; run git worktree prune")
                )
            );
            ok = false;
            continue;
        }
        let moved = match &worktree.branch {
            Some(branch) if branch != &default => Err(format!("is on branch {branch}. Not moved.")),
            branch => match git(
                path,
                &["status", "--porcelain", "--", ".", ":(exclude).opencode"],
            ) {
                Err(error) => Err(format!("could not be checked: {error}")),
                Ok(changes) if !changes.is_empty() => {
                    Err("has uncommitted changes. Not moved.".into())
                }
                Ok(_) if dry_run => Ok(()),
                Ok(_) if branch.is_some() => git(path, &["merge", "-q", "--ff-only", &upstream])
                    .map(drop)
                    .map_err(|_| {
                        format!("{default} has commits that are not on {upstream}. Not moved.")
                    }),
                Ok(_) => git(path, &["checkout", "-q", "--detach", &upstream])
                    .map(drop)
                    .map_err(|error| format!("could not be moved: {error}")),
            },
        };
        match moved {
            Ok(()) => println!("{}      {shown} {would} {target}", paint("32", "ok")),
            Err(reason) => {
                println!("{}", paint("1;31", &format!("FAILED  {shown} {reason}")));
                ok = false;
            }
        }
        if path != &main && !dry_run {
            match link(&main, path) {
                Ok(0) => {}
                Ok(count) => println!(
                    "        {}",
                    paint(
                        "2",
                        &format!("linked {count} .opencode entries from {}", main.display())
                    )
                ),
                Err(error) => {
                    println!(
                        "        {}",
                        paint("1;31", &format!("link FAILED: {error}"))
                    );
                    ok = false;
                }
            }
        }
        let has_rag = path.join(".opencode/rag-cli.mjs").is_file()
            || path != &main && main.join(".opencode/rag-cli.mjs").is_file();
        if !has_rag {
            println!(
                "        {}",
                paint("2", "rag: skipped (no .opencode/rag-cli.mjs)")
            );
        } else if dry_run {
            println!("        rag: would index");
        } else {
            let kind = if path.join(".opencode/rag_db").is_dir() {
                "updated"
            } else {
                "built (first run)"
            };
            let log = path.join(".opencode/sync-index.log");
            let indexed = File::create(&log).and_then(|out| {
                let err = out.try_clone()?;
                Command::new("node")
                    .args([".opencode/rag-cli.mjs", "index"])
                    .current_dir(path)
                    .stdin(Stdio::null())
                    .stdout(out)
                    .stderr(err)
                    .status()
            });
            match indexed {
                Ok(status) if status.success() => println!("        rag: {}", paint("32", kind)),
                _ => {
                    println!(
                        "        rag: {}",
                        paint("1;31", &format!("FAILED, see {}", log.display()))
                    );
                    ok = false;
                }
            }
        }
    }
    if !ok {
        println!(
            "{}",
            paint(
                "1;31",
                "Some worktrees were NOT fully updated. See FAILED lines above."
            )
        );
    }
    Ok(ok)
}

fn config(file: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.arg("config");
    match file {
        Some(file) => command.arg("--file").arg(file),
        None => command.arg("--global"),
    };
    command
}

/// The current `git sync-worktrees` alias. `file` replaces the global Git config in tests.
pub fn alias(file: Option<&Path>) -> eyre::Result<Option<String>> {
    let output = config(file)
        .args(["--get", "alias.sync-worktrees"])
        .output()?;
    Ok(output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }))
}

/// Review text for an install/update that sets the alias choice, or `None` when nothing changes.
pub fn alias_plan(install: bool, file: Option<&Path>) -> eyre::Result<Option<String>> {
    Ok(match (install, alias(file)?) {
        (true, Some(current)) if current == ALIAS => None,
        (true, Some(current)) => Some(format!(
            "Replace your Git alias sync-worktrees ({current}) with: git sync-worktrees => foundry-opencode sync-worktrees"
        )),
        (true, None) => {
            Some("Install Git alias: git sync-worktrees => foundry-opencode sync-worktrees".into())
        }
        (false, Some(current)) if current == ALIAS => {
            Some("Remove Git alias sync-worktrees".into())
        }
        (false, _) => None,
    })
}

/// Installs the alias, or removes it when unselected; another alias is only removed if it is this
/// installer's.
pub fn set_alias(install: bool, file: Option<&Path>) -> eyre::Result<Option<String>> {
    let plan = alias_plan(install, file)?;
    if plan.is_some() {
        let mut command = config(file);
        if install {
            command.args(["alias.sync-worktrees", ALIAS]);
        } else {
            command.args(["--unset", "alias.sync-worktrees"]);
        }
        eyre::ensure!(
            command.status()?.success(),
            "Could not update the Git alias sync-worktrees"
        );
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> PathBuf {
        let path =
            crate::ops::backup_path(&std::env::temp_dir().join("foundry-opencode-worktrees"));
        fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    /// A main checkout with an origin and a configured `.opencode`.
    fn repository(root: &Path) -> PathBuf {
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        run(&main, &["init", "-q"]);
        fs::write(main.join("file"), "one\n").unwrap();
        run(&main, &["add", "file"]);
        run(&main, &["commit", "-qm", "one"]);
        run(root, &["init", "-q", "--bare", "origin.git"]);
        run(&main, &["remote", "add", "origin", "../origin.git"]);
        run(&main, &["push", "-q", "-u", "origin", "main"]);
        run(&main, &["remote", "set-head", "origin", "main"]);
        let open = main.join(".opencode");
        fs::create_dir_all(open.join("node_modules")).unwrap();
        fs::create_dir(open.join("rag_db")).unwrap();
        for name in [
            "opencode.json",
            "rag.json",
            ".gitignore",
            "opencode-rag.log",
            "opencode.json.backup-1",
            "runtime-overrides.json",
        ] {
            fs::write(open.join(name), name).unwrap();
        }
        main
    }

    fn fake_rag(main: &Path) {
        fs::write(
            main.join(".opencode/rag-cli.mjs"),
            "import fs from 'node:fs';\nfs.mkdirSync('.opencode/rag_db', { recursive: true });\nfs.writeFileSync('.opencode/rag_db/from', process.cwd());\nif (process.cwd().endsWith('broken')) process.exit(1);\n",
        )
        .unwrap();
    }

    #[test]
    fn worktrees_are_told_apart_from_their_main_checkout() {
        let root = sandbox();
        let main = repository(&root);
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        assert!(main_of(&main).unwrap().is_none());
        assert_eq!(main_of(&root.join("linked")).unwrap().unwrap(), main);
        let listed = list(&root.join("linked")).unwrap();
        assert_eq!(listed[0].path, main);
        assert_eq!(listed[0].branch.as_deref(), Some("main"));
        assert!(listed[1].branch.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn links_share_config_but_keep_index_logs_and_backups_per_worktree() {
        let root = sandbox();
        let main = repository(&root);
        let worktree = root.join("linked");
        fs::create_dir_all(worktree.join(".opencode")).unwrap();
        fs::write(worktree.join(".opencode/rag.json"), "mine").unwrap();
        assert_eq!(link(&main, &worktree).unwrap(), 3);
        let mut names: Vec<_> = fs::read_dir(worktree.join(".opencode"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [".gitignore", "node_modules", "opencode.json", "rag.json"]
        );
        assert_eq!(
            fs::read_link(worktree.join(".opencode/opencode.json")).unwrap(),
            main.join(".opencode/opencode.json")
        );
        assert_eq!(
            fs::read_to_string(worktree.join(".opencode/rag.json")).unwrap(),
            "mine"
        );
        assert_eq!(link(&main, &worktree).unwrap(), 0);
        fs::write(main.join(".opencode/added.json"), "{}").unwrap();
        assert_eq!(link(&main, &worktree).unwrap(), 1);

        let elsewhere = root.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let custom = root.join("custom");
        fs::create_dir(&custom).unwrap();
        std::os::unix::fs::symlink(&elsewhere, custom.join(".opencode")).unwrap();
        assert_eq!(link(&main, &custom).unwrap(), 0);
        assert!(fs::read_dir(&elsewhere).unwrap().next().is_none());

        fs::create_dir(worktree.join(".opencode/rag_db")).unwrap();
        fs::write(worktree.join(".opencode/sync-index.log"), "log").unwrap();
        fs::write(worktree.join(".opencode/opencode.json.backup-2"), "keep").unwrap();
        unlink(&main, &worktree).unwrap();
        let mut left: Vec<_> = fs::read_dir(worktree.join(".opencode"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["opencode.json.backup-2", "rag.json"]);
        assert!(main.join(".opencode/opencode.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn exclude_hides_opencode_for_every_worktree_and_is_removed_cleanly() {
        let root = sandbox();
        let main = repository(&root);
        let file = main.join(".git/info/exclude");
        fs::write(&file, "keep").unwrap();
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        assert!(exclude(&main).unwrap());
        assert!(!exclude(&root.join("linked")).unwrap());
        assert_eq!(run(&main, &["status", "--porcelain"]), "");
        link(&main, &root.join("linked")).unwrap();
        assert_eq!(run(&root.join("linked"), &["status", "--porcelain"]), "");
        assert_eq!(linked(&main), [root.join("linked")]);
        unexclude(&main).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "keep\n");

        let tracked = root.join("tracked");
        fs::create_dir_all(tracked.join(".opencode")).unwrap();
        run(&tracked, &["init", "-q"]);
        fs::write(tracked.join(".opencode/agent.md"), "shared").unwrap();
        run(&tracked, &["add", ".opencode/agent.md"]);
        assert!(!exclude(&tracked).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_moves_clean_worktrees_flags_the_rest_and_indexes_each_checkout() {
        let root = sandbox();
        let main = repository(&root);
        fake_rag(&main);
        exclude(&main).unwrap();
        let first = run(&main, &["rev-parse", "HEAD"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../clean"]);
        run(
            &main,
            &["worktree", "add", "-q", "-b", "feature", "../feature"],
        );
        run(&main, &["worktree", "add", "-q", "--detach", "../dirty"]);
        fs::write(root.join("dirty/file"), "edited\n").unwrap();
        run(&main, &["worktree", "add", "-q", "--detach", "../broken"]);
        run(&root, &["clone", "-q", "origin.git", "other"]);
        fs::write(root.join("other/file"), "two\n").unwrap();
        run(&root.join("other"), &["commit", "-qam", "two"]);
        run(&root.join("other"), &["push", "-q"]);

        assert!(!sync(&root.join("feature"), true).unwrap());
        assert_eq!(run(&root.join("clean"), &["rev-parse", "HEAD"]), first);
        assert!(!root.join("clean/.opencode").exists());

        assert!(!sync(&root.join("feature"), false).unwrap());
        let latest = run(&main, &["rev-parse", "origin/main"]);
        for name in ["main", "clean"] {
            assert_eq!(run(&root.join(name), &["rev-parse", "HEAD"]), latest);
            assert_eq!(run(&root.join(name), &["status", "--porcelain"]), "");
        }
        assert_eq!(run(&root.join("dirty"), &["rev-parse", "HEAD"]), first);
        assert_eq!(
            run(
                &root.join("feature"),
                &["rev-parse", "--abbrev-ref", "HEAD"]
            ),
            "feature"
        );
        for name in ["main", "clean", "feature", "dirty"] {
            assert_eq!(
                fs::read_to_string(root.join(name).join(".opencode/rag_db/from")).unwrap(),
                root.join(name).display().to_string()
            );
        }
        assert!(fs::read_link(root.join("clean/.opencode/rag-cli.mjs")).is_ok());
        fs::remove_dir_all(root.join("broken")).unwrap();
        run(&main, &["worktree", "prune"]);
        run(&main, &["worktree", "remove", "--force", "../dirty"]);
        run(&main, &["worktree", "remove", "--force", "../feature"]);
        assert!(sync(&main, false).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn alias_is_installed_on_request_and_only_this_installers_alias_is_removed() {
        let root = sandbox();
        let file = root.join("gitconfig");
        assert_eq!(alias_plan(false, Some(&file)).unwrap(), None);
        assert!(
            set_alias(true, Some(&file))
                .unwrap()
                .unwrap()
                .starts_with("Install")
        );
        assert_eq!(alias(Some(&file)).unwrap().as_deref(), Some(ALIAS));
        assert_eq!(set_alias(true, Some(&file)).unwrap(), None);
        assert!(set_alias(false, Some(&file)).unwrap().is_some());
        assert_eq!(alias(Some(&file)).unwrap(), None);
        config(Some(&file))
            .args(["alias.sync-worktrees", "!echo mine"])
            .status()
            .unwrap();
        assert_eq!(set_alias(false, Some(&file)).unwrap(), None);
        assert_eq!(alias(Some(&file)).unwrap().as_deref(), Some("!echo mine"));
        assert!(
            alias_plan(true, Some(&file))
                .unwrap()
                .unwrap()
                .contains("!echo mine")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
