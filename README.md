# apt-install-unstuck

A GitHub Action that keeps `apt-get` on GitHub-hosted Ubuntu runners from
getting stuck on a misbehaving mirror. Add one step; your existing `apt-get`
commands stay as they are.

## The problem

Ubuntu runners fetch packages from `azure.archive.ubuntu.com` (or `ports.ubuntu.com`
on arm64). Every now and then this mirror degrades: transfers crawl at a few tens
of KB/s, or a connection is accepted and then never answers. `apt-get` has no
notion of "too slow" and waits, so a job that normally installs packages in
20 seconds hangs for many minutes or until the job times out.

## Usage

```yaml
- uses: jksy/apt-install-unstuck@v0
- run: sudo apt-get update && sudo apt-get install -y ghostscript imagemagick
```

Every `apt-get` after the action, in any later step of the job, downloads Ubuntu
packages through the proxy. To stop using it, remove the step.

`@v0` follows the latest 0.x release. While the major version is 0, a minor
release may change inputs or behaviour; pin a full version such as `@v0.1.0` to
avoid that.

## How it works

The action starts a small HTTP forward proxy (`unstuck-proxy`, written in Rust)
that runs until the end of the job, and adds one apt setting,
`Acquire::http::Proxy-Auto-Detect`, in `/etc/apt/apt.conf.d/99apt-install-unstuck`.
For each host it connects to, apt asks a small hook installed under
`/usr/local/lib/apt-install-unstuck/` which proxy to use:

- Ubuntu archive hosts (see [Mirrors](#mirrors)) go through the proxy.
- Every other host, including HTTPS sources, PPAs and vendor repositories, goes
  `DIRECT`, exactly as without the action.
- If the proxy is not answering, everything goes `DIRECT`.

The action's post step removes the setting and the hook and stops the proxy,
after waiting for pending cache writes. `sources.list` and `apt-mirrors.txt` are
not touched.

The proxy:

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

Calling the action again later in the same job reuses the running proxy; the
inputs of the later call are ignored.

If apt already has an HTTP proxy configured (for example a corporate proxy on a
self-hosted runner), the action leaves that configuration alone, prints a
warning, and only starts the proxy; use `proxy-url` to opt in explicitly.

### Inputs

| Input | Default | Description |
|---|---|---|
| `cache` | `none` | `none`, `s3://bucket/prefix`, or a local directory. |
| `cache-key-prefix` | `${ImageOS}-${ImageVersion}-${arch}` | Key prefix inside the cache. |
| `mirrors` | | Upstreams per archive (see below). |
| `min-rate` | `50K` | Minimum transfer rate in bytes/s (`K` and `M` are KiB and MiB). |
| `stall-timeout` | `20` | Seconds below `min-rate` before switching mirrors. |
| `rounds` | `2` | How many times to go through the mirror list. |
| `listen` | `127.0.0.1:0` | Address the proxy listens on (port 0 picks a free port). |

### Outputs

| Output | Description |
|---|---|
| `proxy-url` | URL of the proxy, for tools that do not read the apt configuration (e.g. `docker build`). |
| `proxy-port` | Port the proxy listens on. |

The post step writes request counts, cache hits and mirror failovers to the job
summary. While the job runs, `GET <proxy-url>/_stats` returns them as JSON.

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
  - uses: aws-actions/configure-aws-credentials@v6
    with:
      role-to-assume: arn:aws:iam::123456789012:role/apt-cache
      aws-region: ap-northeast-1
  - uses: jksy/apt-install-unstuck@v0
    with:
      cache: s3://my-apt-cache/apt
  - run: sudo apt-get update && sudo apt-get install -y ghostscript
```

### Cloudflare R2

```yaml
  - uses: jksy/apt-install-unstuck@v0
    env:
      AWS_ACCESS_KEY_ID: ${{ secrets.R2_ACCESS_KEY_ID }}
      AWS_SECRET_ACCESS_KEY: ${{ secrets.R2_SECRET_ACCESS_KEY }}
      AWS_REGION: auto
      AWS_ENDPOINT_URL: https://<account-id>.r2.cloudflarestorage.com
    with:
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
  - uses: actions/cache@v6
    with:
      path: ~/.cache/apt-unstuck
      key: apt-${{ runner.os }}-${{ runner.arch }}-${{ github.run_id }}
      restore-keys: apt-${{ runner.os }}-${{ runner.arch }}-
  - uses: jksy/apt-install-unstuck@v0
    with:
      cache: ~/.cache/apt-unstuck
  - run: sudo apt-get update && sudo apt-get install -y ghostscript
```

Post steps run in reverse order, so the proxy finishes writing the directory
before actions/cache saves it.

## Using the proxy from `docker build`

apt inside a container does not read the runner's apt configuration. Pass the
proxy explicitly:

```yaml
  - id: apt
    uses: jksy/apt-install-unstuck@v0
    with:
      listen: 0.0.0.0:3142
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

In a [container job](https://docs.github.com/actions/using-jobs/running-jobs-in-a-container)
(`container:`), the action runs inside the container and configures the
container's apt, so no extra setup is needed there.

To send an `apt-get` command to a different proxy explicitly, disable the hook
for that command: apt prefers `Proxy-Auto-Detect` over `Acquire::http::Proxy`.

```sh
sudo apt-get -o Acquire::http::Proxy-Auto-Detect= -o Acquire::http::Proxy=http://other:3128 update
```

## Limitations

- Only `http://` Ubuntu archive sources go through the proxy. HTTPS sources
  and other hosts are fetched by apt directly, as before. When you point apt at
  `proxy-url` yourself with `Acquire::http::Proxy`, also pass
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
