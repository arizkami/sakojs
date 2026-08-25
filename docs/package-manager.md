# Package manager status

The Phase 4 foundation implements bounded npm registry metadata lookup, npm comparator/OR/hyphen/X semver ranges and dist-tag selection, reproducible `engines.sako` validation, SHA-512 integrity verification, recursive dependency installation, scoped package paths, safe bounded tar extraction, and a content-addressed store under `%LOCALAPPDATA%\Sako\Store\sha512`. The store evicts its oldest archives after reaching 50,000 files or 10 GiB. Other engine declarations are retained in `sako.lock` but not enforced; in particular, Sako does not pretend to be a specific Node release for `engines.node`.

Commands:

```powershell
sako install [--ignore-scripts]
sako add <package[@range]> [--dev] [--ignore-scripts]
sako remove <package>
sako update [--ignore-scripts]
```

Installs draw a progress bar on stderr: a bar and a `done/total` count when a lock file names the graph up front, a spinner and a running count while it is still being resolved from the registry, and one summary line with the package count, elapsed time, and how much came from the store rather than the network. It is drawn only when stderr is a terminal, so redirected output keeps just the summary and any warnings; `SAKO_PROGRESS=0` turns it off and `SAKO_PROGRESS=1` forces it on. Nothing is written to stdout.

`sako.lock` version 2 is deterministic and human-readable. A compatible lock graph is replayed directly from integrity-addressed archives without fetching registry metadata. `sako update` intentionally discards the lock and resolves current matching versions.

Package lifecycle scripts run through direct `CreateProcessW`/`STARTUPINFOEX` launch of `cmd.exe` in the extracted package directory and remain untrusted code with the user's permissions. Only named-pipe standard handles are inherited, each output stream is bounded to 16 MiB, and a kill-on-close Job Object owns the process tree. `--ignore-scripts` disables `preinstall`, `install`, and `postinstall`; richer npm environment emulation is not implemented yet.

Peer ranges are validated, optional dependency failures are tolerated, and global/user/project `.npmrc` files support default or scoped registries plus path-scoped Bearer, encoded Basic, and `username`/base64 `_password` credentials. Supported settings use `global .npmrc < user .npmrc < project .npmrc < npm environment < Sako environment < explicit CLI options`; package commands accept `--registry`, `--token`, and `--proxy`. Workspace arrays and `{ "packages": [...] }` forms support direct paths or one `*` segment; workspace packages are copied with file/byte limits and replayed from the lock graph. Current limitations include recursive `**` workspace globs, live workspace links, `NO_PROXY` matching, hardlink materialization, imported third-party lockfiles, non-npm package sources, and full npm configuration parity.
