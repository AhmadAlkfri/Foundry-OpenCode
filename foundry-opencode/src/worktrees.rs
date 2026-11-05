//! Git worktrees share their main checkout's `.opencode` configuration through links.
//! Only the main checkout is a configured project; each worktree keeps its own RAG index.

use std::fs::{self, File};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use color_eyre::eyre::{self, Context};

/// The global Git alias this installer can manage.
pub const ALIAS: &str = "!foundry-opencode sync-worktrees";
const MARK: &str = "# foundry-opencode: worktrees share the main checkout's .opencode (local only)";
const GITIGNORE_MARK: &str =
    "# foundry-opencode: local OpenCode and OpenChamber setup, never committed";
/// Folders that hold one machine's OpenCode/OpenChamber setup and must never reach a remote.
const LOCAL: [&str; 2] = [".opencode", ".openchamber"];
const HELPER: &str = ".opencode/rag-cli.mjs";
/// The RAG plugin version whose index layout `SEED_SCRIPT` knows; others get a full index instead.
const RAG_PLUGIN_VERSION: &str = "1.22.1";
/// Re-points a copy of the main checkout's RAG index at a worktree. Only the manifest, the keyword
/// index and the vector table's `filePath` column hold absolute paths; the embedded text uses
/// checkout-relative paths, so every embedding stays valid. Runs in the worktree's `.opencode`,
/// whose linked `node_modules` provide LanceDB.
const SEED_SCRIPT: &str = r#"
import fs from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
const { SEED_FROM: from, SEED_TO: to, SEED_DB: db } = process.env;
const inJson = (text) => JSON.stringify(text).slice(1, -1);
for (const name of ["manifest.json", "keyword-index.json"]) {
  const file = `${db}/${name}`;
  if (!fs.existsSync(file)) continue;
  const text = fs.readFileSync(file, "utf8").split(inJson(from)).join(inJson(to));
  if (name === "manifest.json") {
    // Git-based change detection would trust the main checkout's commit, missing its uncommitted
    // edits; without it the next pass compares every file by hash once.
    const manifest = JSON.parse(text);
    delete manifest.lastGitCommit;
    fs.writeFileSync(file, JSON.stringify(manifest, null, 2));
  } else {
    fs.writeFileSync(file, text);
  }
}
const require = createRequire(`${process.cwd()}/`);
const lancedb = await import(pathToFileURL(require.resolve("@lancedb/lancedb")).href);
const table = await (await lancedb.connect(db)).openTable("chunks");
const sql = (text) => `'${text.replaceAll("'", "''")}'`;
const rows = await table.countRows();
await table.update({
  where: `starts_with(filePath, ${sql(from)})`,
  valuesSql: { filePath: `replace(filePath, ${sql(from)}, ${sql(to)})` },
});
const left = await table.countRows(`starts_with(filePath, ${sql(from)})`);
if (left !== 0 || (await table.countRows()) !== rows) {
  throw new Error(`${left} rows still point at ${from}`);
}
"#;

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

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // Git exports these to aliases; they would override the directory each call targets.
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX");
    command
}

fn git(dir: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = git_command(dir)
        .args(args)
        .stdin(Stdio::null())
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
    // Git prints these relative to `dir`; `--path-format=absolute` would need Git 2.31.
    Ok(dir.join(git(dir, &["rev-parse", which])?).canonicalize()?)
}

/// Every checkout of the repository containing `dir`; Git lists the main checkout first.
pub fn list(dir: &Path) -> eyre::Result<Vec<Worktree>> {
    let mut list: Vec<Worktree> = Vec::new();
    for line in git(dir, &["worktree", "list", "--porcelain"])?.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path);
            list.push(Worktree {
                path: path.canonicalize().unwrap_or(path),
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

/// Whether `link`, inside a worktree's `.opencode`, was made by this installer for `source`,
/// including one left dangling because the main checkout has since moved.
fn ours(link: &Path, source: &Path) -> bool {
    let Ok(target) = fs::read_link(link) else {
        return false;
    };
    target.starts_with(source)
        || !target.exists()
            && target.file_name() == link.file_name()
            && target.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".opencode"))
}

/// Whether `link` may create links in this worktree: its `.opencode` is missing or a real folder.
fn linkable(worktree: &Path) -> bool {
    fs::symlink_metadata(worktree.join(".opencode"))
        .map_or(true, |meta| meta.is_dir() && !meta.file_type().is_symlink())
}

/// Links each shared entry of `main/.opencode` into `worktree/.opencode`. Nothing existing is
/// replaced, except this installer's links left dangling by a moved main checkout. A worktree
/// whose `.opencode` is itself a link or a file is left alone.
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
        if per_worktree(&name.to_string_lossy()) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) if ours(&path, &source) && !path.exists() => fs::remove_file(&path)?,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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
            if ours(&path, &source) {
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
            fs::read_dir(path.join(".opencode"))
                .is_ok_and(|entries| entries.flatten().any(|entry| ours(&entry.path(), &source)))
        })
        .collect()
}

fn exclude_file(repo: &Path) -> eyre::Result<PathBuf> {
    Ok(absolute(repo, "--git-common-dir")?.join("info/exclude"))
}

/// The text of a regular file, or "" when it does not exist yet. Refuses links and folders.
fn read_plain(path: &Path) -> eyre::Result<String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            eyre::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "Unsafe {}",
                path.display()
            );
            Ok(fs::read_to_string(path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn append(path: &Path, text: &str, mark: &str, names: &[&str]) -> eyre::Result<()> {
    let separator = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(
        path,
        format!("{text}{separator}{mark}\n{}\n", names.join("\n")),
    )?;
    Ok(())
}

/// Adds the local folders to the repository's never-pushed `.git/info/exclude`, which applies to
/// every worktree and branch at once.
fn exclude(repo: &Path) -> eyre::Result<()> {
    let path = exclude_file(repo)?;
    let text = read_plain(&path)?;
    if !text.lines().any(|line| line == MARK) {
        fs::create_dir_all(path.parent().unwrap())?;
        append(&path, &text, MARK, &LOCAL)?;
    }
    Ok(())
}

/// Whether `.gitignore` already ignores `name` at the root (`.opencode`, `/.opencode/`, ...).
fn listed(text: &str, name: &str) -> bool {
    text.lines()
        .map(|line| line.trim().trim_start_matches('/').trim_end_matches('/'))
        .any(|line| line == name)
}

/// Keeps `.opencode` and `.openchamber` out of Git: in the repository's `.gitignore`, so every
/// clone ignores them, and in `.git/info/exclude`, which covers every worktree and branch before
/// that change is committed. Files already committed there are reported, never removed.
pub fn ignore(repo: &Path) -> eyre::Result<Vec<String>> {
    let mut messages = Vec::new();
    let gitignore = repo.join(".gitignore");
    let text = read_plain(&gitignore)?;
    let missing: Vec<&str> = LOCAL
        .into_iter()
        .filter(|name| !listed(&text, name))
        .collect();
    if !missing.is_empty() {
        append(&gitignore, &text, GITIGNORE_MARK, &missing)?;
        messages.push(format!(
            "Added {} to {}; commit this change",
            missing.join(" and "),
            gitignore.display()
        ));
    }
    exclude(repo)?;
    let tracked = git(repo, &["ls-files", "--", ".opencode", ".openchamber"])?;
    if !tracked.is_empty() {
        let files: Vec<&str> = tracked.lines().collect();
        messages.push(format!(
            "{} commits machine-specific files under .opencode/.openchamber ({}{}). Remove them from the repository with: git rm -r --cached .opencode .openchamber, then commit",
            repo.display(),
            files[..files.len().min(3)].join(", "),
            if files.len() > 3 {
                format!(" and {} more", files.len() - 3)
            } else {
                String::new()
            }
        ));
    }
    Ok(messages)
}

/// Whether this installer's entry is in the repository's `.git/info/exclude`.
pub fn excluded(repo: &Path) -> bool {
    exclude_file(repo)
        .and_then(|path| read_plain(&path))
        .is_ok_and(|text| text.lines().any(|line| line == MARK))
}

/// Removes only the `.git/info/exclude` entry this installer added. `.gitignore` lines are kept.
pub fn unexclude(repo: &Path) -> eyre::Result<()> {
    let path = exclude_file(repo)?;
    let text = read_plain(&path)?;
    let mut kept = String::new();
    let mut changed = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line == MARK {
            changed = true;
            while lines.next_if(|line| LOCAL.contains(line)).is_some() {}
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

/// Prepares a configured main checkout for worktrees: keeps the local folders out of Git and links
/// every worktree.
pub fn share(repo: &Path) -> eyre::Result<Vec<String>> {
    let mut messages = ignore(repo)?;
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

/// The remote to sync from: the one the main checkout's branch tracks, else the only remote, else
/// `origin`.
fn remote(main: &Path, branch: Option<&str>) -> eyre::Result<String> {
    if let Some(branch) = branch
        && let Ok(remote) = git(main, &["config", &format!("branch.{branch}.remote")])
        && remote != "."
    {
        return Ok(remote);
    }
    let names = git(main, &["remote"])?;
    let names: Vec<&str> = names.lines().collect();
    match names.as_slice() {
        [only] => Ok((*only).to_owned()),
        _ if names.contains(&"origin") => Ok("origin".into()),
        [] => eyre::bail!("This repository has no remote to sync from"),
        _ => eyre::bail!(
            "Several remotes ({}) and the main checkout's branch tracks none; set one with git branch --set-upstream-to",
            names.join(", ")
        ),
    }
}

/// The remote's default branch: its remote HEAD, else what the remote reports, else main or master.
fn default_branch(main: &Path, remote: &str, online: bool) -> eyre::Result<String> {
    if let Ok(head) = git(
        main,
        &[
            "symbolic-ref",
            "-q",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    ) && let Some(branch) = head.strip_prefix(&format!("{remote}/"))
    {
        return Ok(branch.to_owned());
    }
    if online
        && let Ok(text) = git(main, &["ls-remote", "--symref", remote, "HEAD"])
        && let Some(branch) = text.lines().find_map(|line| {
            line.strip_prefix("ref: refs/heads/")?
                .strip_suffix("\tHEAD")
        })
    {
        return Ok(branch.to_owned());
    }
    for branch in ["main", "master"] {
        if git(
            main,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/remotes/{remote}/{branch}"),
            ],
        )
        .is_ok()
        {
            return Ok(branch.into());
        }
    }
    eyre::bail!(
        "Could not find the default branch of {remote}; run git remote set-head {remote} --auto"
    )
}

fn paint(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Copies the main checkout's RAG index into a worktree that has none and re-points it there.
fn seed(main: &Path, worktree: &Path) -> eyre::Result<()> {
    let source = main.join(".opencode/rag_db");
    let target = worktree.join(".opencode/rag_db");
    let package: serde_json::Value = serde_json::from_slice(
        &fs::read(main.join(".opencode/node_modules/opencode-rag-plugin/package.json"))
            .wrap_err("RAG plugin not found")?,
    )?;
    eyre::ensure!(
        package["version"] == RAG_PLUGIN_VERSION,
        "RAG plugin {} has an index layout this installer does not know",
        package["version"]
    );
    eyre::ensure!(
        !source.join("index.lock").exists(),
        "the main checkout is being indexed right now"
    );
    // An APFS clone (`cp -c`) is instant and shares disk blocks until either side changes.
    let copied = |flags: &str| {
        Command::new("cp")
            .arg(flags)
            .arg(&source)
            .arg(&target)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !copied("-cR") {
        let _ = fs::remove_dir_all(&target);
        eyre::ensure!(copied("-R"), "could not copy {}", source.display());
    }
    let repointed = (|| -> eyre::Result<()> {
        let mut child = Command::new("node")
            .args(["--input-type=module", "-"])
            .current_dir(worktree.join(".opencode"))
            .env("SEED_FROM", format!("{}/", main.display()))
            .env("SEED_TO", format!("{}/", worktree.display()))
            .env("SEED_DB", &target)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .wrap_err("node is required")?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(SEED_SCRIPT.as_bytes())?;
        let output = child.wait_with_output()?;
        eyre::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .unwrap_or("re-pointing failed")
        );
        Ok(())
    })();
    if repointed.is_err() {
        let _ = fs::remove_dir_all(&target);
    }
    repointed
}

/// (Re)indexes one checkout and prints the result. A worktree without an index starts from a copy
/// of the main checkout's, so only the files that differ are embedded. Returns false on failure.
fn index(main: &Path, path: &Path, dry_run: bool) -> bool {
    let has_rag = path.join(HELPER).is_file()
        || dry_run && path != main && linkable(path) && main.join(HELPER).is_file();
    if !has_rag {
        println!(
            "        {}",
            paint("2", "rag: skipped (no .opencode/rag-cli.mjs)")
        );
        return true;
    }
    let seedable = path != main
        && !path.join(".opencode/rag_db").exists()
        && main.join(".opencode/rag_db").is_dir();
    if dry_run {
        println!(
            "        rag: would {}",
            if seedable {
                "seed from the main checkout and index"
            } else {
                "index"
            }
        );
        return true;
    }
    let mut kind = if path.join(".opencode/rag_db").is_dir() {
        "updated"
    } else {
        "built (first run)"
    };
    if seedable {
        match seed(main, path) {
            Ok(()) => kind = "seeded from the main checkout and updated",
            Err(error) => println!(
                "        {}",
                paint(
                    "2",
                    &format!("rag: could not seed ({error}); building from scratch")
                )
            ),
        }
    }
    let log = path.join(".opencode/sync-index.log");
    let indexed = File::create(&log).and_then(|out| {
        let err = out.try_clone()?;
        Command::new("node")
            .args([HELPER, "index"])
            .current_dir(path)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .status()
    });
    if indexed.is_ok_and(|status| status.success()) {
        println!("        rag: {}", paint("32", kind));
        true
    } else {
        println!(
            "        rag: {}",
            paint("1;31", &format!("FAILED, see {}", log.display()))
        );
        false
    }
}

fn report_link(main: &Path, path: &Path) -> bool {
    match link(main, path) {
        Ok(0) => true,
        Ok(count) => {
            println!(
                "        {}",
                paint(
                    "2",
                    &format!("linked {count} .opencode entries from {}", main.display())
                )
            );
            true
        }
        Err(error) => {
            println!(
                "        {}",
                paint("1;31", &format!("link FAILED: {error}"))
            );
            false
        }
    }
}

/// `foundry-opencode sync-worktrees [--gen-rag]`: moves every worktree of the current repository to
/// the latest default branch and links the shared configuration; `gen_rag` also (re)indexes every
/// checkout. Worktrees on another branch or with uncommitted work are reported and not moved.
pub fn sync(dir: &Path, dry_run: bool, gen_rag: bool) -> eyre::Result<bool> {
    let worktrees = list(dir)?;
    let first = worktrees
        .first()
        .ok_or_else(|| eyre::eyre!("No worktrees found"))?;
    let main = first.path.clone();
    let remote = remote(&main, first.branch.as_deref())?;
    if !dry_run {
        git(&main, &["fetch", "-q", &remote])
            .wrap_err_with(|| format!("git fetch {remote} failed"))?;
    }
    let default = default_branch(&main, &remote, !dry_run)?;
    let upstream = format!("{remote}/{default}");
    let target = git(&main, &["rev-parse", "--short", &upstream])?;
    println!(
        "{}",
        paint("2", &format!("Syncing worktrees to {upstream} ({target})"))
    );
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
                &[
                    "status",
                    "--porcelain",
                    "--",
                    ".",
                    ":(exclude).opencode",
                    ":(exclude).openchamber",
                ],
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
            ok &= report_link(&main, path);
        }
        if gen_rag {
            ok &= index(&main, path, dry_run);
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

/// `foundry-opencode add-worktree [--gen-rag] <git worktree add options>`: runs `git worktree add`
/// as typed, then hides the local setup folders and links the shared configuration into the new
/// worktree; `gen_rag` also builds its RAG index.
pub fn add(dir: &Path, options: &[String], gen_rag: bool, dry_run: bool) -> eyre::Result<bool> {
    let before: Vec<PathBuf> = list(dir)?.into_iter().map(|item| item.path).collect();
    let main = before
        .first()
        .cloned()
        .ok_or_else(|| eyre::eyre!("No worktrees found"))?;
    if dry_run {
        println!("Would run: git worktree add {}", options.join(" "));
        println!(
            "Would then link .opencode from {}{}",
            main.display(),
            if gen_rag {
                " and seed and index its RAG"
            } else {
                ""
            }
        );
        return Ok(true);
    }
    let status = git_command(dir)
        .args(["worktree", "add"])
        .args(options)
        .status()
        .wrap_err("git is required")?;
    if !status.success() {
        return Ok(false);
    }
    let added = list(dir)?
        .into_iter()
        .map(|item| item.path)
        .find(|path| !before.contains(path))
        .ok_or_else(|| eyre::eyre!("git worktree add did not create a new worktree"))?;
    exclude(&main)?;
    println!("{}      {}", paint("32", "ok"), added.display());
    let mut ok = report_link(&main, &added);
    if gen_rag {
        ok &= index(&main, &added, false);
    }
    Ok(ok)
}

/// `foundry-opencode generate-rag [--all]`: (re)indexes the current checkout, or with `all` the
/// main checkout and every worktree. Nothing is moved.
pub fn generate(dir: &Path, all: bool, dry_run: bool) -> eyre::Result<bool> {
    let worktrees = list(dir)?;
    let main = worktrees
        .first()
        .map(|item| item.path.clone())
        .ok_or_else(|| eyre::eyre!("No worktrees found"))?;
    eyre::ensure!(
        main.join(HELPER).is_file(),
        "RAG is not set up for {}; run foundry-opencode init-rag there",
        main.display()
    );
    let targets: Vec<PathBuf> = if all {
        worktrees
            .iter()
            .filter(|item| !item.bare && item.path.is_dir())
            .map(|item| item.path.clone())
            .collect()
    } else {
        vec![absolute(dir, "--show-toplevel")?]
    };
    let mut ok = true;
    for path in &targets {
        println!("{}", path.display());
        if path != &main && !dry_run {
            ok &= report_link(&main, path);
        }
        ok &= index(&main, path, dry_run);
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
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
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

        let link_path = worktree.join(".opencode/opencode.json");
        fs::remove_file(&link_path).unwrap();
        std::os::unix::fs::symlink(root.join("moved-away/.opencode/opencode.json"), &link_path)
            .unwrap();
        assert_eq!(link(&main, &worktree).unwrap(), 1);
        assert_eq!(
            fs::read_link(&link_path).unwrap(),
            main.join(".opencode/opencode.json")
        );
        std::os::unix::fs::symlink(
            root.join("moved-away/.opencode/stale.json"),
            worktree.join(".opencode/stale.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), worktree.join(".opencode/notes")).unwrap();

        fs::create_dir(worktree.join(".opencode/rag_db")).unwrap();
        fs::write(worktree.join(".opencode/sync-index.log"), "log").unwrap();
        fs::write(worktree.join(".opencode/opencode.json.backup-2"), "keep").unwrap();
        unlink(&main, &worktree).unwrap();
        let mut left: Vec<_> = fs::read_dir(worktree.join(".opencode"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["notes", "opencode.json.backup-2", "rag.json"]);
        assert!(main.join(".opencode/opencode.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignore_keeps_local_folders_out_of_git_in_every_clone_and_worktree() {
        let root = sandbox();
        let main = repository(&root);
        let exclude_path = main.join(".git/info/exclude");
        fs::write(&exclude_path, "keep").unwrap();
        fs::write(main.join(".gitignore"), "/.opencode/\nnode_modules").unwrap();
        run(&main, &["add", ".gitignore"]);
        run(&main, &["commit", "-qm", "ignore"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        let messages = ignore(&main).unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].starts_with("Added .openchamber to"));
        assert_eq!(
            fs::read_to_string(main.join(".gitignore")).unwrap(),
            format!("/.opencode/\nnode_modules\n{GITIGNORE_MARK}\n.openchamber\n")
        );
        assert!(ignore(&main).unwrap().is_empty());
        assert!(excluded(&main));
        fs::create_dir(main.join(".openchamber")).unwrap();
        fs::write(main.join(".openchamber/state"), "local").unwrap();
        link(&main, &root.join("linked")).unwrap();
        assert_eq!(run(&main, &["status", "--porcelain"]), " M .gitignore");
        assert_eq!(run(&root.join("linked"), &["status", "--porcelain"]), "");
        assert_eq!(linked(&main), [root.join("linked")]);
        unexclude(&main).unwrap();
        assert_eq!(fs::read_to_string(&exclude_path).unwrap(), "keep\n");
        assert!(!excluded(&main));

        let tracked = root.join("tracked");
        fs::create_dir_all(tracked.join(".opencode")).unwrap();
        run(&tracked, &["init", "-q"]);
        fs::write(tracked.join(".opencode/agent.md"), "shared").unwrap();
        run(&tracked, &["add", ".opencode/agent.md"]);
        let messages = ignore(&tracked).unwrap();
        assert!(messages[0].starts_with("Added .opencode and .openchamber to"));
        assert!(
            messages[1].contains(".opencode/agent.md")
                && messages[1].contains("git rm -r --cached"),
            "{messages:?}"
        );
        assert_eq!(run(&tracked, &["ls-files"]), ".opencode/agent.md");
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
        run(&main, &["worktree", "add", "-q", "--detach", "../custom"]);
        fs::create_dir(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("custom/.opencode")).unwrap();
        run(&root, &["clone", "-q", "origin.git", "other"]);
        fs::write(root.join("other/file"), "two\n").unwrap();
        run(&root.join("other"), &["commit", "-qam", "two"]);
        run(&root.join("other"), &["push", "-q"]);

        assert!(!sync(&root.join("feature"), true, true).unwrap());
        assert_eq!(run(&root.join("clean"), &["rev-parse", "HEAD"]), first);
        assert!(!root.join("clean/.opencode").exists());

        assert!(!sync(&root.join("feature"), false, false).unwrap());
        assert!(fs::read_link(root.join("clean/.opencode/rag-cli.mjs")).is_ok());
        assert!(!root.join("clean/.opencode/rag_db").exists());
        assert!(!sync(&root.join("feature"), false, true).unwrap());
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
        assert!(sync(&main, false, true).unwrap());
        assert!(
            fs::read_dir(root.join("elsewhere"))
                .unwrap()
                .next()
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_uses_the_tracked_remote_whatever_its_name() {
        let root = sandbox();
        let main = repository(&root);
        exclude(&main).unwrap();
        run(&main, &["remote", "rename", "origin", "upstream"]);
        run(&main, &["remote", "set-head", "upstream", "-d"]);
        run(&main, &["remote", "add", "mirror", "../origin.git"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        run(&root, &["clone", "-q", "origin.git", "other"]);
        fs::write(root.join("other/file"), "two\n").unwrap();
        run(&root.join("other"), &["commit", "-qam", "two"]);
        run(&root.join("other"), &["push", "-q"]);
        assert_eq!(remote(&main, Some("main")).unwrap(), "upstream");
        assert!(sync(&root.join("linked"), false, false).unwrap());
        let latest = run(&main, &["rev-parse", "upstream/main"]);
        assert_eq!(run(&root.join("linked"), &["rev-parse", "HEAD"]), latest);
        assert_eq!(run(&main, &["rev-parse", "HEAD"]), latest);
        assert_eq!(default_branch(&main, "upstream", false).unwrap(), "main");
        run(&main, &["branch", "--unset-upstream"]);
        let error = remote(&main, Some("main")).unwrap_err().to_string();
        assert!(error.contains("Several remotes"), "{error}");
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
    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn add_worktree_passes_options_to_git_and_links_the_new_checkout() {
        let root = sandbox();
        let main = repository(&root);
        fake_rag(&main);
        assert!(add(&main, &strings(&["-b", "topic", "../topic"]), true, true).unwrap());
        assert!(!root.join("topic").exists());
        assert!(
            add(
                &main,
                &strings(&["-q", "-b", "topic", "../topic"]),
                false,
                false
            )
            .unwrap()
        );
        assert_eq!(
            run(&root.join("topic"), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "topic"
        );
        assert!(fs::read_link(root.join("topic/.opencode/opencode.json")).is_ok());
        assert!(!root.join("topic/.opencode/rag_db").exists());
        assert!(excluded(&main));
        assert_eq!(run(&root.join("topic"), &["status", "--porcelain"]), "");
        assert!(
            add(
                &main,
                &strings(&["-q", "--detach", "../indexed"]),
                true,
                false
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.join("indexed/.opencode/rag_db/from")).unwrap(),
            root.join("indexed").display().to_string()
        );
        assert!(
            !add(
                &main,
                &strings(&["-q", "--detach", "../topic"]),
                false,
                false
            )
            .unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generate_rag_indexes_this_checkout_or_every_checkout() {
        let root = sandbox();
        let main = repository(&root);
        run(&main, &["worktree", "add", "-q", "--detach", "../one"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../two"]);
        let error = generate(&root.join("one"), false, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("init-rag"), "{error}");
        fake_rag(&main);
        assert!(generate(&root.join("one"), false, true).unwrap());
        assert!(!root.join("one/.opencode").exists());
        assert!(generate(&root.join("one"), false, false).unwrap());
        assert!(root.join("one/.opencode/rag_db/from").is_file());
        assert!(!root.join("two/.opencode").exists());
        assert!(!main.join(".opencode/rag_db/from").exists());
        assert!(generate(&root.join("one"), true, false).unwrap());
        for name in ["main", "two"] {
            assert_eq!(
                fs::read_to_string(root.join(name).join(".opencode/rag_db/from")).unwrap(),
                root.join(name).display().to_string()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn node(dir: &Path, script: &str, env: &[(&str, &str)]) -> String {
        let mut child = Command::new("node")
            .args(["--input-type=module", "-"])
            .current_dir(dir)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    const LANCE: &str = r#"
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
const require = createRequire(`${process.cwd()}/`);
const lancedb = await import(pathToFileURL(require.resolve("@lancedb/lancedb")).href);
"#;

    /// Needs the RAG plugin's node_modules (LanceDB), e.g.
    /// `RAG_NODE_MODULES=~/Repos/K-OS/.opencode/node_modules cargo test -- --ignored seeding`.
    #[test]
    #[ignore = "requires RAG_NODE_MODULES with the pinned opencode-rag-plugin and LanceDB"]
    fn seeding_reuses_the_main_index_with_paths_pointing_at_the_worktree() {
        let modules = PathBuf::from(std::env::var("RAG_NODE_MODULES").unwrap());
        let root = sandbox();
        let main = repository(&root);
        let open = main.join(".opencode");
        fs::remove_dir_all(open.join("node_modules")).unwrap();
        fs::remove_dir_all(open.join("rag_db")).unwrap();
        std::os::unix::fs::symlink(&modules, open.join("node_modules")).unwrap();
        let from = format!("{}/", main.display());
        let main_db = open.join("rag_db").display().to_string();
        node(
            &open,
            &format!(
                r#"{LANCE}
import fs from "node:fs";
const {{ FROM: from, DB: db }} = process.env;
const rows = ["file", "dir/other.ts"].map((name, i) => ({{ id: `id-${{i}}`, filePath: `${{from}}${{name}}`, vector: [i, 1] }}));
await (await lancedb.connect(db)).createTable("chunks", rows);
fs.writeFileSync(`${{db}}/manifest.json`, JSON.stringify({{ lastGitCommit: "abc", files: {{ [`${{from}}file`]: {{ hash: "h", chunkCount: 1 }} }} }}));
fs.writeFileSync(`${{db}}/keyword-index.json`, JSON.stringify({{ chunkMap: {{ "id-0": `${{from}}file` }} }}));
"#
            ),
            &[("FROM", &from), ("DB", &main_db)],
        );
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        let worktree = root.join("linked");
        link(&main, &worktree).unwrap();
        seed(&main, &worktree).unwrap();
        let to = format!("{}/", worktree.display());
        let db = worktree.join(".opencode/rag_db");
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(db.join("manifest.json")).unwrap()).unwrap();
        assert!(manifest.get("lastGitCommit").is_none());
        assert!(manifest["files"].get(format!("{to}file")).is_some());
        let keywords = fs::read_to_string(db.join("keyword-index.json")).unwrap();
        assert!(keywords.contains(&format!("{to}file")) && !keywords.contains(&from));
        let list = format!(
            r#"{LANCE}
const table = await (await lancedb.connect(process.env.DB)).openTable("chunks");
console.log((await table.query().toArray()).map((row) => row.filePath).sort().join("\n"));
"#
        );
        assert_eq!(
            node(&open, &list, &[("DB", &db.display().to_string())]),
            format!("{to}dir/other.ts\n{to}file")
        );
        assert_eq!(
            node(&open, &list, &[("DB", &main_db)]),
            format!("{from}dir/other.ts\n{from}file")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
