mod cleanup;
mod ops;
mod projects;
mod rag;
mod stack;
mod ui;
mod worktrees;

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{self, Context};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use ratatui::DefaultTerminal;
use ui::notice;

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn version() -> &'static str {
    option_env!("FOUNDRY_OPENCODE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Parser)]
#[command(version = version(), about = "Foundry OpenCode installer")]
struct Args {
    /// Preview actions without opening a browser or changing anything
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    command: Option<Mode>,
}

#[derive(Clone, Copy, Subcommand)]
enum Mode {
    /// Configure a Git repository with its assigned Foundry stack
    Init,
    /// Configure and index RAG in the current project
    InitRag,
    /// Retire a verified V1 RAG plugin in the current project
    RepairPlugins,
    /// Move this repository's worktrees to the default branch, link .opencode and reindex RAG
    SyncWorktrees,
    /// Update packages, global config and assigned repositories
    Update {
        /// Accept saved stacks, repositories and RAG choices without opening the wizard
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove installed programs, configs, stack credentials and user data
    Uninstall,
    /// Stop OpenCode and OpenChamber and clear the OpenCode cache
    Reset,
    /// Install tools and set up Foundry OpenCode (the default)
    Install,
    /// Print the installer version
    Version,
    /// Show example commands
    Usage,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Mode::Version) => {
            println!("{}", version());
            return Ok(());
        }
        Some(Mode::SyncWorktrees) => {
            // Exit quietly when piped into `head` instead of panicking on a closed stdout.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            if !worktrees::sync(&std::env::current_dir()?, args.dry_run)? {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Mode::Usage) => {
            println!(
                "foundry-opencode           Install (same as install)\nfoundry-opencode install   Install tools and assign repositories to Foundry stacks\nfoundry-opencode update    Update tools, global config and project assignments\nfoundry-opencode update -y Accept saved defaults and update without the wizard\nfoundry-opencode repair-plugins Retire verified V1 RAG plugin in current project\nfoundry-opencode uninstall Remove tools, configs, credentials and user data\nfoundry-opencode reset     Stop OpenCode/OpenChamber and clear OpenCode cache\nfoundry-opencode init      Configure a Git repository for a Foundry stack\nfoundry-opencode init-rag  Configure and index RAG in the current project\nfoundry-opencode sync-worktrees Move worktrees to the default branch, link .opencode, reindex RAG\nfoundry-opencode --dry-run uninstall Preview full removal\nfoundry-opencode update -y --dry-run Preview saved defaults without changes\nfoundry-opencode version   Print version\nfoundry-opencode usage     Show these examples"
            );
            return Ok(());
        }
        _ => {}
    }
    color_eyre::install()?;
    if !cfg!(target_os = "macos")
        && !args.dry_run
        && !matches!(
            args.command,
            Some(Mode::Init | Mode::InitRag | Mode::RepairPlugins | Mode::Reset)
        )
    {
        eyre::bail!("Installation and updates require macOS and Homebrew");
    }
    let mut quick_summary = None;
    let staged = match ratatui::run(|terminal| {
        execute!(std::io::stdout(), EnableMouseCapture)?;
        let result = match args.command.unwrap_or(Mode::Install) {
            Mode::Init => init(terminal, args.dry_run).map(|_| None),
            Mode::InitRag => init_rag(terminal, args.dry_run).map(|_| None),
            Mode::RepairPlugins => repair_plugins(terminal, args.dry_run).map(|_| None),
            Mode::Update { yes } => update(terminal, args.dry_run, yes, &mut quick_summary),
            Mode::Uninstall => uninstall(terminal, args.dry_run).map(|_| None),
            Mode::Reset => reset(terminal, args.dry_run).map(|_| None),
            Mode::Install => install(terminal, args.dry_run),
            _ => unreachable!(),
        };
        execute!(std::io::stdout(), DisableMouseCapture)?;
        result
    }) {
        Ok(staged) => staged,
        Err(error) if error.is::<Cancelled>() => {
            println!("Cancelled; no changes were made.");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if let Some(summary) = quick_summary {
        println!("{summary}");
    }
    if let Some(staged) = staged {
        ops::install_binary(&staged, std::path::Path::new(ops::BIN_PATH))?;
        println!("Installed foundry-opencode to {}", ops::BIN_PATH);
    }
    if matches!(args.command, Some(Mode::Uninstall)) && !args.dry_run {
        let binary = std::path::Path::new(ops::BIN_PATH);
        if fs::symlink_metadata(binary).is_ok() {
            match remove_safe(binary) {
                Ok(()) => {}
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) =>
                {
                    let script =
                        "if [ -L \"$1\" ] || [ ! -f \"$1\" ]; then exit 1; fi; /bin/rm -f \"$1\"";
                    eyre::ensure!(
                        Process::new("sudo")
                            .args(["/bin/sh", "-c", script, "uninstall"])
                            .arg(binary)
                            .status()?
                            .success(),
                        "Could not remove installer binary"
                    );
                }
                Err(error) => return Err(error),
            }
            println!("Removed {}", binary.display());
        }
    }
    Ok(())
}

fn init_rag(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    let cwd = std::env::current_dir()?;
    if let Ok(Some(main)) = worktrees::main_of(&cwd) {
        eyre::bail!(
            "{} is a Git worktree of {}. Run init-rag there; this worktree then gets the shared setup and its own index from foundry-opencode sync-worktrees",
            cwd.display(),
            main.display()
        );
    }
    let (url, token) = stack::credentials(&cwd)?;
    let preview = rag::setup(&cwd, &url, true, |_, _| unreachable!())?;
    if ui::step_review(
        terminal,
        "Review RAG setup",
        &preview,
        dry_run,
        false,
        "Initialize RAG",
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        return notice(terminal, "Dry-run complete", &preview);
    }
    let mut progress = Progress {
        done: 0,
        total: 2,
        output: Vec::new(),
    };
    progress.show(terminal, "Preparing RAG files")?;
    let mut projects = load_paths(&home()?.join(".config/foundry-opencode/rag.json"))?;
    if !projects.contains(&cwd) {
        projects.push(cwd.clone());
    }
    save_paths(
        &home()?.join(".config/foundry-opencode/rag.json"),
        &projects,
    )?;
    let result = rag::setup(&cwd, &url, false, |label, mut command| {
        command.env("OPENAI_API_KEY", &token);
        run(terminal, &mut progress, label, command)
    })?;
    notice(terminal, "RAG — complete", &result)
}

fn register_rag_project(home: &std::path::Path, project: &std::path::Path) -> eyre::Result<()> {
    let registry = home.join(".config/foundry-opencode/rag.json");
    let mut paths = load_paths(&registry)?;
    if !paths.contains(&project.to_path_buf()) {
        paths.push(project.to_path_buf());
        save_paths(&registry, &paths)?;
    }
    Ok(())
}

fn repair_plugins(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    let project = std::env::current_dir()?;
    let Some(path) = rag::legacy_plugin(&project)? else {
        return notice(
            terminal,
            "No known V1 RAG plugin",
            "No installer-known V1 RAG plugin was found in this project. Other plugins were not changed.",
        );
    };
    let backup = ops::backup_path(&project.join(".opencode/rag-plugin.js"));
    let detail = format!(
        "The known V1 RAG watcher at {} fails to load in OpenCode V2. Move it out of the auto-loaded plugins folder to {}. Existing index and RAG MCP configuration will be preserved. Reindex manually with node .opencode/rag-cli.mjs index if that helper exists.",
        path.display(),
        backup.display()
    );
    if ui::step_review(
        terminal,
        "Repair V1 RAG plugin",
        &detail,
        dry_run,
        true,
        "Retire V1 plugin",
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        return notice(terminal, "Dry-run complete", &detail);
    }
    eyre::ensure!(
        rag::legacy_plugin(&project)?.as_ref() == Some(&path),
        "RAG plugin changed during review; not moving it"
    );
    fs::rename(&path, &backup)?;
    notice(
        terminal,
        "Plugin repair — complete",
        &format!(
            "V1 RAG plugin backed up at {}. Restart only if OpenCode does not reload automatically. If this project has no RAG MCP helper, run init-rag from the project root to configure it.",
            backup.display()
        ),
    )
}

fn token_page(url: &str) -> String {
    format!("{url}/workspace/settings/tokens")
}

fn open_token_page(terminal: &mut DefaultTerminal, url: &str, dry_run: bool) -> eyre::Result<()> {
    let page = token_page(url);
    if dry_run {
        return notice(
            terminal,
            "Browser preview",
            &format!("Would open {page}; no browser opened."),
        );
    }
    eyre::ensure!(cfg!(target_os = "macos"), "Browser flow requires macOS");
    eyre::ensure!(
        Process::new("open").arg(&page).status()?.success(),
        "Could not open Foundry token page"
    );
    Ok(())
}

fn init(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    eyre::ensure!(
        projects::is_repo(&cwd)?,
        "Run init from a project repository root"
    );
    let user_home = home()?;
    let catalog = projects::collect(terminal, &user_home, dry_run, Some(&cwd))?;
    let project = catalog
        .projects
        .iter()
        .find(|item| item.path == cwd)
        .ok_or_else(|| eyre::eyre!("Select the current project"))?;
    let stack = catalog
        .choices()
        .into_iter()
        .find(|stack| stack.id == project.stack)
        .ok_or_else(|| eyre::eyre!("Select a configured stack"))?;
    let detail = format!(
        "Back up and replace configuration in {} with stack {} ({}). RAG: {}. Private credentials remain outside the repository. Existing unrelated .opencode files and legacy parent configs are preserved.",
        cwd.display(),
        stack.name,
        stack.url,
        if project.rag {
            "initialize"
        } else {
            "not selected"
        }
    );
    if ui::step_review(
        terminal,
        "Review project config",
        &detail,
        dry_run,
        false,
        "Initialize project",
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        return notice(terminal, "Dry-run complete", &detail);
    }
    catalog.save(&user_home)?;
    let mut progress = Progress {
        done: 0,
        total: 1 + usize::from(project.rag) * 2,
        output: Vec::new(),
    };
    let upgraded = if project.rag {
        upgrade_rag(
            terminal,
            &mut progress,
            &cwd,
            &stack.url,
            &catalog.tokens[&stack.id],
        )?
    } else {
        None
    };
    let existing_rag = project.rag && rag::ready(&cwd, &stack.url);
    let backups = projects::write_project(&user_home, project, stack)?;
    let mut result = format!(
        "Configured {} ({} previous config files backed up).",
        cwd.display(),
        backups.len()
    );
    if let Some(message) = upgraded {
        result.push_str(&format!("\n{message}"));
    }
    if existing_rag {
        result.push_str("\nReused the existing RAG index.");
    } else if project.rag {
        let token = &catalog.tokens[&stack.id];
        match rag::setup(&cwd, &stack.url, false, |label, mut command| {
            command.env("OPENAI_API_KEY", token);
            run(terminal, &mut progress, label, command)
        }) {
            Ok(message) => {
                register_rag_project(&user_home, &cwd)?;
                result.push_str(&format!("\n{message}"));
            }
            Err(error) => result.push_str(&format!("\nRAG incomplete: {error}")),
        }
    }
    for message in share_worktrees(&cwd) {
        result.push_str(&format!("\n{message}"));
    }
    notice(terminal, "Project init — complete", &result)
}

// shortcut: external commands report no percentages, replace the spinner when they expose progress.
struct Progress {
    done: usize,
    total: usize,
    output: Vec<String>,
}

impl Progress {
    fn show(&self, terminal: &mut DefaultTerminal, label: &str) -> eyre::Result<()> {
        terminal.draw(|frame| {
            ui::progress_with_output(frame, label, self.done, self.total, &self.output)
        })?;
        Ok(())
    }

    fn line(&mut self, bytes: &[u8]) -> bool {
        let text = String::from_utf8_lossy(bytes);
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .take(500)
            .collect();
        if !text.trim().is_empty() {
            self.output.push(text);
            if self.output.len() > 200 {
                self.output.remove(0);
            }
            return true;
        }
        false
    }
}

fn busy<T: Send + 'static>(
    terminal: &mut DefaultTerminal,
    label: &str,
    work: impl FnOnce() -> eyre::Result<T> + Send + 'static,
) -> eyre::Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut tick = 0;
    let spinner = ['|', '/', '-', '\\'];
    loop {
        terminal
            .draw(|frame| ui::progress(frame, &format!("{label} {}", spinner[tick % 4]), 0, 1))?;
        tick += 1;
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eyre::bail!("{label} stopped unexpectedly")
            }
        }
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    label: &str,
    command: Process,
) -> eyre::Result<()> {
    run_limited(terminal, progress, label, command, None)
}

fn stream_output(mut stream: impl Read, tx: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0; 2048];
    let mut line = Vec::new();
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                for byte in &buffer[..count] {
                    if *byte == b'\r' || *byte == b'\n' {
                        if tx.send(std::mem::take(&mut line)).is_err() {
                            return;
                        }
                    } else if line.len() < 4096 {
                        // shortcut: keep only 4096 bytes of a command line; raise for larger diagnostics.
                        line.push(*byte);
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        let _ = tx.send(line);
    }
}

fn run_limited(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    label: &str,
    mut command: Process,
    timeout: Option<Duration>,
) -> eyre::Result<()> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .wrap_err(format!("{label} failed to start"))?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let streams: [Box<dyn std::io::Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || stream_output(stream, tx));
    }
    drop(tx);
    let mut tick = 0;
    let spinner = ['|', '/', '-', '\\'];
    let mut status = None;
    let started = Instant::now();
    let mut last_draw = started - Duration::from_millis(120);
    let mut recent = Vec::new();
    loop {
        if last_draw.elapsed() >= Duration::from_millis(120) {
            progress.show(
                terminal,
                &format!("{label}  {}", spinner[tick % spinner.len()]),
            )?;
            tick += 1;
            last_draw = Instant::now();
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_none() && timeout.is_some_and(|limit| started.elapsed() >= limit) {
            child.kill()?;
            child.wait()?;
            eyre::bail!("{label} timed out");
        }
        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(line) => {
                if progress.line(&line)
                    && let Some(line) = progress.output.last()
                {
                    recent.push(line.clone());
                    if recent.len() > 8 {
                        recent.remove(0);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = match status {
                    Some(status) => status,
                    None => child.wait()?,
                };
                progress.show(terminal, label)?;
                if !status.success() {
                    eyre::bail!("{label} failed ({status}): {}", recent.join("\n"));
                }
                progress.done += 1;
                progress.show(terminal, label)?;
                return Ok(());
            }
        }
    }
}

fn run_interactive_git(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    label: &str,
    mut command: Process,
    mut retry: Process,
    retry_directory: Option<&std::path::Path>,
) -> eyre::Result<()> {
    command.env("GIT_TERMINAL_PROMPT", "0");
    match run(terminal, progress, label, command) {
        Ok(()) => return Ok(()),
        Err(error)
            if [
                "terminal prompts disabled",
                "Authentication failed",
                "could not read Username",
                "could not read Password",
            ]
            .iter()
            .any(|text| error.to_string().contains(text)) => {}
        Err(error) => return Err(error),
    }
    if let Some(path) = retry_directory {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(path)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => eyre::bail!(
                "Cannot retry Git authentication; inspect {}",
                path.display()
            ),
        }
    }
    execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::try_restore()?;
    eyre::ensure!(
        !crossterm::terminal::is_raw_mode_enabled()?,
        "Cannot prompt for Git credentials while the terminal is in raw mode"
    );
    println!("{label}.");
    let result = retry.status();
    *terminal = ratatui::try_init()?;
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let status = result.wrap_err(format!("{label} failed to start"))?;
    eyre::ensure!(status.success(), "{label} failed ({status})");
    progress.done += 1;
    progress.show(terminal, label)
}

fn installed(name: &str, cask: bool) -> eyre::Result<bool> {
    let mut command = Process::new("brew");
    command.arg("list");
    if cask {
        command.arg("--cask");
    }
    match command.arg("--versions").arg(name).output() {
        Ok(output) => Ok(output.status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn brew_package(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    name: &str,
    cask: bool,
) -> eyre::Result<()> {
    let exists = installed(name, cask)?;
    let mut command = Process::new("brew");
    command.arg(if exists { "upgrade" } else { "install" });
    if cask {
        command.arg("--cask");
    }
    command.arg(name);
    run(
        terminal,
        progress,
        &format!("{} {name}", if exists { "Upgrading" } else { "Installing" }),
        command,
    )
}

fn brew_update(terminal: &mut DefaultTerminal, progress: &mut Progress) -> eyre::Result<()> {
    eyre::ensure!(
        Process::new("brew").arg("--version").output().is_ok(),
        "Homebrew is required: https://brew.sh"
    );
    run(terminal, progress, "Refreshing Homebrew", {
        let mut cmd = Process::new("brew");
        cmd.arg("update");
        cmd
    })
}

fn chamber_running() -> eyre::Result<bool> {
    let output = Process::new("pgrep").args(["-x", "OpenChamber"]).output()?;
    chamber_status(output.status.code())
}

fn chamber_status(code: Option<i32>) -> eyre::Result<bool> {
    match code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => eyre::bail!(
            "Could not determine whether OpenChamber is running; skipping its upgrade is safer"
        ),
    }
}

fn chamber_available(terminal: &mut DefaultTerminal) -> bool {
    match chamber_running() {
        Ok(false) => true,
        Ok(true) => {
            let _ = notice(
                terminal,
                "OpenChamber skipped",
                "OpenChamber is running. Quit it and rerun install/update to upgrade it. The remaining setup will continue.",
            );
            false
        }
        Err(error) => {
            let _ = notice(
                terminal,
                "OpenChamber skipped",
                &format!("{error}. The remaining setup will continue."),
            );
            false
        }
    }
}

fn optional_chamber(terminal: &mut DefaultTerminal, progress: &mut Progress, quick: bool) -> bool {
    if quick {
        let skip = match chamber_running() {
            Ok(false) => None,
            Ok(true) => Some("OpenChamber is running; upgrade skipped".to_owned()),
            Err(error) => Some(format!("OpenChamber upgrade skipped: {error}")),
        };
        if let Some(message) = skip {
            progress.line(message.as_bytes());
            progress.done += 1;
            return false;
        }
    } else if !chamber_available(terminal) {
        progress.done += 1;
        return false;
    }
    if let Err(error) = brew_package(terminal, progress, "openchamber", true) {
        let message =
            format!("OpenChamber upgrade skipped: {error}. Retry after quitting OpenChamber.");
        if quick {
            progress.line(message.as_bytes());
        } else {
            let _ = notice(terminal, "OpenChamber upgrade skipped", &message);
        }
        progress.done += 1;
        return false;
    }
    true
}

fn restore_chamber(backup: &std::path::Path, app: &std::path::Path) -> eyre::Result<()> {
    eyre::ensure!(
        fs::symlink_metadata(app).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "OpenChamber upgrade failed; previous app is safe at {} but {} is occupied",
        backup.display(),
        app.display()
    );
    fs::rename(backup, app).wrap_err_with(|| {
        format!(
            "OpenChamber upgrade failed; restore the previous app from {}",
            backup.display()
        )
    })?;
    Ok(())
}

fn home() -> eyre::Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| eyre::eyre!("HOME is not set"))?,
    ))
}

fn stack_registry(home: &std::path::Path) -> PathBuf {
    home.join(".config/foundry-opencode/stacks.json")
}

fn load_stacks(home: &std::path::Path) -> eyre::Result<Vec<PathBuf>> {
    load_paths(&stack_registry(home))
}

fn load_paths(path: &std::path::Path) -> eyre::Result<Vec<PathBuf>> {
    match fs::read(path) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn save_paths(path: &std::path::Path, paths: &[PathBuf]) -> eyre::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let staged = ops::backup_path(path);
    fs::write(&staged, serde_json::to_vec_pretty(paths)?)?;
    fs::rename(staged, path)?;
    Ok(())
}

fn remove_direnv(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    if dry_run {
        return Ok(());
    }
    let user_home = home()?;
    let zsh = std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home.clone())
        .join(".zshrc");
    cleanup::clean_direnv_text(&zsh, false)?;
    if zsh != user_home.join(".zshrc") {
        cleanup::clean_direnv_text(&user_home.join(".zshrc"), false)?;
    }
    cleanup::clean_direnv_text(&user_home.join(".config/direnv/direnv.toml"), true)?;
    if installed("direnv", false)? {
        let mut progress = Progress {
            done: 0,
            total: 1,
            output: Vec::new(),
        };
        let mut command = Process::new("brew");
        command.args(["uninstall", "direnv"]);
        run(terminal, &mut progress, "Uninstalling direnv", command)?;
    }
    for path in [
        user_home.join(".config/direnv"),
        user_home.join(".local/share/direnv"),
        user_home.join(".local/state/direnv"),
        user_home.join(".cache/direnv"),
    ] {
        remove_safe(&path)?;
    }
    for path in [
        user_home.join(".local/bin/direnv"),
        user_home.join("bin/direnv"),
    ] {
        remove_safe(&path)?;
    }
    Ok(())
}

fn remove_safe(path: &std::path::Path) -> eyre::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(meta) => {
            eyre::ensure!(
                !meta.file_type().is_symlink(),
                "Refusing to remove symlink {}",
                path.display()
            );
            if meta.is_dir() {
                fs::remove_dir_all(path)?;
            } else if meta.is_file() {
                fs::remove_file(path)?;
            } else {
                eyre::bail!("Unsupported file type at {}", path.display());
            }
            Ok(())
        }
    }
}

fn uninstall(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    let user_home = home()?;
    let managed = projects::Catalog::inventory(&user_home)?;
    // shortcut: older RAG projects have no registry; add safe discovery if historical projects must be wiped automatically.
    let mut selected = load_stacks(&user_home)?;
    selected.retain(|path| path.exists());
    let mut current = user_home.canonicalize()?;
    if ui::step_explorer(terminal, &user_home, &mut current, &mut selected)? != ui::Nav::Next {
        return Err(Cancelled.into());
    }
    let mut projects = load_paths(&user_home.join(".config/foundry-opencode/rag.json"))?;
    projects.retain(|path| path.exists());
    let config = user_home.join(".config/opencode");
    let mut targets = vec![
        config.clone(),
        user_home.join(".config/foundry-opencode"),
        user_home.join(".config/direnv"),
        user_home.join(".local/share/direnv"),
        user_home.join(".local/state/direnv"),
        user_home.join(".cache/direnv"),
        user_home.join(".local/bin/direnv"),
        user_home.join("bin/direnv"),
        user_home.join(".cache/opencode"),
        user_home.join(".local/share/opencode"),
        user_home.join(".local/state/opencode"),
        user_home.join("Library/Application Support/opencode"),
        user_home.join("Library/Caches/opencode"),
        user_home.join("Library/Logs/opencode"),
        user_home.join("Library/Application Support/OpenChamber"),
        user_home.join("Library/Application Support/openchamber"),
        user_home.join(".config/openchamber"),
        user_home.join("Library/Caches/OpenChamber"),
        user_home.join("Library/Logs/OpenChamber"),
    ];
    for project in &managed.projects {
        if project.path.exists() {
            eyre::ensure!(
                project
                    .path
                    .canonicalize()
                    .is_ok_and(|path| path == project.path)
                    && project.path != user_home
                    && project.path != std::path::Path::new("/"),
                "Unsafe saved project {}",
                project.path.display()
            );
            let open = project.path.join(".opencode");
            eyre::ensure!(
                fs::symlink_metadata(&open)
                    .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink()),
                "Unsafe project config directory {}",
                open.display()
            );
            targets.push(open.join("opencode.json"));
        }
    }
    if let Ok(entries) = fs::read_dir(config.parent().unwrap()) {
        for entry in entries {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("opencode.backup-")
            {
                targets.push(entry.path());
            }
        }
    }
    if let Ok(entries) = fs::read_dir(user_home.join(".Trash")) {
        for entry in entries {
            let entry = entry?;
            if ["opencode.backup-", "OpenChamber.app.backup-"]
                .iter()
                .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
            {
                targets.push(entry.path());
            }
        }
    }
    for stack in &selected {
        eyre::ensure!(
            stack.canonicalize().is_ok_and(|path| path == *stack)
                && stack != &user_home
                && stack != std::path::Path::new("/"),
            "Invalid saved stack path {}",
            stack.display()
        );
        eyre::ensure!(
            stack != &config && !stack.starts_with(user_home.join(".config/foundry-opencode")),
            "Managed config directory cannot be a stack"
        );
        let open = stack.join(".opencode");
        eyre::ensure!(
            fs::symlink_metadata(&open)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                || fs::symlink_metadata(&open)
                    .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink()),
            "Unsafe stack config directory {}",
            open.display()
        );
        for name in [
            ".envrc",
            ".opencode/token",
            ".opencode/opencode.json",
            ".opencode/opencode.jsonc",
        ] {
            targets.push(stack.join(name));
        }
        for entry in fs::read_dir(stack)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".envrc.backup-")
            {
                targets.push(entry.path());
            }
        }
        if let Ok(entries) = fs::read_dir(&open) {
            for entry in entries {
                let entry = entry?;
                if [
                    "opencode.json.backup-",
                    "opencode.jsonc.backup-",
                    "token.backup-",
                ]
                .iter()
                .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
                {
                    targets.push(entry.path());
                }
            }
        }
    }
    for project in &projects {
        eyre::ensure!(
            project.canonicalize().is_ok_and(|path| path == *project),
            "Invalid saved RAG project {}",
            project.display()
        );
        let dir = project.join(".opencode");
        eyre::ensure!(
            fs::symlink_metadata(&dir)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                || fs::symlink_metadata(&dir)
                    .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink()),
            "Unsafe RAG directory {}",
            dir.display()
        );
        for name in [
            "rag.json",
            "rag-cli.mjs",
            "patch-opencode-rag.mjs",
            "package.json",
            "package-lock.json",
            ".gitignore",
            "node_modules",
            "rag_db",
            "rag_db_tmp",
            "opencode-rag.log",
            "runtime-overrides.json",
        ] {
            targets.push(dir.join(name));
        }
    }
    let mut repositories: Vec<PathBuf> = managed
        .projects
        .iter()
        .map(|project| project.path.clone())
        .chain(projects.iter().cloned())
        .filter(|path| path.is_dir())
        .collect();
    repositories.sort();
    repositories.dedup();
    let shared: Vec<(PathBuf, PathBuf)> = repositories
        .iter()
        .flat_map(|repo| {
            worktrees::linked(repo)
                .into_iter()
                .map(move |worktree| (repo.clone(), worktree))
        })
        .collect();
    let packages = ["opencode", "opencode-v1", "opencode-v2", "direnv"];
    let npm_packages = cleanup::npm_opencode()?;
    let mut review = format!(
        "PERMANENT FULL WIPE: sessions, credentials, caches and global/legacy stack backups will be deleted. Project config backups remain for manual recovery.\nRemove CLI: {}\nHomebrew: {:?}; OpenChamber cask (if installed).\nGlobal npm: {:?}.\nRemove direnv hooks from .zshrc (also ZDOTDIR) and generated stack .gitignore entries.\nLegacy stacks: {}; managed project configs: {}; RAG projects: {}\nPaths:\n{}\n\nOther files inside project/stack directories are preserved.",
        ops::BIN_PATH,
        packages,
        npm_packages,
        selected.len(),
        managed.projects.len(),
        projects.len(),
        targets
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    if !shared.is_empty() {
        review.push_str(&format!(
            "\nShared .opencode links, RAG indexes and logs in Git worktrees:\n{}",
            shared
                .iter()
                .map(|(_, worktree)| worktree.join(".opencode").display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    let excluded: Vec<String> = repositories
        .iter()
        .filter(|repo| worktrees::excluded(repo))
        .map(|repo| repo.display().to_string())
        .collect();
    if !excluded.is_empty() {
        review.push_str(&format!(
            "\nRemove this installer's .opencode/.openchamber entry from .git/info/exclude (.gitignore lines are kept) in:\n{}",
            excluded.join("\n")
        ));
    }
    if let Some(change) = worktrees::alias_plan(false, None)? {
        review.push_str(&format!("\n{change}"));
    }
    if dry_run {
        review.push_str("\nDry run: no changes.");
    }
    if ui::step_review(
        terminal,
        "Confirm full uninstall",
        &review,
        dry_run,
        true,
        "Wipe everything",
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        return notice(terminal, "Dry-run complete", &review);
    }
    let mut confirmed = false;
    if ui::step_checkbox(
        terminal,
        "Confirm irreversible removal",
        "Sessions, credentials and backups will be permanently deleted. Confirm you have reviewed the paths and wish to continue.",
        "I understand this cannot be undone",
        &mut confirmed,
        false,
    )? != ui::Nav::Next
        || !confirmed
    {
        return Err(Cancelled.into());
    }
    let mut service = Process::new("opencode");
    service.args(["service", "stop"]);
    let mut progress = Progress {
        done: 0,
        total: 1,
        output: Vec::new(),
    };
    let _ = run_limited(
        terminal,
        &mut progress,
        "Stopping OpenCode service",
        service,
        Some(Duration::from_secs(5)),
    );
    let id = Process::new("id").arg("-u").output()?;
    eyre::ensure!(id.status.success(), "Could not determine current user");
    let uid = String::from_utf8(id.stdout)?.trim().parse::<u32>()?;
    let processes = running_targets(uid)?;
    for pid in &processes {
        signal_target(*pid, uid, "-TERM")?;
    }
    if !processes.is_empty() {
        std::thread::sleep(Duration::from_secs(1));
    }
    for pid in running_targets(uid)?
        .into_iter()
        .filter(|pid| processes.contains(pid))
    {
        signal_target(pid, uid, "-KILL")?;
    }
    eyre::ensure!(
        running_targets(uid)?.is_empty(),
        "OpenCode/OpenChamber processes still running; no configs removed"
    );
    remove_direnv(terminal, false)?;
    for name in packages {
        if installed(name, false)? {
            let mut progress = Progress {
                done: 0,
                total: 1,
                output: Vec::new(),
            };
            let mut command = Process::new("brew");
            command.args(["uninstall", name]);
            run(
                terminal,
                &mut progress,
                &format!("Uninstalling {name}"),
                command,
            )?;
        }
    }
    for name in npm_packages {
        let mut progress = Progress {
            done: 0,
            total: 1,
            output: Vec::new(),
        };
        let mut command = Process::new("npm");
        command.args(["uninstall", "-g", &name]);
        run(
            terminal,
            &mut progress,
            &format!("Uninstalling global npm {name}"),
            command,
        )?;
    }
    if installed("openchamber", true)? {
        let mut progress = Progress {
            done: 0,
            total: 1,
            output: Vec::new(),
        };
        let mut command = Process::new("brew");
        command.args(["uninstall", "--cask", "openchamber"]);
        run(terminal, &mut progress, "Uninstalling OpenChamber", command)?;
    }
    for project in &projects {
        remove_rag_entry(project)?;
    }
    for (repo, worktree) in &shared {
        worktrees::unlink(repo, worktree)?;
    }
    for repo in &repositories {
        // A folder that is no longer a Git repository has no exclude entry to remove.
        let _ = worktrees::unexclude(repo);
    }
    worktrees::set_alias(false, None)?;
    for path in targets {
        remove_safe(&path)?;
    }
    for stack in &selected {
        remove_stack_ignores(stack)?;
    }
    for stack in &selected {
        let dir = stack.join(".opencode");
        if fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none()) {
            fs::remove_dir(dir)?;
        }
    }
    for project in &projects {
        let dir = project.join(".opencode");
        if fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none()) {
            fs::remove_dir(dir)?;
        }
    }
    for project in &managed.projects {
        let dir = project.path.join(".opencode");
        if fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_none()) {
            fs::remove_dir(dir)?;
        }
    }
    notice(
        terminal,
        "Uninstall — complete",
        "Removed registered project configs, legacy stack configs, direnv and known OpenCode/OpenChamber user data. Project config backups remain. Installer binary will be removed when you close this screen.",
    )
}

fn remove_rag_entry(project: &std::path::Path) -> eyre::Result<()> {
    for name in ["opencode.jsonc", "opencode.json"] {
        let path = project.join(".opencode").join(name);
        if !path.exists() {
            continue;
        }
        eyre::ensure!(
            fs::symlink_metadata(&path)?.is_file()
                && !fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "Unsafe config {}",
            path.display()
        );
        let root = jsonc_parser::cst::CstRootNode::parse(
            &fs::read_to_string(&path)?,
            &Default::default(),
        )?;
        let object = root.object_value_or_set();
        if let Some(server) = object
            .get("mcp")
            .and_then(|prop| prop.object_value())
            .and_then(|mcp| mcp.get("servers"))
            .and_then(|prop| prop.object_value())
            .and_then(|servers| servers.get("rag"))
        {
            server.remove();
            let text = root.to_string();
            let doc: serde_json::Value =
                jsonc_parser::parse_to_serde_value(&text, &Default::default())?;
            if doc.as_object().is_some_and(|map| {
                map.iter().all(|(key, value)| {
                    key == "$schema"
                        || (key == "mcp"
                            && value["servers"]
                                .as_object()
                                .is_some_and(|servers| servers.is_empty()))
                })
            }) {
                fs::remove_file(path)?;
            } else {
                fs::write(path, text)?;
            }
        }
    }
    Ok(())
}

fn remove_stack_ignores(stack: &std::path::Path) -> eyre::Result<()> {
    let repo = Process::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(stack)
        .output()?;
    if !repo.status.success() {
        return Ok(());
    }
    let root = PathBuf::from(String::from_utf8(repo.stdout)?.trim());
    let ignore = root.join(".gitignore");
    let Ok(original) = fs::read_to_string(&ignore) else {
        return Ok(());
    };
    eyre::ensure!(
        fs::symlink_metadata(&ignore)?.is_file()
            && !fs::symlink_metadata(&ignore)?.file_type().is_symlink(),
        "Unsafe gitignore {}",
        ignore.display()
    );
    let relative = stack
        .canonicalize()?
        .strip_prefix(root.canonicalize()?)?
        .to_string_lossy()
        .to_string();
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{relative}/")
    };
    let archive = format!("/{prefix}.envrc.backup-");
    let token = format!("/{prefix}.opencode/token");
    let token_backup = format!("/{prefix}.opencode/token.backup-");
    let updated = original
        .lines()
        .filter(|line| {
            *line != token && !line.starts_with(&archive) && !line.starts_with(&token_backup)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let updated = if updated.is_empty() {
        updated
    } else {
        format!("{updated}\n")
    };
    if updated != original {
        fs::write(ignore, updated)?;
    }
    Ok(())
}

fn reset_targets(list: &str, uid: u32) -> Vec<u32> {
    list.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let owner = parts.next()?.parse::<u32>().ok()?;
            let name = parts.collect::<Vec<_>>().join(" ");
            let name = name.rsplit('/').next()?;
            (owner == uid
                && pid != std::process::id()
                && (name == "opencode"
                    || name == "OpenCode"
                    || name.starts_with("OpenCode Helper")
                    || name == "OpenChamber"
                    || name == "openchamber"
                    || name.starts_with("OpenChamber Helper")))
            .then_some(pid)
        })
        .collect()
}

fn running_targets(uid: u32) -> eyre::Result<Vec<u32>> {
    let output = Process::new("ps")
        .args(["-axo", "pid=,uid=,comm="])
        .output()?;
    eyre::ensure!(
        output.status.success(),
        "Could not inspect running processes"
    );
    Ok(reset_targets(&String::from_utf8(output.stdout)?, uid))
}

fn signal_target(pid: u32, uid: u32, signal: &str) -> eyre::Result<()> {
    if !running_targets(uid)?.contains(&pid) {
        return Ok(());
    }
    let output = Process::new("kill")
        .args([signal, &pid.to_string()])
        .output()?;
    eyre::ensure!(
        output.status.success() || !running_targets(uid)?.contains(&pid),
        "Could not stop process {pid}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn clear_cache(path: &std::path::Path) -> eyre::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            eyre::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "{} is not a real cache directory; not removing it",
                path.display()
            );
            fs::remove_dir_all(path)?;
            Ok(true)
        }
    }
}

fn reset(terminal: &mut DefaultTerminal, dry_run: bool) -> eyre::Result<()> {
    let cache = home()?.join(".cache/opencode");
    let details = format!(
        "Stop the current user's OpenCode service and OpenCode/OpenChamber processes, then remove {} if it exists. Active sessions will be interrupted. Configs, service registration, and session data are preserved.",
        cache.display()
    );
    if ui::step_review(terminal, "Review reset", &details, dry_run, true, "Reset")? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        return notice(terminal, "Dry-run complete", &details);
    }
    let mut progress = Progress {
        done: 0,
        total: 3,
        output: Vec::new(),
    };
    let mut warning = String::new();
    let mut stop = Process::new("opencode");
    stop.args(["service", "stop"]);
    if let Err(error) = run_limited(
        terminal,
        &mut progress,
        "Stopping OpenCode service",
        stop,
        Some(Duration::from_secs(5)),
    ) {
        warning = format!("Service stop did not complete ({error}); stopping processes directly. ");
        progress.line(warning.as_bytes());
        progress.done += 1;
    }
    let id = Process::new("id").arg("-u").output()?;
    eyre::ensure!(id.status.success(), "Could not determine current user");
    let uid = String::from_utf8(id.stdout)?.trim().parse::<u32>()?;
    progress.show(terminal, "Stopping OpenCode and OpenChamber processes")?;
    let targets = running_targets(uid)?;
    for pid in &targets {
        signal_target(*pid, uid, "-TERM")?;
        progress.line(format!("Sent TERM to {pid}").as_bytes());
    }
    if !targets.is_empty() {
        std::thread::sleep(Duration::from_secs(1));
    }
    for pid in running_targets(uid)?
        .into_iter()
        .filter(|pid| targets.contains(pid))
    {
        signal_target(pid, uid, "-KILL")?;
        progress.line(format!("Sent KILL to {pid}").as_bytes());
    }
    eyre::ensure!(
        running_targets(uid)?.is_empty(),
        "OpenCode or OpenChamber is still running; cache was not cleared"
    );
    progress.done += 1;
    progress.show(terminal, "Clearing OpenCode cache")?;
    let removed = clear_cache(&cache)?;
    progress.done += 1;
    progress.show(terminal, "Reset complete")?;
    notice(
        terminal,
        "Reset — complete",
        &format!(
            "{warning}Stopped {} process(es). Cache {}.",
            targets.len(),
            if removed { "removed" } else { "not present" }
        ),
    )
}

struct InstallPlan {
    config: PathBuf,
    backup: PathBuf,
    existing: bool,
    with_chamber: bool,
    catalog: projects::Catalog,
    cleanup: cleanup::Inventory,
}

fn collect_install_plan(
    terminal: &mut DefaultTerminal,
    dry_run: bool,
) -> eyre::Result<Option<InstallPlan>> {
    let user_home = home()?;
    let config = user_home.join(".config/opencode");
    let existing = fs::symlink_metadata(&config).is_ok();
    let backup = ops::backup_path(&config);
    let catalog = projects::collect(terminal, &user_home, dry_run, None)?;
    let mut with_chamber = false;
    if ui::step_checkbox(
        terminal,
        "Optional application",
        "OpenCode is required. OpenChamber is optional.",
        "Install OpenChamber",
        &mut with_chamber,
        false,
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if existing {
        let mut confirmed = false;
        if ui::step_checkbox(
            terminal,
            "Existing global configuration",
            &format!(
                "{} will be backed up to {} before replacement.",
                config.display(),
                backup.display()
            ),
            "I understand",
            &mut confirmed,
            false,
        )? != ui::Nav::Next
            || !confirmed
        {
            return Err(Cancelled.into());
        }
    }
    let inventory = busy(terminal, "Inspecting installed tools", {
        let user_home = user_home.clone();
        move || cleanup::inspect(&user_home, with_chamber)
    })?;
    let mut review = format!(
        "Install/upgrade OpenCode{}; clone latest global configuration{}; download and verify the installer release. Remove direnv and its shell hooks.\n{} selected repositories:",
        if with_chamber { " and OpenChamber" } else { "" },
        if existing {
            format!(" (backup: {})", backup.display())
        } else {
            String::new()
        },
        catalog.projects.len()
    );
    if inventory.brew_v2 {
        review.push_str("\nMigrate Homebrew opencode-v2 to the standard V2 formula.");
    }
    for name in &inventory.brew_v1 {
        review.push_str(&format!("\nUninstall V1 Homebrew package: {name}"));
    }
    for name in &inventory.npm_v1 {
        review.push_str(&format!("\nUninstall global V1 npm package: {name}"));
    }
    for path in &inventory.files {
        review.push_str(&format!(
            "\nMove older OpenCode executable to Trash: {}",
            path.display()
        ));
    }
    if inventory.chamber {
        review.push_str("\nMove non-Homebrew /Applications/OpenChamber.app to Trash if OpenChamber was selected.");
    }
    for path in &inventory.unknown {
        review.push_str(&format!(
            "\nUnrecognized executable (leave untouched): {}",
            path.display()
        ));
    }
    for project in &catalog.projects {
        let stack = catalog
            .choices()
            .into_iter()
            .find(|item| item.id == project.stack)
            .ok_or_else(|| eyre::eyre!("Unknown stack for {}", project.path.display()))?;
        review.push_str(&format!(
            "\n{} => {} ({}){}; back up and replace existing project config files: {}",
            project.path.display(),
            stack.name,
            stack.url,
            if project.rag { "; initialize RAG" } else { "" },
            projects::config_paths(&project.path)
                .iter()
                .filter(|path| path.exists())
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for previous in projects::Catalog::inventory(&user_home)?.projects {
        if !catalog
            .projects
            .iter()
            .any(|project| project.path == previous.path)
        {
            review.push_str(&format!("\nDeselect {}: assignment removed; previous config is preserved for manual removal.", previous.path.display()));
        }
    }
    if let Some(change) = worktrees::alias_plan(catalog.sync_alias, None)? {
        review.push_str(&format!("\n{change}"));
    }
    review.push_str("\nOther .opencode files and existing legacy stack configs are preserved. Tokens are redacted.");
    if dry_run {
        review.push_str("\nDry run: no files, packages, browser or credentials changed.");
    }
    if ui::step_review(
        terminal,
        "Review installation",
        &review,
        dry_run,
        false,
        "Install",
    )? != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        notice(terminal, "Dry-run complete", &review)?;
        return Ok(None);
    }
    Ok(Some(InstallPlan {
        config,
        backup,
        existing,
        with_chamber,
        catalog,
        cleanup: inventory,
    }))
}

/// Upgrades an earlier installer `rag.json` in place, so RAG stays enabled, then reindexes so the
/// new exclusions apply. A failed reindex keeps the previous index and RAG enabled.
fn upgrade_rag(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    project: &std::path::Path,
    url: &str,
    token: &str,
) -> eyre::Result<Option<String>> {
    if !rag::upgrade_config(project, url)? {
        return Ok(None);
    }
    let mut node = Process::new("node");
    node.args([".opencode/rag-cli.mjs", "index"])
        .current_dir(project)
        .env("OPENAI_API_KEY", token);
    Ok(Some(
        match run(
            terminal,
            progress,
            "Reindexing with updated RAG settings",
            node,
        ) {
            Ok(()) => format!(
                "Updated RAG settings for {} (skips build caches and generated SDKs) and reindexed",
                project.display()
            ),
            Err(error) => format!(
                "Updated RAG settings for {}; reindex failed ({error}). RAG stays enabled with the previous index; retry with node .opencode/rag-cli.mjs index",
                project.display()
            ),
        },
    ))
}

/// Keeps local setup folders out of Git for a configured repository and links its worktrees.
fn share_worktrees(repo: &std::path::Path) -> Vec<String> {
    worktrees::share(repo).unwrap_or_else(|error| {
        vec![format!(
            "Worktree setup incomplete for {}: {error}",
            repo.display()
        )]
    })
}

fn configure_projects(
    terminal: &mut DefaultTerminal,
    progress: &mut Progress,
    catalog: &projects::Catalog,
) -> eyre::Result<Vec<String>> {
    let mut result = Vec::new();
    let user_home = home()?;
    for project in &catalog.projects {
        let chosen = catalog
            .choices()
            .into_iter()
            .find(|item| item.id == project.stack)
            .ok_or_else(|| eyre::eyre!("Unknown stack"))?;
        progress.show(
            terminal,
            &format!("Configuring repository {}", project.path.display()),
        )?;
        let upgraded = if project.rag {
            upgrade_rag(
                terminal,
                progress,
                &project.path,
                &chosen.url,
                &catalog.tokens[&chosen.id],
            )?
        } else {
            None
        };
        let existing_rag = project.rag && rag::ready(&project.path, &chosen.url);
        let backups = projects::write_project(&user_home, project, chosen)?;
        result.push(format!(
            "Configured {} for {} ({} configs backed up)",
            project.path.display(),
            chosen.name,
            backups.len()
        ));
        result.extend(upgraded);
        if existing_rag {
            register_rag_project(&user_home, &project.path)?;
            result.push(format!(
                "Reused existing RAG index for {}",
                project.path.display()
            ));
        } else if project.rag {
            let token = catalog.tokens[&chosen.id].clone();
            let path = project.path.clone();
            let url = chosen.url.clone();
            match rag::setup(&path, &url, false, |label, mut command| {
                command.env("OPENAI_API_KEY", &token);
                run(terminal, progress, label, command)
            }) {
                Ok(message) => {
                    register_rag_project(&user_home, &path)?;
                    result.push(message);
                }
                Err(error) => {
                    result.push(format!("RAG incomplete for {}: {error}", path.display()))
                }
            }
        }
        result.extend(share_worktrees(&project.path));
        progress.done += 1;
    }
    Ok(result)
}

fn install(
    terminal: &mut DefaultTerminal,
    dry_run: bool,
) -> eyre::Result<Option<ops::StagedBinary>> {
    let Some(plan) = collect_install_plan(terminal, dry_run)? else {
        return Ok(None);
    };
    let InstallPlan {
        config,
        backup,
        existing,
        with_chamber,
        catalog,
        cleanup: inventory,
    } = plan;
    let (binary, release_version) = busy(
        terminal,
        "Downloading and verifying latest installer",
        ops::stage_latest_binary,
    )?;
    let mut progress = Progress {
        done: 0,
        total: 4
            + usize::from(with_chamber)
            + catalog.projects.len()
            + catalog
                .projects
                .iter()
                .filter(|project| project.rag)
                .count()
                * 2
            + inventory.brew_v1.len()
            + inventory.npm_v1.len()
            + inventory.files.len()
            + usize::from(inventory.brew_v2)
            + usize::from(inventory.chamber),
        output: Vec::new(),
    };
    brew_update(terminal, &mut progress)?;
    busy(
        terminal,
        "Verifying OpenCode formula",
        cleanup::latest_is_v2,
    )?;
    let home_for_check = home()?;
    eyre::ensure!(
        busy(terminal, "Rechecking installed tools", move || {
            cleanup::inspect(&home_for_check, with_chamber)
        })? == inventory,
        "Install inventory changed after the Homebrew refresh; review the install again before removing anything"
    );
    // Decide before removing anything: a running optional app must never block required setup.
    let plan_with_chamber = with_chamber;
    let with_chamber = with_chamber && chamber_available(terminal);
    let mut trashed = Vec::new();
    for name in &inventory.brew_v1 {
        let mut command = Process::new("brew");
        command.args(["uninstall", name]);
        run(
            terminal,
            &mut progress,
            &format!("Removing V1 Homebrew package {name}"),
            command,
        )?;
    }
    for name in &inventory.npm_v1 {
        let mut command = Process::new("npm");
        command.args(["uninstall", "-g", name]);
        run(
            terminal,
            &mut progress,
            &format!("Removing V1 npm package {name}"),
            command,
        )?;
    }
    for path in &inventory.files {
        progress.show(terminal, &format!("Moving {} to Trash", path.display()))?;
        trashed.push(cleanup::move_to_trash(path, &home()?)?);
        progress.done += 1;
    }
    if inventory.brew_v2 {
        let mut command = Process::new("brew");
        command.args(["uninstall", "opencode-v2"]);
        run(
            terminal,
            &mut progress,
            "Migrating Homebrew opencode-v2",
            command,
        )?;
    }
    if let Err(error) = brew_package(terminal, &mut progress, "opencode", false) {
        if inventory.brew_v2 {
            let mut restore = Process::new("brew");
            restore.args(["install", "opencode-v2"]);
            if let Err(restore_error) = run(
                terminal,
                &mut progress,
                "Restoring previous Homebrew OpenCode",
                restore,
            ) {
                eyre::bail!("{error}; restore of opencode-v2 also failed: {restore_error}");
            }
        }
        return Err(error);
    }
    let version = Process::new("opencode").arg("--version").output();
    if !matches!(version, Ok(ref output) if output.status.success() && cleanup::reports_v2(&String::from_utf8_lossy(&output.stdout)))
    {
        notice(
            terminal,
            "PATH warning",
            "Homebrew installed OpenCode, but another opencode executable is first on PATH. Check your shell PATH after installation.",
        )?;
    }
    if with_chamber {
        let safe_to_move = inventory.chamber && chamber_available(terminal);
        let mut chamber_backup = None;
        if safe_to_move {
            progress.show(terminal, "Moving non-Homebrew OpenChamber.app to Trash")?;
            let backup = cleanup::move_to_trash(
                std::path::Path::new("/Applications/OpenChamber.app"),
                &home()?,
            )?;
            chamber_backup = Some(backup.clone());
            trashed.push(backup);
            progress.done += 1;
        }
        if !inventory.chamber || safe_to_move {
            if !optional_chamber(terminal, &mut progress, false)
                && let Some(backup) = chamber_backup
            {
                let app = std::path::Path::new("/Applications/OpenChamber.app");
                restore_chamber(&backup, app)?;
                trashed.retain(|path| path != &backup);
            }
        } else {
            progress.done += 2; // skipped the app move and optional cask installation
        }
    } else if inventory.chamber {
        progress.done += 2;
    } else if plan_with_chamber {
        progress.done += 1;
    }
    fs::create_dir_all(config.parent().expect("config has parent"))?;
    let staged = ops::backup_path(&config);
    let make_clone = || {
        let mut clone = Process::new("git");
        clone
            .args([
                "clone",
                "--progress",
                "--depth",
                "1",
                "--branch",
                "main",
                ops::REMOTE,
            ])
            .arg(&staged);
        clone
    };
    if let Err(error) = run_interactive_git(
        terminal,
        &mut progress,
        "Cloning latest configs",
        make_clone(),
        make_clone(),
        Some(&staged),
    ) {
        if staged.exists() {
            fs::remove_dir_all(&staged)?;
        }
        return Err(error);
    }
    if let Some(default) = &catalog.default
        && let Some(key) = catalog.tokens.get(&default.id)
        && let Err(error) = ops::prepare_global(&staged, &default.url, key)
    {
        fs::remove_dir_all(&staged)?;
        return Err(error);
    }
    ops::replace_config(&config, &staged, existing.then_some(backup.as_path()))?;
    catalog.save(&home()?)?;
    let mut migration = configure_projects(terminal, &mut progress, &catalog)?;
    migration.extend(worktrees::set_alias(catalog.sync_alias, None)?);
    remove_direnv(terminal, false)?;
    progress.show(terminal, "Installation complete")?;
    notice(
        terminal,
        "Install — complete",
        &format!(
            "Installation finished.{}\nSelected repositories have their own stack configuration.{}\nRelease {release_version} will replace /usr/local/bin/foundry-opencode when you leave this screen.{}",
            if existing {
                format!(" Backup: {}", backup.display())
            } else {
                String::new()
            },
            if migration.is_empty() {
                String::new()
            } else {
                format!("\n{}", migration.join("\n"))
            },
            if trashed.is_empty() {
                String::new()
            } else {
                format!(
                    "\nMoved older applications to Trash:\n{}",
                    trashed
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            },
        ),
    )?;
    Ok(Some(binary))
}

fn update(
    terminal: &mut DefaultTerminal,
    dry_run: bool,
    yes: bool,
    quick_summary: &mut Option<String>,
) -> eyre::Result<Option<ops::StagedBinary>> {
    let user_home = home()?;
    let mut accept = false;
    let quick = if yes {
        true
    } else {
        if ui::step_checkbox(
            terminal,
            "Update options",
            "Keep unchecked to review or edit stacks, repositories and RAG choices.",
            "Accept all loaded defaults and update",
            &mut accept,
            true,
        )? != ui::Nav::Next
        {
            return Err(Cancelled.into());
        }
        accept
    };
    let catalog = if quick {
        projects::Catalog::saved(&user_home)?
    } else {
        projects::collect(terminal, &user_home, dry_run, None)?
    };
    let config = user_home.join(".config/opencode");
    let existing = fs::symlink_metadata(&config).is_ok();
    let backup = ops::backup_path(&config);
    let mut details = format!(
        "Upgrade Homebrew OpenCode (OpenChamber if installed), download and verify the latest installer release, clone global configs{}, and remove direnv plus its shell hooks.",
        if existing {
            format!(" (backup: {})", backup.display())
        } else {
            String::new()
        }
    );
    for project in &catalog.projects {
        let stack = catalog
            .choices()
            .into_iter()
            .find(|item| item.id == project.stack)
            .ok_or_else(|| eyre::eyre!("Unknown stack"))?;
        details.push_str(&format!(
            "\n{}: {} ({}){}; replace existing config with backups: {}",
            project.path.display(),
            stack.name,
            stack.url,
            if project.rag { "; initialize RAG" } else { "" },
            projects::config_paths(&project.path)
                .iter()
                .filter(|path| path.exists())
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for previous in projects::Catalog::inventory(&user_home)?.projects {
        if !catalog
            .projects
            .iter()
            .any(|project| project.path == previous.path)
        {
            details.push_str(&format!("\nDeselect {}: assignment removed; previous config is preserved for manual removal.", previous.path.display()));
        }
    }
    if let Some(change) = worktrees::alias_plan(catalog.sync_alias, None)? {
        details.push_str(&format!("\n{change}"));
    }
    details.push_str(
        "\nExisting parent stack configs and unrelated .opencode files remain. Tokens redacted.",
    );
    if dry_run {
        details.push_str("\nDry run: no files, packages, browser or credentials changed.");
    }
    if !quick
        && ui::step_review(terminal, "Review update", &details, dry_run, true, "Update")?
            != ui::Nav::Next
    {
        return Err(Cancelled.into());
    }
    if dry_run {
        if quick {
            *quick_summary = Some(details);
        } else {
            notice(terminal, "Dry-run complete", &details)?;
        }
        return Ok(None);
    }
    let (binary, release_version) = busy(
        terminal,
        "Downloading and verifying latest installer",
        ops::stage_latest_binary,
    )?;
    let chamber = installed("openchamber", true)?;
    let mut progress = Progress {
        done: 0,
        total: 3
            + usize::from(chamber)
            + catalog.projects.len()
            + catalog
                .projects
                .iter()
                .filter(|project| project.rag)
                .count()
                * 2,
        output: Vec::new(),
    };
    brew_update(terminal, &mut progress)?;
    brew_package(terminal, &mut progress, "opencode", false)?;
    if chamber {
        let _ = optional_chamber(terminal, &mut progress, quick);
    }
    let staged = ops::backup_path(&config);
    let make_clone = || {
        let mut git = Process::new("git");
        git.args([
            "clone",
            "--progress",
            "--depth",
            "1",
            "--branch",
            "main",
            ops::REMOTE,
        ])
        .arg(&staged);
        git
    };
    if let Err(error) = run_interactive_git(
        terminal,
        &mut progress,
        "Cloning latest configs",
        make_clone(),
        make_clone(),
        Some(&staged),
    ) {
        if staged.is_dir() {
            fs::remove_dir_all(&staged)?;
        }
        return Err(error);
    }
    if let Some(default) = &catalog.default {
        let token = &catalog.tokens[&default.id];
        if let Err(error) = ops::prepare_global(&staged, &default.url, token) {
            fs::remove_dir_all(&staged)?;
            return Err(error);
        }
    }
    ops::replace_config(&config, &staged, existing.then_some(backup.as_path()))?;
    catalog.save(&user_home)?;
    let mut results = configure_projects(terminal, &mut progress, &catalog)?;
    results.extend(worktrees::set_alias(catalog.sync_alias, None)?);
    remove_direnv(terminal, false)?;
    let result = format!(
        "Repositories configured. Release {release_version} {}.\n{}",
        if quick {
            "verified; installing the CLI next"
        } else {
            "will replace the CLI in /usr/local/bin when you leave this screen"
        },
        results.join("\n")
    );
    if quick {
        let warnings = progress
            .output
            .iter()
            .filter(|line| line.starts_with("OpenChamber"))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        *quick_summary = Some(format!("{details}\n{result}\n{warnings}"));
    } else {
        notice(terminal, "Update — complete", &result)?;
    }
    Ok(Some(binary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_is_a_distinct_outcome() {
        let error: eyre::Report = Cancelled.into();
        assert!(error.is::<Cancelled>());
    }

    #[test]
    fn no_arguments_select_install() {
        let args = Args::try_parse_from(["foundry-opencode"]).unwrap();
        assert!(matches!(
            args.command.unwrap_or(Mode::Install),
            Mode::Install
        ));
    }

    #[test]
    fn dry_run_works_before_and_after_commands() {
        for args in [
            vec!["foundry-opencode", "--dry-run"],
            vec!["foundry-opencode", "--dry-run", "init"],
            vec!["foundry-opencode", "init", "--dry-run"],
            vec!["foundry-opencode", "update", "--dry-run"],
            vec!["foundry-opencode", "init-rag", "--dry-run"],
            vec!["foundry-opencode", "reset", "--dry-run"],
            vec!["foundry-opencode", "uninstall", "--dry-run"],
        ] {
            assert!(Args::try_parse_from(args).unwrap().dry_run);
        }
    }

    #[test]
    fn update_defaults_flags_are_update_only() {
        for args in [
            vec!["foundry-opencode", "update", "--yes"],
            vec!["foundry-opencode", "update", "-y"],
            vec!["foundry-opencode", "--dry-run", "update", "-y"],
            vec!["foundry-opencode", "update", "--yes", "--dry-run"],
        ] {
            let args = Args::try_parse_from(args).unwrap();
            assert!(matches!(args.command, Some(Mode::Update { yes: true })));
        }
        assert!(matches!(
            Args::try_parse_from(["foundry-opencode", "update"])
                .unwrap()
                .command,
            Some(Mode::Update { yes: false })
        ));
        assert!(Args::try_parse_from(["foundry-opencode", "install", "--yes"]).is_err());
    }

    #[test]
    fn reset_selects_only_current_users_named_processes_and_cache() {
        let list = " 123 501 /usr/local/bin/opencode\n 124 502 opencode\n 125 501 /Applications/OpenChamber.app/Contents/MacOS/OpenChamber\n 126 501 OpenChamber Helper (Renderer)\n 127 501 unrelated-opencode\n 128 501 OpenCode Helper (GPU)\n";
        assert_eq!(reset_targets(list, 501), [123, 125, 126, 128]);
        let root = ops::backup_path(&std::env::temp_dir().join("reset-test"));
        fs::create_dir(&root).unwrap();
        let cache = root.join("opencode");
        assert!(!clear_cache(&cache).unwrap());
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("tmp"), "cached").unwrap();
        #[cfg(unix)]
        {
            let link = root.join("linked-cache");
            std::os::unix::fs::symlink(&cache, &link).unwrap();
            assert!(clear_cache(&link).is_err());
            assert!(cache.join("tmp").exists());
        }
        assert!(clear_cache(&cache).unwrap());
        assert!(!cache.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_output_is_bounded_and_strips_controls() {
        let mut progress = Progress {
            done: 0,
            total: 1,
            output: Vec::new(),
        };
        progress.line(b"first\x1b[31m\n");
        assert_eq!(progress.output, ["first[31m"]);
        for _ in 0..220 {
            progress.line(b"next");
        }
        assert_eq!(progress.output.len(), 200);
        let (tx, rx) = mpsc::channel();
        stream_output(&b"git 10%\rgit 20%\nfinished"[..], tx);
        assert_eq!(
            rx.into_iter().collect::<Vec<_>>(),
            [
                b"git 10%".to_vec(),
                b"git 20%".to_vec(),
                b"finished".to_vec()
            ]
        );
    }

    #[test]
    fn unknown_chamber_status_is_not_treated_as_not_running() {
        assert!(chamber_status(Some(0)).unwrap());
        assert!(!chamber_status(Some(1)).unwrap());
        assert!(chamber_status(Some(2)).is_err());
        assert!(chamber_status(None).is_err());
    }

    #[test]
    fn chamber_restore_never_replaces_an_existing_app() {
        let root = ops::backup_path(&std::env::temp_dir().join("chamber-restore-test"));
        fs::create_dir(&root).unwrap();
        let backup = root.join("backup");
        let app = root.join("app");
        fs::write(&backup, "original").unwrap();
        fs::write(&app, "new").unwrap();
        assert!(restore_chamber(&backup, &app).is_err());
        assert_eq!(fs::read_to_string(&backup).unwrap(), "original");
        fs::remove_file(&app).unwrap();
        restore_chamber(&backup, &app).unwrap();
        assert_eq!(fs::read_to_string(&app).unwrap(), "original");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn token_page_is_on_the_validated_host() {
        assert_eq!(
            token_page("https://example.palantirfoundry.com"),
            "https://example.palantirfoundry.com/workspace/settings/tokens"
        );
        assert!(!ops::valid_url("https://example.palantirfoundry.com/other"));
    }

    #[test]
    fn rag_uninstall_preserves_unrelated_jsonc_and_ignores() {
        let root = ops::backup_path(&std::env::temp_dir().join("rag-uninstall-test"));
        fs::create_dir_all(root.join("stack/.opencode")).unwrap();
        fs::write(root.join("stack/.opencode/opencode.jsonc"), "{\n  // keep this comment\n  \"model\": \"other\",\n  \"mcp\": {\"servers\": {\"rag\": {\"type\": \"local\"}}}\n}\n").unwrap();
        remove_rag_entry(&root.join("stack")).unwrap();
        let text = fs::read_to_string(root.join("stack/.opencode/opencode.jsonc")).unwrap();
        assert!(text.contains("keep this comment"));
        assert!(!text.contains("\"rag\""));
        assert!(
            Process::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(
            root.join(".gitignore"),
            "/stack/.opencode/token\n/stack/.envrc.backup-123\n/keep\n",
        )
        .unwrap();
        remove_stack_ignores(&root.join("stack")).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "/keep\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_stack_paths_and_removal_stay_bounded() {
        let root = ops::backup_path(&std::env::temp_dir().join("stack-registry-test"));
        fs::create_dir(&root).unwrap();
        let stack = root.join("stack");
        fs::create_dir(&stack).unwrap();
        save_paths(&stack_registry(&root), std::slice::from_ref(&stack)).unwrap();
        assert_eq!(load_stacks(&root).unwrap(), [stack]);
        let keep = root.join("keep");
        fs::write(&keep, "preserve").unwrap();
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&keep, &link).unwrap();
            assert!(remove_safe(&link).is_err());
            assert!(keep.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
