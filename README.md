# apt-install-unstuck

`apt-get install` for GitHub-hosted Ubuntu runners that does not hang when the
Ubuntu mirror does.

The runner images fetch packages from `azure.archive.ubuntu.com`. Every now and
then that mirror slows down to a few tens of KB/s or stops answering, and an
`apt-get install` step sits there until the job times out. This action watches
the download, aborts an attempt that stops making progress, and continues on
another mirror. The runner's global apt configuration is left alone.

```yaml
- uses: jksy/apt-install-unstuck@v1
  with:
    packages: libfoo-dev libbar-dev
```

## How it works

1. **The first attempt uses the image as is**: `apt-get update` and
   `apt-get install` with the image's own sources, under the watchdog described
   below.
2. **Later attempts are isolated.** The Ubuntu sources of the image
   (`/etc/apt/sources.list.d/ubuntu.sources` in deb822 format on 24.04 and
   later, `/etc/apt/sources.list` on 22.04) are copied to a temporary directory
   with the archive replaced by the next mirror, and apt is pointed there with
   `-o Dir::Etc::SourceList`, `Dir::Etc::SourceParts` and `Dir::State::Lists`.
   `Dir::Cache::pkgcache` and `srcpkgcache` are disabled. The package cache
   (`/var/cache/apt/archives`) stays shared, so partially downloaded packages
   are reused. Nothing under `/etc/apt` or `/var/lib/apt/lists` is changed, and
   the temporary directory is removed at the end.
3. **Replaced URIs**: `mirror+file:/etc/apt/apt-mirrors.txt` and
   `azure.archive.ubuntu.com`, `archive.ubuntu.com`, `security.ubuntu.com` and
   `ports.ubuntu.com`. If the generated sources still mention Azure, the action
   fails, since that means the image layout changed.
4. **Watchdog**: while apt is fetching, the size of the lists directory (during
   `update`) or of `/var/cache/apt/archives` including `partial/` (during the
   download) is checked every 5 seconds. If it does not change for
   `stall-timeout` seconds, apt gets `SIGINT`, and `SIGKILL` 10 seconds later if
   it is still running. In addition, `timeout --signal=INT --kill-after=10`
   limits the network part of each attempt to `attempt-timeout` seconds; that
   is what catches a mirror that trickles data slowly rather than stopping.
5. Packages are downloaded with `--download-only` under the watchdog and then
   installed with `--no-download` without it, so a long `postinst` is never
   mistaken for a stall. A failure in that last step is not a mirror problem
   and is not retried.

### Architectures

On amd64 the mirrors serve `/ubuntu`. On arm64 before 26.04 the action uses the
ports layout (`ports.ubuntu.com/ubuntu-ports`, `<host>/ubuntu-ports`); from
26.04 on, arm64 is part of the main archive. Before an isolated attempt the
action asks the mirror for `dists/<codename>/InRelease`, and a mirror that
answers 404 is skipped. For example, `mirrors.edge.kernel.org` has no
`ubuntu-ports` tree, so it is skipped on arm64 runners before 26.04.

### Containers and other distributions

When the job already runs as root (for example a `container:` job without
`sudo`), commands run without `sudo`. On anything other than Ubuntu the action
prints a warning and runs plain `apt-get update` and `apt-get install`.

## Inputs

| Input | Default | Description |
|-------|---------|-------------|
| `packages` | (required) | Packages to install, separated by spaces or newlines. |
| `mirrors` | `default archive.ubuntu.com mirrors.edge.kernel.org` | Mirrors to try in order. `default` is the image configuration; anything else is a host name or a full URL such as `http://mirror.example.com/ubuntu`. |
| `rounds` | `2` | How many times to walk the mirror list. |
| `stall-timeout` | `60` | Seconds without download progress before an attempt is aborted. |
| `attempt-timeout` | `600` | Hard limit in seconds for `update` and the download of one attempt. |
| `install-recommends` | `false` | Install recommended packages. |
| `lock-timeout` | `120` | Seconds to wait for the dpkg lock (`DPkg::Lock::Timeout`). |
| `isolate` | `true` | Leave the system apt configuration untouched. With `false`, the mirror that worked is prepended to `/etc/apt/apt-mirrors.txt`, so later `apt-get update` calls in the job prefer it. |
| `extra-sources` | `false` | Keep non-Ubuntu sources (`packages.microsoft.com`, PPAs, ...) in isolated attempts. Without it, isolated attempts only see the Ubuntu archive. |

## Outputs

| Output | Description |
|--------|-------------|
| `mirror` | The mirror the packages came from, `default` for the image configuration. |
| `attempts` | Number of attempts made (skipped mirrors are not counted). |
| `duration` | Total time in seconds. |
| `results` | Every attempt as `mirror=result`, separated by spaces. Results are `ok`, `stalled`, `timeout` and `failed`. |

Each attempt is also listed in the job summary.

## Using the script directly

`apt-install.sh` has no dependency on the action and can be copied into another
action or run by hand:

```sh
./apt-install.sh --mirrors "default archive.ubuntu.com" --stall-timeout 30 -- libfoo-dev
```

Run `./apt-install.sh --help` for all options. They match the inputs above.
`GITHUB_OUTPUT` and `GITHUB_STEP_SUMMARY` are written when they are set.

## Tests

`tests/slow-mirror.py` is a fake mirror that either proxies a real mirror at a
fixed rate (`--mode slow --rate 2048`) or accepts connections and never answers
(`--mode hang`). The workflow puts it first in `mirrors` on ubuntu-22.04, 24.04
and 26.04, x64 and arm64, and checks that the second mirror succeeds, that each
attempt reports the expected result, and that `/etc/apt` and
`/var/lib/apt/lists` are unchanged.

## License

MIT
