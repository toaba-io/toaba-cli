# toaba

The command-line tool for [Toaba](https://toaba.io), the PaaS for live games. It publishes a
game's content from a terminal or a build pipeline: upload a build folder to a bucket, cut a
release, point a tag at it, and promote it from staging to production. Setting things up (apps,
environments, buckets, keys) stays in the dashboard.

**Documentation: [docs.toaba.io/docs/cli](https://docs.toaba.io/docs/cli)**

## Install

On Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/toaba-io/toaba-cli/releases/latest/download/toaba-cli-installer.sh | sh
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/toaba-io/toaba-cli/releases/latest/download/toaba-cli-installer.ps1 | iex"
```

Either puts `toaba` in `~/.toaba/bin` and adds it to your `PATH`. There are no macOS builds for
now. On a Mac, or to build from source anywhere, with a Rust toolchain:

```sh
cargo install --locked --git https://github.com/toaba-io/toaba-cli
```

## Quick start

```sh
toaba login                     # log in through the browser
toaba app use "Space Shooter"   # the app commands act on

# Upload a build's new and changed files, release it, and tag it beta
toaba assets sync ./build staging/android --release v1.4 --tag beta

# Publish that release in production, and move the tag players download by
toaba assets promote staging/android beta production/android --name v1.4 --tag live
```

Commands that change something show their plan and ask first; `--yes` skips the question, and
`-o json` gives output a script can read.

## Guides

- [Install, log in, and choose an app](https://docs.toaba.io/docs/cli/login)
- [Use the CLI in CI](https://docs.toaba.io/docs/cli/ci) with a service account token
- [Publish from CI](https://docs.toaba.io/docs/content/publish-from-ci): staging on every merge,
  production on a version tag
- [A bucket per pull request](https://docs.toaba.io/docs/content/branch-buckets)
- [Migrate from Unity CCD](https://docs.toaba.io/docs/content/migrate-from-unity-ccd), with a
  `ugs ccd` command mapping

## License

Apache-2.0, see [LICENSE](LICENSE). The license covers this CLI only; it grants no right to use
the Toaba name or logo.
