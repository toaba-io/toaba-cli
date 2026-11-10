# toaba

The command-line tool for [Toaba](https://toaba.io). It publishes a game's content from a
terminal or a build pipeline: sync a folder into a bucket, cut a release, point a tag at it, and
promote it from staging to production.

Setting things up (apps, environments, buckets, keys) is done in the dashboard. The CLI covers
what each build does.

## Build

There are no release binaries yet. With a Rust toolchain:

```sh
cargo install --path .
```

## Log in

```sh
toaba login      # opens the browser; one login per org
toaba status     # your logins, and the app commands use
toaba app list   # the apps your logins reach
toaba app use my-game
```

Every command acts on the app in use. To name another for one command, pass `--app`, by name or
id.

## Publish content

A bucket is written `env/bucket`, such as `staging/assets`.

```sh
# See what a sync would change, without changing anything
toaba assets sync ./build staging/assets --dry-run

# Make the bucket match the folder, cut a release, and point a tag at it
toaba assets sync ./build staging/assets --release v1.4 --tag beta

# Publish that release in production, and move the tag players download by
toaba assets promote staging/assets beta production/assets --tag stable

# Roll back: point the tag at an older release
toaba assets tag set stable production/assets 41
```

`sync` is a mirror: files that are gone from the folder are removed from the bucket. Game clients
download by tag, so nothing reaches players until a tag moves.

Commands that change something show what they are about to do and ask first. Pass `--yes` to skip
the question.

To look around:

```sh
toaba assets bucket list
toaba assets release list staging/assets
toaba assets tag list production/assets
toaba app keys --env    # TOABA_APP_ID and TOABA_PUBLISHABLE_KEY, for a game build
```

## In CI

A pipeline doesn't log in. Create a service account and a token for it in the dashboard, then set:

| Variable | What it is |
| --- | --- |
| `TOABA_TOKEN` | The service account's token |
| `TOABA_APP` | The app, by id or name |

```sh
toaba assets sync ./build staging/assets --release "$VERSION" --tag beta --yes
```

Without a terminal, a command that would ask a question fails unless it gets `--yes`. Add
`-o json` to any command for output a script can read: results go to stdout, messages to stderr,
and a failure prints `{"error": "…"}` with a non-zero exit code.

## Files

The CLI keeps its state in `~/.config/toaba` (`$XDG_CONFIG_HOME/toaba`, or `%APPDATA%\toaba` on
Windows): `config.toml` holds your logins and the app in use, and `credentials.toml`, readable
only by you, holds their tokens. `toaba logout` ends a login on Toaba and removes it from both.
