# Package manager status

The Phase 4 foundation implements bounded npm registry metadata lookup, semver and dist-tag selection, SHA-512 integrity verification, recursive dependency installation, scoped package paths, safe bounded tar extraction, and a content-addressed store under `%LOCALAPPDATA%\Sako\Store\sha512`. The store evicts its oldest archives after reaching 50,000 files or 10 GiB.

Commands:

```powershell
sako install [--ignore-scripts]
sako add <package[@range]> [--dev] [--ignore-scripts]
sako remove <package>
sako update [--ignore-scripts]
```

`sako.lock` is deterministic and human-readable. The current installer writes the resolved graph but does not replay it yet, so installs still resolve against current registry metadata. Reproducible lockfile replay is required before a stable release.

Package lifecycle scripts run through `cmd.exe` in the extracted package directory. They are untrusted code with the user's permissions. `--ignore-scripts` disables `preinstall`, `install`, and `postinstall`; it should be used for untrusted dependency trees. Job Object containment and richer npm environment emulation are not implemented yet.

Current limitations include peer dependency validation, workspaces, optional-dependency failure tolerance, private registry authentication, `.npmrc`, proxies, package `exports`/`imports`, hardlink materialization, lockfile replay, and non-npm sources.
