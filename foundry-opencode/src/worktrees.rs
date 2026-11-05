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
/// What earlier releases wrote below their markers; replaced by `rules()`.
const LEGACY: [&str; 2] = [".opencode", ".openchamber"];
/// Folders inside `.opencode` that teams share through the repository (OpenCode project commands).
/// They stay in Git and are never linked: each worktree has its own checked-out copy.
const SHARED: [&str; 1] = ["commands"];
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
// Every row and manifest entry must now point into the worktree; anything else would send its
// searches (and edits) to another checkout.
const moved = await table.countRows(`starts_with(filePath, ${sql(to)})`);
if (moved !== rows || (await table.countRows()) !== rows) {
  throw new Error(`${rows - moved} of ${rows} rows do not point into ${to}`);
}
const manifest = JSON.parse(fs.readFileSync(`${db}/manifest.json`, "utf8"));
if (!Object.keys(manifest.files ?? {}).every((file) => file.startsWith(to))) {
  throw new Error(`manifest entries do not point into ${to}`);
}
"#;

pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    bare: bool,
}

/// Entries that stay per worktree: the RAG index, logs, runtime state, backups and shared folders.
fn per_worktree(name: &str) -> bool {
    generated(name) || name.contains(".backup-") || SHARED.contains(&name)
}

/// Ignore rules for one machine's setup: everything in `.opencode` except the shared folders, and
/// all of `.openchamber`. Ignoring `.opencode` as a whole would stop Git from ever seeing
/// `.opencode/commands`, so the folder's contents are ignored instead.
fn rules() -> Vec<String> {
    let mut rules = vec![".opencode/*".to_owned()];
    rules.extend(SHARED.iter().map(|name| format!("!.opencode/{name}/")));
    rules.push(".openchamber".into());
    rules
}

/// A rule that ignores `.opencode` as a whole, which no later exception can undo.
fn whole_folder(line: &str) -> bool {
    matches!(
        line.trim(),
        ".opencode"
            | ".opencode/"
            | "/.opencode"
            | "/.opencode/"
            | "**/.opencode"
            | "**/.opencode/"
    )
}

/// Removes this installer's block (its marker and the rule lines after it, in any release's
/// format) and every whole-folder `.opencode` rule from an ignore file. Returns the remaining
/// lines, where the block or the first replaced rule was (so the new block keeps its place and
/// the user's later exceptions keep overriding it), and whether a whole-folder rule was replaced.
fn strip(text: &str, mark: &str) -> (Vec<String>, Option<usize>, bool) {
    let ours = rules();
    let mut kept = Vec::new();
    let mut place = None;
    let mut replaced = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line == mark {
            place.get_or_insert(kept.len());
            while lines
                .next_if(|line| LEGACY.contains(line) || ours.iter().any(|rule| rule == line))
                .is_some()
            {}
        } else if whole_folder(line) {
            place.get_or_insert(kept.len());
            replaced = true;
        } else {
            kept.push(line.to_owned());
        }
    }
    (kept, place, replaced)
}

/// Writes `lines` with this installer's block (`rules`; none: no block) at `place`, or at the end,
/// when that changes more than a final newline. Keeps the file's line endings. Returns whether it
/// wrote.
fn rewrite(
    path: &Path,
    old: &str,
    mut lines: Vec<String>,
    place: Option<usize>,
    mark: &str,
    rules: &[String],
) -> eyre::Result<bool> {
    if !rules.is_empty() {
        let at = place.unwrap_or(lines.len());
        let block = std::iter::once(mark.to_owned()).chain(rules.iter().cloned());
        lines.splice(at..at, block);
    }
    let newline = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let text: String = lines
        .iter()
        .map(|line| format!("{line}{newline}"))
        .collect();
    if text.trim_end() == old.trim_end() {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, text)?;
    Ok(true)
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

/// `node`, without the variables Git exports to aliases: the RAG plugin runs Git itself and would
/// otherwise inspect the checkout the alias ran in, not `dir`.
fn node_command(dir: &Path) -> Command {
    let mut command = Command::new("node");
    command.current_dir(dir);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
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
            // Earlier releases linked shared folders too; each worktree uses its own copy.
            if SHARED.contains(&&*name.to_string_lossy()) && ours(&path, &source) {
                fs::remove_file(&path)?;
            }
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

/// Puts this installer's rules in the repository's never-pushed `.git/info/exclude`, which applies
/// to every worktree and branch at once. An earlier release's block is replaced.
fn exclude(repo: &Path) -> eyre::Result<()> {
    let path = exclude_file(repo)?;
    let text = read_plain(&path)?;
    let (kept, place, _) = strip(&text, MARK);
    rewrite(&path, &text, kept, place, MARK, &rules())?;
    Ok(())
}

/// Whether a remaining `.gitignore` line already provides `rule`, ignoring leading and trailing
/// slashes (`/.openchamber/` provides `.openchamber`).
fn provided(lines: &[String], rule: &str) -> bool {
    let normal = |text: &str| {
        let text = text.trim();
        let (negated, rest) = match text.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        (
            negated,
            rest.trim_start_matches('/')
                .trim_end_matches('/')
                .to_owned(),
        )
    };
    let wanted = normal(rule);
    lines.iter().any(|line| normal(line) == wanted)
}

/// Keeps one machine's setup out of Git while `.opencode/commands` stays shared: in the
/// repository's `.gitignore`, so every clone follows it, and in `.git/info/exclude`, which covers
/// every worktree and branch before that change is committed. Whole-folder `.opencode` rules are
/// replaced, since they would hide the shared folders. Files already committed are reported.
pub fn ignore(repo: &Path) -> eyre::Result<Vec<String>> {
    let mut messages = Vec::new();
    let gitignore = repo.join(".gitignore");
    let text = read_plain(&gitignore)?;
    let (kept, place, replaced) = strip(&text, GITIGNORE_MARK);
    let all = rules();
    // The exceptions must follow `.opencode/*`, so they are added with it.
    let (opencode, openchamber) = all.split_at(all.len() - 1);
    let mut missing: Vec<String> = if provided(&kept, &opencode[0]) {
        opencode[1..]
            .iter()
            .filter(|rule| !provided(&kept, rule))
            .cloned()
            .collect()
    } else {
        opencode.to_vec()
    };
    if !provided(&kept, &openchamber[0]) {
        missing.push(openchamber[0].clone());
    }
    if rewrite(&gitignore, &text, kept, place, GITIGNORE_MARK, &missing)? {
        messages.push(format!(
            "Updated {}{}: keeps local .opencode/.openchamber setup out of Git and shares .opencode/commands; commit this change",
            gitignore.display(),
            if replaced {
                " (replaced its whole-folder .opencode rule)"
            } else {
                ""
            }
        ));
    }
    exclude(repo)?;
    let tracked = git(
        repo,
        &[
            "ls-files",
            "--",
            ".opencode",
            ".openchamber",
            ":(exclude).opencode/commands",
        ],
    )?;
    if !tracked.is_empty() {
        let files: Vec<&str> = tracked.lines().collect();
        messages.push(format!(
            "{} commits machine-specific files under .opencode/.openchamber ({}{}). Remove them from the repository with: git rm -r --cached -- .opencode .openchamber ':!.opencode/commands', then commit",
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

/// Removes only the `.git/info/exclude` block this installer added. `.gitignore` lines are kept.
pub fn unexclude(repo: &Path) -> eyre::Result<()> {
    let path = exclude_file(repo)?;
    let text = read_plain(&path)?;
    let ours = rules();
    let mut kept = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line == MARK {
            while lines
                .next_if(|line| LEGACY.contains(line) || ours.iter().any(|rule| rule == line))
                .is_some()
            {}
        } else {
            kept.push(line.to_owned());
        }
    }
    rewrite(&path, &text, kept, None, MARK, &[])?;
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
    // Built under another name and renamed only once verified, so an interrupted seed never
    // leaves an index that points at the main checkout.
    let staging = worktree.join(".opencode/rag_db.seeding");
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
    let _ = fs::remove_dir_all(&staging);
    let copied = |flags: &str| {
        Command::new("cp")
            .arg(flags)
            .arg(&source)
            .arg(&staging)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !copied("-cR") {
        let _ = fs::remove_dir_all(&staging);
        eyre::ensure!(copied("-R"), "could not copy {}", source.display());
    }
    let repointed = (|| -> eyre::Result<()> {
        let mut child = node_command(&worktree.join(".opencode"))
            .args(["--input-type=module", "-"])
            .env("SEED_FROM", format!("{}/", main.display()))
            .env("SEED_TO", format!("{}/", worktree.display()))
            .env("SEED_DB", &staging)
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
        // Node prints the stack and then its version; the reason is the `Error:` line.
        let stderr = String::from_utf8_lossy(&output.stderr);
        eyre::ensure!(
            output.status.success(),
            "{}",
            stderr
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Error"))
                .unwrap_or("re-pointing failed")
        );
        fs::rename(&staging, &target)?;
        Ok(())
    })();
    if repointed.is_err() {
        let _ = fs::remove_dir_all(&staging);
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
        node_command(path)
            .args([HELPER, "index"])
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

/// Uncommitted changes, ignoring the local setup folders but not the shared ones in `.opencode`.
fn changes(path: &Path) -> eyre::Result<String> {
    let local = git(
        path,
        &[
            "status",
            "--porcelain",
            "--",
            ".",
            ":(exclude).opencode",
            ":(exclude).openchamber",
        ],
    )?;
    let shared: Vec<String> = SHARED
        .iter()
        .map(|name| format!(".opencode/{name}"))
        .collect();
    let mut args = vec!["status", "--porcelain", "--"];
    args.extend(shared.iter().map(String::as_str));
    Ok(format!("{local}{}", git(path, &args)?))
}

fn report_link(main: &Path, path: &Path) -> bool {
    show_link(main, link(main, path))
}

fn show_link(main: &Path, linked: eyre::Result<usize>) -> bool {
    match linked {
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
        // Linking first also removes links an earlier release made to shared folders, which Git
        // would otherwise report as uncommitted work.
        let linked = (path != &main && !dry_run).then(|| link(&main, path));
        let moved = match &worktree.branch {
            Some(branch) if branch != &default => Err(format!("is on branch {branch}. Not moved.")),
            branch => match changes(path) {
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
        if let Some(linked) = linked {
            ok &= show_link(&main, linked);
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
    fn ignore_hides_local_setup_but_keeps_opencode_commands_shared() {
        let root = sandbox();
        let main = repository(&root);
        let exclude_path = main.join(".git/info/exclude");
        // An earlier release's exclude block and a hand-written whole-folder rule.
        fs::write(
            &exclude_path,
            format!("keep\n{MARK}\n.opencode\n.openchamber\n"),
        )
        .unwrap();
        fs::write(main.join(".gitignore"), "/.opencode/\nnode_modules").unwrap();
        run(&main, &["add", ".gitignore"]);
        run(&main, &["commit", "-qm", "ignore"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../linked"]);
        let messages = ignore(&main).unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("replaced its whole-folder .opencode rule"));
        let rules = ".opencode/*\n!.opencode/commands/\n.openchamber\n";
        // The block takes the replaced rule's place.
        assert_eq!(
            fs::read_to_string(main.join(".gitignore")).unwrap(),
            format!("{GITIGNORE_MARK}\n{rules}node_modules\n")
        );
        assert_eq!(
            fs::read_to_string(&exclude_path).unwrap(),
            format!("keep\n{MARK}\n{rules}")
        );
        assert!(ignore(&main).unwrap().is_empty());

        // An earlier release's .gitignore block is migrated the same way.
        fs::write(
            main.join(".gitignore"),
            format!("node_modules\n{GITIGNORE_MARK}\n.opencode\n.openchamber\n"),
        )
        .unwrap();
        assert_eq!(ignore(&main).unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(main.join(".gitignore")).unwrap(),
            format!("node_modules\n{GITIGNORE_MARK}\n{rules}")
        );

        // Only shared commands show up; the rest of the local setup stays hidden everywhere.
        fs::create_dir_all(main.join(".opencode/commands")).unwrap();
        fs::write(main.join(".opencode/commands/deploy.md"), "deploy").unwrap();
        fs::create_dir(main.join(".openchamber")).unwrap();
        fs::write(main.join(".openchamber/state"), "local").unwrap();
        link(&main, &root.join("linked")).unwrap();
        assert_eq!(
            run(&main, &["status", "--porcelain", "-uall"]),
            " M .gitignore\n?? .opencode/commands/deploy.md"
        );
        assert_eq!(run(&root.join("linked"), &["status", "--porcelain"]), "");
        assert!(fs::symlink_metadata(root.join("linked/.opencode/commands")).is_err());
        assert_eq!(linked(&main), [root.join("linked")]);
        unexclude(&main).unwrap();
        assert_eq!(fs::read_to_string(&exclude_path).unwrap(), "keep\n");
        assert!(!excluded(&main));

        // Equivalent hand-written rules are left alone.
        let own = root.join("own");
        fs::create_dir(&own).unwrap();
        run(&own, &["init", "-q"]);
        let written = "/.opencode/*\n!/.opencode/commands\n/.openchamber/\n";
        fs::write(own.join(".gitignore"), written).unwrap();
        assert!(ignore(&own).unwrap().is_empty());
        assert_eq!(fs::read_to_string(own.join(".gitignore")).unwrap(), written);

        // Committed local files are reported; committed shared commands are not.
        let tracked = root.join("tracked");
        fs::create_dir_all(tracked.join(".opencode/commands")).unwrap();
        run(&tracked, &["init", "-q"]);
        fs::write(tracked.join(".opencode/agent.md"), "local").unwrap();
        fs::write(tracked.join(".opencode/commands/review.md"), "shared").unwrap();
        run(&tracked, &["add", ".opencode"]);
        let messages = ignore(&tracked).unwrap();
        assert!(messages[0].starts_with("Updated"), "{messages:?}");
        assert!(
            messages[1].contains(".opencode/agent.md")
                && !messages[1].contains("review.md")
                && messages[1].contains("':!.opencode/commands'"),
            "{messages:?}"
        );
        assert_eq!(
            run(&tracked, &["ls-files"]),
            ".opencode/agent.md\n.opencode/commands/review.md"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_folders_are_never_linked_and_earlier_links_to_them_are_removed() {
        let root = sandbox();
        let main = repository(&root);
        fs::create_dir(main.join(".opencode/commands")).unwrap();
        fs::write(main.join(".opencode/commands/deploy.md"), "deploy").unwrap();
        let worktree = root.join("linked");
        fs::create_dir_all(worktree.join(".opencode")).unwrap();
        std::os::unix::fs::symlink(
            main.join(".opencode/commands"),
            worktree.join(".opencode/commands"),
        )
        .unwrap();
        link(&main, &worktree).unwrap();
        assert!(fs::symlink_metadata(worktree.join(".opencode/commands")).is_err());
        fs::create_dir(worktree.join(".opencode/commands")).unwrap();
        fs::write(worktree.join(".opencode/commands/own.md"), "own").unwrap();
        link(&main, &worktree).unwrap();
        assert!(worktree.join(".opencode/commands/own.md").is_file());
        assert!(main.join(".opencode/commands/deploy.md").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_treats_edited_shared_commands_as_uncommitted_work() {
        let root = sandbox();
        let main = repository(&root);
        exclude(&main).unwrap();
        fs::create_dir(main.join(".opencode/commands")).unwrap();
        fs::write(main.join(".opencode/commands/deploy.md"), "one\n").unwrap();
        run(&main, &["add", ".opencode/commands/deploy.md"]);
        run(&main, &["commit", "-qm", "commands"]);
        run(&main, &["push", "-q"]);
        let first = run(&main, &["rev-parse", "HEAD"]);
        run(&main, &["worktree", "add", "-q", "--detach", "../edited"]);
        run(
            &main,
            &["worktree", "add", "-q", "--detach", "../untouched"],
        );
        fs::write(root.join("edited/.opencode/commands/deploy.md"), "two\n").unwrap();
        // An earlier release linked shared folders; a worktree from before they were committed
        // keeps such a link, which must not count as uncommitted work.
        run(
            &main,
            &["worktree", "add", "-q", "--detach", "../stale", "HEAD~1"],
        );
        fs::create_dir(root.join("stale/.opencode")).unwrap();
        std::os::unix::fs::symlink(
            main.join(".opencode/commands"),
            root.join("stale/.opencode/commands"),
        )
        .unwrap();
        run(&root, &["clone", "-q", "origin.git", "other"]);
        fs::write(root.join("other/file"), "two\n").unwrap();
        run(&root.join("other"), &["commit", "-qam", "two"]);
        run(&root.join("other"), &["push", "-q"]);
        assert!(!sync(&main, false, false).unwrap());
        assert_eq!(run(&root.join("edited"), &["rev-parse", "HEAD"]), first);
        let latest = run(&main, &["rev-parse", "origin/main"]);
        assert_eq!(run(&root.join("untouched"), &["rev-parse", "HEAD"]), latest);
        assert_eq!(run(&root.join("stale"), &["rev-parse", "HEAD"]), latest);
        // The stale link was removed before moving, so Git checked out a real folder there.
        assert!(
            !fs::symlink_metadata(root.join("stale/.opencode/commands"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(root.join("stale/.opencode/commands/deploy.md").is_file());
        assert_eq!(run(&main, &["rev-parse", "HEAD"]), latest);
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
    fn status(dir: &Path) -> String {
        run(dir, &["status", "--porcelain", "-uall"])
    }

    #[test]
    fn ignore_rules_keep_their_place_line_endings_and_later_exceptions() {
        let root = sandbox();
        let main = repository(&root);
        fs::create_dir_all(main.join(".opencode/agents")).unwrap();
        fs::write(main.join(".opencode/agents/review.md"), "shared").unwrap();
        let gitignore = main.join(".gitignore");
        // An exception the team added after an earlier release's block keeps working.
        fs::write(
            &gitignore,
            format!("{GITIGNORE_MARK}\n.opencode\n.openchamber\n!.opencode/agents/\ndist\n"),
        )
        .unwrap();
        ignore(&main).unwrap();
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            format!(
                "{GITIGNORE_MARK}\n.opencode/*\n!.opencode/commands/\n.openchamber\n!.opencode/agents/\ndist\n"
            )
        );
        assert_eq!(
            status(&main),
            "?? .gitignore\n?? .opencode/agents/review.md"
        );
        assert!(ignore(&main).unwrap().is_empty());

        // `**/.opencode` hides the shared folders too, in either file; CRLF files stay CRLF.
        fs::write(&gitignore, "dist\r\n**/.opencode/\r\n").unwrap();
        fs::write(main.join(".git/info/exclude"), ".opencode\n").unwrap();
        assert_eq!(ignore(&main).unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(&gitignore).unwrap(),
            format!(
                "dist\r\n{GITIGNORE_MARK}\r\n.opencode/*\r\n!.opencode/commands/\r\n.openchamber\r\n"
            )
        );
        assert_eq!(
            fs::read_to_string(main.join(".git/info/exclude")).unwrap(),
            format!("{MARK}\n.opencode/*\n!.opencode/commands/\n.openchamber\n")
        );

        // A missing final newline alone is not a change.
        let own = root.join("own");
        fs::create_dir(&own).unwrap();
        run(&own, &["init", "-q"]);
        let written = "/.opencode/*\n!/.opencode/commands\n/.openchamber/";
        fs::write(own.join(".gitignore"), written).unwrap();
        assert!(ignore(&own).unwrap().is_empty());
        assert_eq!(fs::read_to_string(own.join(".gitignore")).unwrap(), written);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn node_runs_without_the_variables_git_gives_aliases() {
        let command = node_command(Path::new("/"));
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
        ] {
            assert!(removed.iter().any(|item| item == name), "{name}");
        }
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

        // A row outside the main checkout (an index built under another path) is refused.
        node(
            &open,
            &format!(
                r#"{LANCE}
const table = await (await lancedb.connect(process.env.DB)).openTable("chunks");
await table.add([{{ id: "foreign", filePath: "/elsewhere/file", vector: [9, 9] }}]);
"#
            ),
            &[("DB", &main_db)],
        );
        run(&main, &["worktree", "add", "-q", "--detach", "../second"]);
        let second = root.join("second");
        link(&main, &second).unwrap();
        let error = seed(&main, &second).unwrap_err().to_string();
        assert!(error.contains("do not point into"), "{error}");
        assert!(!second.join(".opencode/rag_db").exists());
        assert!(!second.join(".opencode/rag_db.seeding").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
