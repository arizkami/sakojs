# Package manager status

The Phase 4 foundation implements bounded npm registry metadata lookup, npm comparator/OR/hyphen/X semver ranges and dist-tag selection, reproducible `engines.sako` validation, SHA-512 integrity verification, concurrent dependency resolution, scoped package paths, safe bounded tar extraction, and content-addressed stores under `%LOCALAPPDATA%\Sako\Store`. Other engine declarations are retained in `sako.lock` but not enforced; in particular, Sako does not pretend to be a specific Node release for `engines.node`.

## How an install runs

An install is a graph walk followed by four bounded worker pools:

```
package.json
     |
     v
resolve  -- bounded pool, keyed by (registry, name, range)
     |        one request per key however many edges ask; a second asker
     |        joins the first request rather than starting its own
     v
plan     -- ONE thread, fixed order, no I/O
     |        every decision about the tree is made here, which is why the
     |        lockfile does not depend on which worker finished first
     v
fetch    -- bounded pool, one download per distinct archive
store    -- bounded pool, one unpack per distinct archive
materialize -- bounded pool, one directory per tree position, a level at a time
link     -- bounded pool, one `.bin` per directory that received packages
scripts  -- sequential, in dependency order
     |
     v
sako.lock
```

Resolution is keyed by what is being asked for rather than by where in the tree
it was asked from. A tree of 1,815 positions typically asks about 160 or so
distinct things; the rest join an answer that already exists or is already on
its way. Packages are likewise unpacked once per archive rather than once per
position, into `Store\unpacked`, and each position is filled by copying from
there. Both stores promote by rename, so an entry is either absent or complete
even when two installs populate it at the same moment.

Concurrency is bounded everywhere and never unbounded task spawning. The
defaults suit the machine they are read on and can be overridden:
`SAKO_METADATA_CONCURRENCY` (16), `SAKO_DOWNLOAD_CONCURRENCY` (12),
`SAKO_EXTRACT_CONCURRENCY` (half the logical processors),
`SAKO_MATERIALIZE_CONCURRENCY` and `SAKO_LINK_CONCURRENCY` (the logical
processor count). See [performance-log.md](performance-log.md) for how those
numbers were chosen.

Registry metadata is cached on disk under `Store\metadata`, keyed by registry
and package name so a private registry's copy is never served for a public one.
Freshness is the registry's own `Cache-Control: max-age`; past it, Sako
revalidates with `If-None-Match` and a 304 avoids transferring the document
again. The tarball store evicts its oldest archives after 50,000 files or
10 GiB, and the unpacked store evicts on the same limits counted in packages.

Commands:

```powershell
sako install [--ignore-scripts] [--perf] [--verbose]
sako add <package[@range]> [--dev] [--ignore-scripts]
sako remove <package>
sako update [--ignore-scripts]
```

`--perf` prints a per-stage breakdown and the resolver's counters when the
install finishes; `--verbose` adds a per-package line above it saying where each
packument came from and what selecting a version cost. Neither is on by default
and neither changes what an install does.

Installs draw a live tree on stderr, with a count per stage and the packages
currently being resolved beneath it:

```
install
├─ resolve  102/106
│  ├─ @nodelib/fs.stat
│  ├─ reusify
│  └─ queue-microtask
├─ fetch    100/100
├─ store    477/1803
└─ link     0/0
```

Workers never draw: they move counters, and a renderer thread repaints from
those on a 40 ms timer, so the terminal cannot become the slowest part of an
install however many events a graph produces. When stderr is not a terminal the
tree is replaced by plain lines with no cursor control, suitable for a log:

```
resolving dependencies...
resolved 106 packages
fetched 100 packages
linked 839 directories
done 1803 packages in 13.2s
```

Terminal detection happens per run rather than being cached at startup;
`SAKO_PROGRESS=0` turns the tree off and `SAKO_PROGRESS=1` forces it on. Nothing
is written to stdout.

`sako.lock` version 2 is deterministic and human-readable. It is written from
the plan, which is built on one thread in a fixed order from a completed
resolution map, so repeated installs of the same graph produce byte-identical
lockfiles regardless of the order work finished in. A compatible lock graph is
replayed through the same pools without fetching registry metadata. `sako
update` intentionally discards the lock and resolves current matching versions.

Package lifecycle scripts run through direct `CreateProcessW`/`STARTUPINFOEX` launch of `cmd.exe` in the extracted package directory and remain untrusted code with the user's permissions. Only named-pipe standard handles are inherited, each output stream is bounded to 16 MiB, and a kill-on-close Job Object owns the process tree. `--ignore-scripts` disables `preinstall`, `install`, and `postinstall`; richer npm environment emulation is not implemented yet.

A dependency named in both `dependencies` and `optionalDependencies` -- which
is how npm's abbreviated packument describes every optional dependency -- is
treated as optional, matching npm. Peer ranges are validated, optional
dependency failures are tolerated, and global/user/project `.npmrc` files support default or scoped registries plus path-scoped Bearer, encoded Basic, and `username`/base64 `_password` credentials. Supported settings use `global .npmrc < user .npmrc < project .npmrc < npm environment < Sako environment < explicit CLI options`; package commands accept `--registry`, `--token`, and `--proxy`. Workspace arrays and `{ "packages": [...] }` forms support direct paths or one `*` segment; workspace packages are copied with file/byte limits and replayed from the lock graph. Current limitations include recursive `**` workspace globs, live workspace
links, `NO_PROXY` matching, hoisted (flat) `node_modules` layout, `npm:` package
aliases, imported third-party lockfiles, non-npm package sources, and full npm
configuration parity.
