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

## A bucket per pull request

`toaba assets branch` gives each branch or pull request its own bucket, `branches/<branch>`, so
its content can be tried before it's merged: point a game build at that bucket's `latest` tag.

```sh
# On every push: make the bucket match the folder, and release it
toaba assets branch sync ./build pr-123 --from staging/assets --yes

toaba assets branch list                  # the branches, most recently released first
toaba assets branch close pr-123 --yes    # once the pull request is merged or closed
```

The first sync creates the `branches` environment and the bucket. With `--from`, a new branch
starts from that bucket's latest release, so only the files that differ are uploaded; later syncs
ignore it, so a pipeline can run the same command every time. The token needs write access to the
app's storage.

A git branch's name can be the branch too: it becomes an id, so `feature/New-UI` is
`branches/feature-new-ui`, and `close` takes the same name. Names that differ only in case or
punctuation, such as `feature/new-ui` and `feature-new-ui`, share a bucket; pull request numbers
never do.

In GitHub Actions, with `toaba` installed on the runner:

```yaml
on:
  pull_request:
    types: [opened, synchronize, reopened, closed]

env:
  TOABA_TOKEN: ${{ secrets.TOABA_TOKEN }}
  TOABA_APP: ${{ vars.TOABA_APP }}
  BRANCH: pr-${{ github.event.pull_request.number }}

# One run at a time per pull request, so two never sync the same bucket at once
concurrency:
  group: toaba-${{ github.event.pull_request.number }}

jobs:
  sync:
    if: github.event.action != 'closed'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: ./build-content.sh    # your build, writing ./build
      - run: toaba assets branch sync ./build "$BRANCH" --from staging/assets --yes

  close:
    if: github.event.action == 'closed'
    runs-on: ubuntu-latest
    steps:
      - run: toaba assets branch close "$BRANCH" --yes
```

Pull requests from forks don't get the repository's secrets, so these jobs fail for them: someone
outside the project can't write to your buckets.

## Files

The CLI keeps its state in `~/.config/toaba` (`$XDG_CONFIG_HOME/toaba`, or `%APPDATA%\toaba` on
Windows): `config.toml` holds your logins and the app in use, and `credentials.toml`, readable
only by you, holds their tokens. `toaba logout` ends a login on Toaba and removes it from both.
