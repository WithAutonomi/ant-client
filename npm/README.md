# npm distribution of the `ant` CLI

Source for the npm packages that distribute `ant`. Published from
[`.github/workflows/ant-cli-release.yml`](../.github/workflows/ant-cli-release.yml) on every
`ant-cli-v*` tag.

This is a packaging layer only. It rebuilds nothing: the binaries are copied verbatim out of the
release archives that the same workflow run has already built and signed, so a published npm
tarball contains a byte-identical copy of the corresponding GitHub release asset. `install.sh`,
`install.ps1` and the release page are unaffected.

## Why npm

Agent sandboxes allow package-manager traffic by default and treat direct binary downloads as an
exception — Claude Code cloud sessions return 403 for release assets of repos not attached to the
session, and skill-directory security scanners flag piped installers while passing package
managers. Shipping a native binary on npm is now a well-trodden path for exactly this reason
(Biome, Turborepo, swc, Deno, oxlint, the Tauri CLI, and Stripe's Go CLI as `@stripe/cli`).

See Linear V2-1152 / [ant-client#190](https://github.com/WithAutonomi/ant-client/issues/190).

## Layout

Six packages, following the mechanism esbuild established:

```
@withautonomi/ant                 meta package: launcher + postinstall, no binary
├── @withautonomi/ant-linux-x64        optional, os=linux  cpu=x64
├── @withautonomi/ant-linux-arm64      optional, os=linux  cpu=arm64
├── @withautonomi/ant-darwin-x64       optional, os=darwin cpu=x64
├── @withautonomi/ant-darwin-arm64     optional, os=darwin cpu=arm64
└── @withautonomi/ant-win32-x64        optional, os=win32  cpu=x64, arm64
```

Each platform package declares `os`/`cpu`. npm skips an `optionalDependencies` entry whose
platform guard does not match instead of failing the install, so a user downloads exactly one
binary. The user only ever names the meta package.

Sources here:

```
npm/
├── ant/                     meta package source
│   ├── package.json.tmpl    __VERSION__ substituted at build time
│   ├── README.md.tmpl       the page shown on npmjs.com
│   ├── bin/ant.js           launcher — resolves and execs the real binary
│   ├── lib/resolve.js       platform → package → binary path resolution
│   └── postinstall.js       copies bootstrap_peers.toml into the config dir
├── platform/                templates rendered once per target
│   ├── package.json.tmpl
│   └── README.md.tmpl
└── build-packages.sh        turns release artifacts into publishable packages
```

## Design notes

**A JS launcher, not a postinstall binary copy.** esbuild copies the binary into the meta package
during postinstall so npm can exec it directly. We do not, because `npm install --ignore-scripts`
is common in the sandboxes this whole exercise targets, and a package that only works when
scripts run would defeat the point. The launcher costs Node's startup time on each invocation —
negligible for a network-bound CLI.

**Windows ARM64 gets the x86_64 build.** There is no native ARM64 Windows target in the release
matrix, and `install.ps1:160-162` already installs the x86_64 binary there to run under
emulation. The win32 package lists `arm64` in its `cpu` array so npm behaves the same way.

**No `libc` field.** Both Linux targets are statically linked musl builds, so they run on glibc
and musl systems alike and `os`+`cpu` is enough to pick the right one.

**Windows ARM64, `--no-optional` and unsupported platforms** all surface through
`bin/ant.js`, which explains what happened rather than failing with a module-resolution error.

## Bootstrap peers, and why the binary embeds them

`postinstall.js` copies `bootstrap_peers.toml` into the platform config directory, mirroring
`install.sh:216-224` and `install.ps1:189-198` exactly — same destination, and only when the file
is absent, so a user's edited peer list is never overwritten.

**That script often will not run.** npm 12 blocks package install scripts by default: its
`allow-scripts` default is empty, so a plain `npm install -g @withautonomi/ant` prints

```
npm warn install-scripts 1 package had install scripts blocked because they are not covered by allowScripts
```

and skips the copy. `--ignore-scripts` and `ignore-scripts=true` in `.npmrc` do the same on older
npm. Since ant-core only ever *reads* this file (`ant-core/src/config.rs`), that would leave a CLI
that installs cleanly, reports its version, and then fails every network command — in exactly the
locked-down sandboxes this package exists to serve.

So `ant-core` embeds the same file with `include_str!` and falls back to it when no config file is
present (`EMBEDDED_BOOTSTRAP_PEERS` in `ant-core/src/config.rs`). Priority is unchanged for every
existing install: explicit `-b` peers beat a devnet manifest, which beats the config file, which
beats the embedded list. The fallback is reached only where the alternative was
`Error::NoBootstrapPeers`, so it can never override a user's choice, and an explicitly selected
devnet manifest still errors rather than silently reaching for mainnet peers.

This is why `resources/bootstrap_peers.toml` now lives at `ant-core/resources/bootstrap_peers.toml`:
`include_str!` cannot reach outside the crate directory, and ant-core is published to crates.io.
The release archives and both installers are unaffected — they carry and copy the same bytes from
the same single source of truth.

The postinstall is kept regardless, because a real file in the config directory is discoverable
and editable in a way an embedded constant is not.

## Building the packages

```sh
npm/build-packages.sh --version 0.3.6 --artifacts <dir-of-release-assets> --out dist/
```

The artifacts directory must contain the five archives, their `.sig` files and `SHA256SUMS.txt`.
The script verifies every checksum against `SHA256SUMS.txt` and every ML-DSA-65 signature against
`resources/release-signing-key.pub` (context `ant-release-v1`) before it copies a single byte,
and aborts on any mismatch. `ant-keygen` comes from `--keygen`, `$ANT_KEYGEN`, or `PATH`.

Pass `--skip-signature-verification` for a local dry-run against archives you built yourself,
which have no signatures. Checksums are still verified.

**Publish the five platform packages before the meta package** — its `optionalDependencies` pin
their exact versions and will not resolve otherwise.

## Local dry-run

Verifies the whole path end to end without publishing anything, using
[verdaccio](https://verdaccio.org) as a throwaway registry. A local tarball alone is not enough:
the meta package has to resolve its optional dependencies from a registry, which is exactly the
step being tested.

```sh
# 1. Build ant for your host, and stage an archive in the shape the release workflow produces.
cargo build --release --bin ant
VERSION=0.0.0-dryrun
TARGET=x86_64-unknown-linux-musl          # the target matching your machine
STAGE=$(mktemp -d)
mkdir -p "$STAGE/ant-$VERSION-$TARGET"
cp target/release/ant "$STAGE/ant-$VERSION-$TARGET/"
cp ant-core/resources/bootstrap_peers.toml "$STAGE/ant-$VERSION-$TARGET/"
(cd "$STAGE" && tar czf "ant-$VERSION-$TARGET.tar.gz" "ant-$VERSION-$TARGET" \
  && sha256sum ant-*.tar.gz > SHA256SUMS.txt)

# 2. Generate the packages.
npm/build-packages.sh --version "$VERSION" --artifacts "$STAGE" --out dist \
  --skip-signature-verification

# 3. Serve them from a local registry and install as a user would.
npx verdaccio --config verdaccio.yaml &
npm --registry http://localhost:4873 adduser        # any credentials; verdaccio accepts them
for p in dist/ant-*; do (cd "$p" && npm publish --registry http://localhost:4873); done
(cd dist/ant && npm publish --registry http://localhost:4873)
npm install -g @withautonomi/ant --registry http://localhost:4873

# 4. Check.
ant --version
cat "${XDG_CONFIG_HOME:-$HOME/.config}/ant/bootstrap_peers.toml"
npm uninstall -g @withautonomi/ant
ls "${XDG_CONFIG_HOME:-$HOME/.config}/ant/"         # config must survive
```

Building only your host target is fine for exercising the packaging; the other four packages will
contain a stand-in binary and must not be published.

## Adding a release target

Four places, all of which must agree:

1. the build matrix in `.github/workflows/ant-cli-release.yml`
2. `PLATFORM_TARGETS` in `build-packages.sh`
3. `PACKAGES` in `ant/lib/resolve.js`
4. `optionalDependencies` in `ant/package.json.tmpl`

## Publishing credentials

The release workflow publishes with `NPM_TOKEN` (a granular access token with write access to
`@withautonomi/*`) and `--provenance`, which needs `id-token: write` on the job.
