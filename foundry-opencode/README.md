# foundry-opencode

macOS installer for OpenCode V2 and Foundry. Requires Homebrew, Git, and HTTPS access to the public config repository. Release binaries are not signed or notarized; verify each archive against the release `SHA256SUMS` before using **System Settings → Privacy & Security → Open Anyway**. Do not disable Gatekeeper.

```sh
foundry-opencode                # install
foundry-opencode install        # guided installation and repository setup
foundry-opencode update         # refresh packages/configs and repository assignments
foundry-opencode update -y      # accept all saved defaults without the wizard
foundry-opencode update -y --dry-run # preview saved defaults without changes
foundry-opencode init           # configure this Git repository
foundry-opencode init-rag       # configure/index RAG in this project
foundry-opencode sync-worktrees # move worktrees to the default branch, link .opencode, reindex RAG
foundry-opencode repair-plugins # retire a verified legacy V1 RAG plugin
foundry-opencode uninstall      # review and remove managed data (irreversible)
foundry-opencode reset          # stop OpenCode/OpenChamber and clear OpenCode cache
foundry-opencode --dry-run update
```

## Project setup

The wizard starts with a **Foundry stacks** list: configure the optional Global default, add named stacks, and edit their HTTPS hosts and masked private tokens. Next, use the single tree to mark **Git repository roots**. For every selected repo, choose one configured stack and whether to initialize RAG. Update reopens these saved assignments. A named stack cannot be removed while it is assigned to a saved project.

Update first offers an unchecked **Accept all loaded defaults and update** checkbox. Checking it, or passing `update -y`/`--yes`, uses the saved stack credentials, repository assignments and RAG choices without editing or confirmation screens. Invalid or missing saved settings stop before updates begin. `update -y --dry-run` prints a redacted read-only preview. The shortcut finishes without waiting for a completion-screen keypress; Git authentication or an administrator password may still be required.

The tree supports Up/Down to highlight, Enter to expand/collapse, Right to expand, Left to collapse or move to a parent, Space to mark, S to search, `/` to enter a path, and Tab to switch between the tree and search. Hidden Finder or dot-prefixed folders are omitted unless entered by their full path. Invalid or inaccessible folders do not close the wizard.

After review, install/update backs up and **replaces** project `opencode.json(c)` and `.opencode/opencode.json(c)` files with one `.opencode/opencode.json` for each selected repo. Other files under `.opencode` and the original config backups remain. Old parent-stack configs and `.envrc` files remain for manual review; they can still affect unselected repositories. Do not leave tracked tokens in Git: untrack them and rotate any exposed token.

Stack metadata and repository assignments live in `~/.config/foundry-opencode/catalog.json`; project credentials are stored in private `~/.config/foundry-opencode/tokens/` files (mode `0600`), not in project JSON. The optional global fallback also has a private `~/.config/opencode/token`. Project configs refer to private files using OpenCode V2 `{file:...}` references. Known credentials from an older `stacks.json` registry are imported on first setup without executing `.envrc`.

The Palantir MCP launcher specifies `--registry=https://registry.npmjs.org` for its own `npx` process; a project `.npmrc` still controls that project's package installs. Use `/mcps` to check the connection after deployment. A broken npm cache requires separate npm repair, not deletion of OpenCode sessions.

Install upgrades Homebrew OpenCode (verified V2), optionally installs OpenChamber, clones the latest global config with a backup, and downloads a checksummed macOS installer release for `/usr/local/bin/foundry-opencode`. Update refreshes packages/global config and the selected project assignments. If OpenChamber is running, its optional upgrade is skipped. A failed release download does not replace the installed binary. The CLI may request an administrator password after the wizard closes.

## RAG

Select **Initialize RAG** for a repository, or run `foundry-opencode init-rag` from its root after assigning a stack. Node.js and npm are required. The installer writes pinned RAG dependencies and `.opencode/rag.json` (which skips build caches, `.palantir` and generated SDK folders), indexes the project, and enables the RAG MCP server only after a successful setup. Failed indexing leaves the existing index and setup files for review/retry. Manual reindexing uses `node .opencode/rag-cli.mjs index` from the project root. A known older CLI helper is backed up before upgrading; edited or unrecognized RAG files are not overwritten. The V1 automatic watcher is unavailable in OpenCode V2.

`repair-plugins` can back up the known V1 `.opencode/plugins/rag-plugin.js` outside the auto-load folder. It refuses unfamiliar or edited plugins. The global configuration disables the incompatible `context-mode` plugin.

## Worktrees

Only a repository's main checkout can be selected; worktrees are refused with a pointer to it. Install/update link each worktree's `.opencode` entries to the main checkout's (the RAG index, logs and backups stay per worktree) and add `.opencode` to the local `.git/info/exclude`, unless the repository tracks files under `.opencode`. `sync-worktrees` moves every clean worktree to the latest default branch, flags branches, uncommitted work and unpushed commits without moving them, links missing entries and reindexes each checkout incrementally. The optional `git sync-worktrees` alias runs it; uninstall removes links, indexes, the exclude entry and the installer's alias.

## Safety

`--dry-run` previews actions without writing files, installing packages, or opening a browser. Tokens are redacted. Existing configs are backed up before replacement; an unrelated file in `.opencode` is not removed. `uninstall --dry-run` previews paths before a full wipe. Uninstall preserves project directories and unrecognized files. `reset` interrupts running OpenCode/OpenChamber processes and removes only the OpenCode cache; configs and sessions are retained.

## License

Copyright (c) Demetry <demetryromanowski@gmail.com>

This project is licensed under the MIT license ([LICENSE] or <http://opensource.org/licenses/MIT>)

[LICENSE]: ./LICENSE
