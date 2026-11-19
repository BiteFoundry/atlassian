# npm distribution

`@bitefoundry/atlassian` is distributed on npm as small JavaScript launchers plus
one optional dependency per platform. The launchers resolve the prebuilt Rust
binary and exec it, so there is no compiler, Node native addon, or postinstall
download step.

The Rust crate is named `atlassian-cli` and produces a single unified binary
(`jira` / `jira.exe`) that powers Jira (`jira`) and Confluence (`confluence` /
`conf`, via `ATLASSIAN_PERSONA=confluence`). The npm package is scoped as
`@bitefoundry/atlassian` and installs **all three** commands (`jira`, `confluence`,
and `conf`).

## Install and run

```sh
# one-off
npx @bitefoundry/atlassian --version
npx --yes -p @bitefoundry/atlassian confluence --version
npx --yes -p @bitefoundry/atlassian conf --version

# global install (installs `jira`, `confluence`, and `conf`)
npm install -g @bitefoundry/atlassian
jira --version
confluence --version
conf --version

# project-local
npm install @bitefoundry/atlassian
npx jira boards
npx confluence spaces
npx conf pages --space ENG
```

All three commands share the exact same Atlassian authentication (Cloud API
Token or Data Center PAT), SQLite WAL + FTS5 cache (`cache.db`), HTTP JSON API
(`jira serve`), and MCP server (`jira mcp`):

- `jira login`, `jira projects`, `jira boards`, `jira issues`, `jira view`, `jira create`, `jira transition`, `jira tui`, `jira serve`, `jira mcp`, `jira update`
- `confluence`, `conf`, `confluence spaces`, `confluence pages`, `confluence view`, `confluence create`, `confluence edit`, `confluence search`, `confluence update`

## Supported platforms

| npm package                       | OS      | CPU   | Rust target                 |
| --------------------------------- | ------- | ----- | --------------------------- |
| `@bitefoundry/atlassian-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`    |
| `@bitefoundry/atlassian-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`   |
| `@bitefoundry/atlassian-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`       |
| `@bitefoundry/atlassian-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`      |
| `@bitefoundry/atlassian-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu`  |
| `@bitefoundry/atlassian-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` |

Linux packages are built against glibc. Alpine/musl users should build from
source with Cargo; there is no musl package yet.

## How resolution works

`bin/jira.js`, `bin/confluence.js`, and `bin/conf.js` look up
`@bitefoundry/atlassian-<platform>-<arch>` through `require.resolve`, then execute
`node_modules/@bitefoundry/atlassian-<platform>-<arch>/bin/jira` (or `jira.exe` on
Windows) with `stdio: 'inherit'`. `bin/confluence.js` and `bin/conf.js` set
`ATLASSIAN_PERSONA=confluence` before spawning the binary so the unified
executable boots directly in Confluence mode. Signals (`SIGINT`, `SIGTERM`,
`SIGHUP`) are forwarded and the launcher exits with the child's exit code. If
the platform package is missing (for example because optional dependencies were
skipped), the launcher prints the supported platforms and the
`--include=optional` hint instead of a stack trace.

## Layout

```text
package.json                     # @bitefoundry/atlassian launcher package (bin: jira, confluence, conf)
bin/jira.js                      # jira launcher
bin/confluence.js                # confluence launcher (sets ATLASSIAN_PERSONA=confluence)
bin/conf.js                      # conf launcher (sets ATLASSIAN_PERSONA=confluence)
jira / jira.cmd                  # repo-local Jira launchers
confluence / confluence.cmd      # repo-local Confluence launchers
conf / conf.cmd                  # repo-local short Confluence launchers
npm/atlassian-<platform>/        # one package per platform
  package.json                   # os/cpu/files, same version as root
  README.md                      # platform package README
  bin/jira[.exe]                 # added by scripts/build-npm.mjs, never committed
scripts/build-npm.mjs            # copy target/<triple>/release/jira[.exe]
scripts/sync-version.mjs         # Cargo.toml version -> all package.json files
scripts/install-profile.ps1      # PowerShell $PROFILE + $HOME\Tools\bin installer
.github/workflows/release.yml    # tag-triggered build, publish, GitHub Release
```

Binaries are gitignored (`npm/*/bin/jira*`). A clean checkout has empty platform
`bin/` directories until a build or `npm run build:npm` runs.

## Versioning

`Cargo.toml` (`[package].version`) is the single source of truth. To bump:

```sh
# edit Cargo.toml [package].version
node scripts/sync-version.mjs
```

The script updates the root `package.json`, every `npm/atlassian-*/package.json`,
and the root `optionalDependencies` pins. It is idempotent; `--check` exits
non-zero if anything is stale. All packages must be published at the same
version because the root package pins its platform packages exactly.

Tag the release with a matching `v` prefix:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Publishing

Publishing is automated by `.github/workflows/release.yml`, which is triggered
by pushing a `v*` tag (or manually via `workflow_dispatch`).

1. Add a repository secret `NPM_TOKEN` (an npm automation token with publish
   rights for the `@bitefoundry/atlassian` packages).
2. Bump the version in `Cargo.toml`, run `node scripts/sync-version.mjs`,
   commit, and push `v<version>`.
3. The workflow builds six targets, stages binaries into the platform
   packages, uploads artifacts, publishes each platform package, publishes the
   root package, and creates a GitHub Release with the archives, `jira.exe`,
   and `SHA256SUMS.txt`.

The publish job verifies that the tag matches the package version and that all
six platform packages contain a binary before publishing anything. Platform
packages are published first so the root package's `optionalDependencies` can
resolve immediately.

Manual publish (matching order):

```sh
node scripts/sync-version.mjs --check
for dir in npm/atlassian-*; do npm publish "$dir" --access public; done
npm publish --access public
```

## Local validation

```sh
# stage the current host binary (Windows example)
node scripts/build-npm.mjs --target x86_64-pc-windows-msvc
node bin/jira.js --version
node bin/confluence.js --version
node bin/conf.js --version

npm pack --dry-run
npm pack --dry-run ./npm/atlassian-win32-x64
```

`node scripts/build-npm.mjs` with no arguments uses the host target and falls
back to `target/release/` when a plain `cargo build --release` was used.
`--all` attempts every supported target and warns about the ones not built on
the current host.

## Troubleshooting

- **`no prebuilt binary found for <platform>-<arch>`** — the optional
  dependency was skipped. Reinstall with
  `npm install -g @bitefoundry/atlassian --include=optional` or
  `npm install @bitefoundry/atlassian --include=optional`. Check that your npm
  config does not set `omit=optional`.
- **`EBADPLATFORM` / package skipped** — npm installs only the matching
  `os`/`cpu` package. Cross-installing (for example x64 Node on ARM Windows via
  emulation) still reports the Node architecture, so install under the native
  Node build.
- **Alpine/musl Linux** — unsupported; build with Cargo.
- **macOS Gatekeeper** — npm-installed binaries are not notarized. If macOS
  blocks execution, allow the binary in System Settings -> Privacy & Security,
  or build from source.
- **Corporate proxy/firewall** — `npx @bitefoundry/atlassian` performs a registry
  download each time unless cached; use a global install for offline-friendly
  use.
- **Verify the binary** —
  `node -p "require.resolve('@bitefoundry/atlassian-win32-x64/package.json')"`
  (adjust for your platform) shows which platform package npm selected.
- **Tag push did not publish** — check the publish job log; a tag/version
  mismatch or a missing `NPM_TOKEN` fails the job before any publish.
