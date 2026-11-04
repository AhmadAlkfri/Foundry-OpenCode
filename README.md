# OpenCode v2 Config

OpenCode configuration for Palantir. Requires OpenCode v2; The configuration uses the native v2 format and must not be loaded by v1.

# Install

## WARNING
The installer backs up and replaces `~/.config/opencode` and the OpenCode config files in each selected Git repository after review. It does not delete unrelated `.opencode` files or repository contents. It removes confirmed V1 OpenCode packages before installing Homebrew OpenCode V2. OpenChamber is optional; existing sessions and app data are preserved.

## Installation
Download the [latest macOS release](https://github.com/DemiLovato69/Foundry-OpenCode/releases/latest) for Apple Silicon (`aarch64-apple-darwin`) or Intel (`x86_64-apple-darwin`), extract the binary, and run it from the extracted location. The installer offers to copy itself into `/usr/local/bin` for future use. Homebrew, Git, and HTTPS access to this public repository are required; a GitHub SSH key is not. OpenChamber is optional.

**macOS security:** The release binaries are not signed or notarized. Check your downloaded archive with `shasum -a 256 foundry-opencode-<arch>.tar.gz` and compare it to the matching line in the release's `SHA256SUMS`. macOS may block the first launch; after attempting to run the binary, go to **System Settings → Privacy & Security → Open Anyway** only if you trust the release. Do not disable Gatekeeper or remove the quarantine attribute globally.

Run `foundry-opencode` (equivalent to `foundry-opencode install`). The explorer is a single expandable directory tree. Arrows or a single click highlight without opening; Enter expands or collapses, Right expands, Left collapses or selects the parent, Space marks a folder, and C continues. Tab switches between the tree and its search/path field. Press S to filter visible directory names, or `/` to enter an absolute path; `~/`, `./`, and `../` paths also work. Path matches appear in the same tree; Enter opens a complete or arrow-highlighted path. Dot-prefixed and macOS Finder-invisible folders are hidden by default; type their full path to open them. Previously marked folders remain marked even when hidden. Inaccessible directories show an error without losing selections. No central projects directory or direnv is required. Existing direnv Zsh hooks, whitelist settings and installation are removed.

Before uninstalling a legacy OpenCode installation, the installer verifies that the standard Homebrew `opencode` formula targets V2. Unrecognized executables are reported but not deleted or allowed to block config setup. A Homebrew-managed `opencode-v2` is migrated to the standard V2 formula after confirmation.

Use the mouse or keyboard in the wizard. First configure the global default and named Foundry stacks, then mark Git repository roots in the tree. Assign a stack and optionally RAG to each selected repo. Those choices are saved for updates. Installing the CLI to `/usr/local/bin/foundry-opencode` may request an administrator password after closing the TUI.

The optional global default also provides a fallback outside selected repositories. Each selected repo gets its own `.opencode/opencode.json`, referencing a private token under `~/.config/foundry-opencode/tokens/`; no token is written into a repo config. Known old stack credentials can be imported without executing `.envrc`. Legacy parent-stack configs and `.envrc` files are left in place for review: they may still affect unselected repositories. Tracked `.envrc` secrets require Git cleanup and token rotation.

## After Install

The installer provides many commands to make developer experience better.

- Configure one Git repository
*Run this command from the repository root*
`foundry-opencode init`

At the token prompt, select **Open Foundry token page** to open `https://<host>/workspace/settings/tokens`, create your token there, then paste it into the masked prompt. Opening the browser is optional and never happens just by entering the host.

- Update everything
`foundry-opencode update`

Update reopens the stack catalog and saved repo assignments. After the update-options screen, edit credentials in the stack catalog, add or remove projects in the tree, then review the backups and per-repo RAG choices. An existing repo config is replaced, not merged; unrelated `.opencode` files are preserved. The globally configured default model still applies unless the project overrides it.

To keep all saved stacks, tokens, repository assignments and per-repo RAG choices, check **Accept all loaded defaults and update** on update's first screen. Or run `foundry-opencode update -y` (`--yes`) to skip the wizard entirely; `update -y --dry-run` previews the saved update without changing anything. Missing or invalid saved settings stop the shortcut before any update. The command prints the redacted summary and finishes without an extra confirmation; Git authentication or administrator prompts may still appear.

Update downloads the latest published macOS installer binary for your CPU, checks its release SHA-256 and version, and replaces `/usr/local/bin/foundry-opencode` after the wizard closes. Download or verification failures leave the existing binary untouched. The global config is cloned independently from `main`.

Palantir MCP runs `npx` with a command-local `--registry=https://registry.npmjs.org` so project `.npmrc` files can continue to use their Foundry registry. Install/update generate this launcher in the per-project configs and optional global fallback. After deployment, use `/mcps` to reconnect `palantir` and verify a tool call from the affected repo.

If Anthropic fails with `INVALID_ARGUMENT` and `unrecognizedProperty=cache_control`, update the installed config: the compatibility plugin removes unsupported prompt-cache markers from Foundry-bound requests. Use `/reload` if needed, then retry; switching Anthropic models does not address this error.

Host, token and path fields support mouse cursor positioning, arrows, Home/End, Delete/Backspace, and Ctrl+U or terminal-translated Cmd+Delete to delete to the beginning. Some terminals intercept Cmd shortcuts; use Ctrl+U there. Tokens stay masked.

If an older project reports `.opencode/plugins/rag-plugin.js` is not a V2 plugin, run `foundry-opencode repair-plugins` from that project (or `--dry-run` first). It backs up only the exact known V1 wrapper without removing the RAG index; edited plugins are left alone. Reindex manually with `node .opencode/rag-cli.mjs index` where that helper exists. If it is missing, review the project config and run `init-rag` to set up V2 RAG MCP support.
The incompatible `context-mode` plugin is disabled by the global config. A missing `palantir-mcp` executable under npm's cache is a separate npm/MCP installation failure; inspect `opencode mcp list` and repair that package's installation rather than deleting OpenCode sessions or configuration.

- Work in Git worktrees
*Run this command from any checkout of the repository*
`foundry-opencode sync-worktrees`

Select only a repository's main checkout; the installer refuses its worktrees as separate projects. Each worktree gets a real `.opencode` folder whose config, scripts and dependencies are links to the main checkout's, so editing the config anywhere edits it everywhere. Its RAG index (`rag_db`), logs and backups stay its own. Install/update add `.opencode` to the repository's local, never-pushed `.git/info/exclude` (unless the repository tracks files under `.opencode`), which hides it in every worktree and branch.

`sync-worktrees` fetches `origin`, then for every worktree: moves it to the latest default branch (fast-forwards the checkout on that branch, moves detached ones), links any missing shared `.opencode` entries, and runs an incremental `node .opencode/rag-cli.mjs index` (output in `.opencode/sync-index.log`). Worktrees on another branch, with uncommitted changes, or with unpushed commits on the default branch are reported as `FAILED` and not moved, but are still linked and reindexed. It exits non-zero if anything failed; `--dry-run sync-worktrees` previews without fetching or changing anything. A worktree's first index is a full build; later runs only re-embed changed files.

Install/update can also add the opt-in Git alias `git sync-worktrees` for the same command. An existing different alias is replaced only after it is shown in the review. Uninstall removes worktree links and indexes, the exclude entry, and the alias if it is still the installer's.

New RAG setups also skip `.nx`, `.palantir`, `osdk-output` and `generated-imports`, which hold build caches, local ontology state and generated SDKs. Update upgrades an earlier installer `rag.json` with a backup; a hand-edited one is left alone.

- Remove installed tools, configs, credentials and session data permanently
`foundry-opencode uninstall` (review first with `foundry-opencode uninstall --dry-run`)

- Re-run the install process
*This backs up and replaces previous configs after confirmation*
`foundry-opencode install`

- Show the binary version or example commands
`foundry-opencode version` / `foundry-opencode usage`

Preview operations without writing files or opening a browser: `foundry-opencode init --dry-run`, `foundry-opencode update --dry-run`, `foundry-opencode uninstall --dry-run`, or `foundry-opencode --dry-run install`.

Releases are created automatically from successful merges to `main` (including changes to these configs); each release contains macOS Apple Silicon and Intel binaries and checksums.


## Configuration layout

OpenCode loads `~/.config/opencode/opencode.jsonc` as a global base and each selected
repository's `.opencode/opencode.json` as a more specific override. The installer
stores credentials in private `~/.config/foundry-opencode/tokens/` files, not in
`.envrc` or project JSON. Update backs up and replaces the global checkout and
each selected repository config. Uninstall removes registered project configs,
generated RAG files and OpenCode/OpenChamber user data; project directories and
original config backups remain for manual recovery.

New model selectors: `palantir-openai/gpt-6-1-sol`, `palantir-google/gemini-4-argon`,
and `palantir-google/gemini-3-8-flash`. Gemini uses Foundry's beta Google proxy;
your enrollment must enable the models. Its streaming requests require `alt=sse`
and Foundry bearer authentication, not a Google API key header.
