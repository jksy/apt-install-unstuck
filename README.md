# apt-install-unstuck

A GitHub Action that installs apt packages on GitHub-hosted Ubuntu runners
without getting stuck on a misbehaving mirror.

## The problem

Ubuntu runners fetch packages from `azure.archive.ubuntu.com` (or `ports.ubuntu.com`
on arm64). Every now and then this mirror degrades: transfers crawl at a few tens
of KB/s, or a connection is accepted and then never answers. `apt-get` has no
notion of "too slow" and waits, so a job that normally installs packages in
20 seconds hangs for many minutes or until the job times out.

## How it works

The action starts a small HTTP forward proxy (`unstuck-proxy`, written in Rust)
inside the job and runs `apt-get` with `-o Acquire::http::Proxy=...` (and
`-o Acquire::https::Proxy=DIRECT`) on each command. The proxy:

- **Switches mirrors when a transfer stalls.** If response headers do not arrive
  within 15 s, or the average rate over the last 20 s drops below 50 KiB/s, it
  abandons that upstream and continues with the next one. The next mirror is
  fetched from the start and the bytes apt already received are skipped, so apt
  sees one uninterrupted download.
- **Optionally caches files** in S3-compatible storage (AWS S3, Cloudflare R2,
  MinIO, ...) or a local directory. Package files (`pool/`) and `by-hash` indexes
  never change for a given path, so they are served from the cache without asking
  upstream. Release files (`InRelease`, ...) are always fetched fresh, and only
  served from the cache when every mirror fails.

The runner's global apt configuration (`sources.list`, `apt.conf.d`,
`apt-mirrors.txt`) is never modified. Only the commands run by this action (or
commands you point at `proxy-url`) use the proxy.

## Usage

```yaml
- uses: jksy/apt-install-unstuck@v1
  with:
    packages: ghostscript imagemagick
```

This runs `apt-get update` and `apt-get install -y --no-install-recommends ghostscript imagemagick`
through the proxy, then stops it and writes a summary to the job page.

### Inputs

| Input | Default | Description |
|---|---|---|
| `packages` | | Packages to install, separated by whitespace. When empty, only the proxy is started. |
| `cache` | `none` | `none`, `s3://bucket/prefix`, or a local directory. |
| `cache-key-prefix` | `${ImageOS}-${ImageVersion}-${arch}` | Key prefix inside the cache. |
| `mirrors` | | Upstreams per archive (see below). |
| `min-rate` | `50K` | Minimum transfer rate in bytes/s (`K` and `M` are KiB and MiB). |
| `stall-timeout` | `20` | Seconds below `min-rate` before switching mirrors. |
| `rounds` | `2` | How many times to go through the mirror list. |
| `update` | `true` | Run `apt-get update` first. |
| `install-recommends` | `false` | Install recommended packages. |
| `lock-timeout` | `120` | Seconds to wait for the dpkg lock. |
| `attempt-timeout` | `900` | Upper bound in seconds for each `apt-get` command. |
| `keep-running` | `false` | Keep the proxy running until the end of the job. |
| `listen` | `127.0.0.1:0` | Address the proxy listens on (port 0 picks a free port). |

### Outputs

| Output | Description |
|---|---|
| `proxy-url` | URL of the proxy, when `keep-running` is set. |
| `proxy-port` | Port the proxy listens on. |
| `cache-hits` | Requests served from the cache. |
| `cache-misses` | Requests fetched from upstream mirrors. |
| `failovers` | Times the proxy switched to another mirror. |

### Mirrors

By default, requests go to the host in the runner's apt sources first, then to
the other mirrors of that archive:

| Archive | Matched hosts | Default upstreams |
|---|---|---|
| `ubuntu` | `archive.ubuntu.com`, `*.archive.ubuntu.com`, `security.ubuntu.com` | `origin`, `http://archive.ubuntu.com/ubuntu/`, `http://mirrors.edge.kernel.org/ubuntu/` |
| `ubuntu-ports` | `ports.ubuntu.com`, `*.ports.ubuntu.com` | `origin`, `http://ports.ubuntu.com/ubuntu-ports/`, `http://mirrors.edge.kernel.org/ubuntu-ports/` |

`origin` stands for the host the request was made to (for example
`azure.archive.ubuntu.com`). A list given in `mirrors` replaces the defaults for
that archive, and the origin is only used where you write `origin`. For
example, to skip the Azure mirror entirely:

```yaml
    mirrors: >-
      ubuntu=http://archive.ubuntu.com/ubuntu/,http://mirrors.edge.kernel.org/ubuntu/;
      ubuntu-ports=http://ports.ubuntu.com/ubuntu-ports/
```

## S3 cache

Cached objects are stored at `<prefix>/<cache-key-prefix>/<archive>/<path>`,
for example `apt/ubuntu24-20261020.1-x64/ubuntu/pool/main/g/ghostscript/...`.
The host name is not part of the key, so a file is shared no matter which mirror
it came from. Credentials come from the usual environment variables
(`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION`,
`AWS_ENDPOINT_URL`). Only complete downloads are stored: the size must match
`Content-Length`, and `by-hash` files must match their hash.

### AWS with OIDC

```yaml
permissions:
  id-token: write
  contents: read

steps:
  - uses: aws-actions/configure-aws-credentials@v5
    with:
      role-to-assume: arn:aws:iam::123456789012:role/apt-cache
      aws-region: ap-northeast-1
  - uses: jksy/apt-install-unstuck@v1
    with:
      packages: ghostscript
      cache: s3://my-apt-cache/apt
```

### Cloudflare R2

```yaml
  - uses: jksy/apt-install-unstuck@v1
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.R2_SECRET_ACCESS_KEY }}
      AWS_REGION: auto
      AWS_ENDPOINT_URL: https://<account-id>.r2.cloudflarestorage.com
    with:
      packages: ghostscript
      cache: s3://my-apt-cache/apt
```

### IAM permissions

The role needs read and write access to the cache prefix only:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject"],
      "Resource": "arn:aws:s3:::my-apt-cache/apt/*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::my-apt-cache",
      "Condition": { "StringLike": { "s3:prefix": "apt/*" } }
    }
  ]
}
```

`s3:ListBucket` lets a missing object come back as 404 instead of 403. Large
files are uploaded with multipart upload, which `s3:PutObject` covers.

### Cleaning up

The default `cache-key-prefix` includes the runner image version, which changes
about once a week, so old prefixes stop being used. Add a lifecycle rule that
expires objects under the cache prefix, for example after 30 days, and one that
aborts incomplete multipart uploads after a day:

```json
{
  "Rules": [
    {
      "ID": "expire-apt-cache",
      "Status": "Enabled",
      "Filter": { "Prefix": "apt/" },
      "Expiration": { "Days": 30 },
      "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 1 }
    }
  ]
}
```

R2 offers the same rules under the bucket's *Object lifecycle rules* settings.

### Local directory with actions/cache

```yaml
  - uses: actions/cache@v4
    with:
      path: ~/.cache/apt-unstuck
      key: apt-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('.github/apt-packages.txt') }}
  - uses: jksy/apt-install-unstuck@v1
    with:
      packages: ghostscript
      cache: ~/.cache/apt-unstuck
```

## Using the proxy from later steps and `docker build`

With `keep-running: true` the proxy keeps running until the end of the job, and
pending cache writes are flushed in the action's post step.

```yaml
  - id: apt
    uses: jksy/apt-install-unstuck@v1
    with:
      keep-running: true
      listen: 0.0.0.0:3142
  - run: |
      sudo apt-get -o Acquire::http::Proxy=${{ steps.apt.outputs.proxy-url }} \
        -o Acquire::https::Proxy=DIRECT install -y ghostscript
  - run: |
      docker build --network host \
        --build-arg http_proxy=http://127.0.0.1:3142 \
        -t my-image .
```

`proxy-url` uses `127.0.0.1`, which is only reachable from the runner itself. A
container on the default bridge network cannot reach the runner's loopback
address, so either build with `--network host` (as above), or listen on an
address the container can reach (`0.0.0.0:3142` or the docker bridge,
`172.17.0.1:3142`) and pass `http://172.17.0.1:3142` as `http_proxy`. Listening
on `0.0.0.0` exposes the proxy to anything that can reach the runner on that
port; hosted runners do not accept inbound connections, but keep this in mind on
self-hosted runners.

`http_proxy` makes every HTTP request in the build use the proxy. Requests for
hosts outside the archive table are relayed without caching or mirror switching.

## Limitations

- Only `http://` apt sources go through the proxy. HTTPS sources (PPAs on
  `ppa.launchpadcontent.net`, vendor repositories, ...) are fetched by apt
  directly, as before. When you point apt at `proxy-url` yourself, also pass
  `-o Acquire::https::Proxy=DIRECT`: without it apt sends HTTPS sources to the
  proxy as `CONNECT` requests, which the proxy rejects.
- Only the Ubuntu archives listed above get mirror switching and caching. Other
  distributions (Debian, ...) are not supported yet; their requests are relayed
  as-is.
- The proxy does not send `Range` requests upstream. A switch costs re-reading
  the part of the file that was already delivered.

## Development

```sh
cd proxy
cargo test
```

`proxy/tests/` contains integration tests against a fake mirror (slow, hanging,
truncating, 404, no Range support). The E2E workflow in `.github/workflows/ci.yml`
exercises the action on Ubuntu 22.04, 24.04 and 26.04 for x64 and arm64.

When the action is used from a ref that has no release (a branch or commit SHA),
it builds the proxy with `cargo`. Releases are created by pushing a `vX.Y.Z` tag;
the release workflow also moves the `vX` tag and its release.

## License

MIT
