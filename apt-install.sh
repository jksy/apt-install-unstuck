#!/usr/bin/env bash
# apt-install-unstuck: apt-get install that detects a stalled mirror, aborts,
# and retries against another mirror without touching the global apt config.
#
# Usage: apt-install.sh [options] [--] package...
# Run with --help for the list of options.
set -euo pipefail

MIRRORS="default archive.ubuntu.com mirrors.edge.kernel.org"
ROUNDS=2
STALL_TIMEOUT=60
ATTEMPT_TIMEOUT=600
INSTALL_RECOMMENDS=false
LOCK_TIMEOUT=120
ISOLATE=true
EXTRA_SOURCES=false
POLL_INTERVAL=5
KILL_AFTER=10
PACKAGES=()

# URIs in the image's sources that point at Ubuntu's own archive (and get
# replaced in isolated attempts).
UBUNTU_URI_RE='mirror\+file:/etc/apt/apt-mirrors\.txt|https?://([a-z0-9-]+\.)*(archive|security|ports)\.ubuntu\.com/ubuntu(-ports)?/?'

usage() {
  cat <<'EOF'
Usage: apt-install.sh [options] [--] package...

Options (defaults in brackets):
  --mirrors LIST            space separated mirrors to try in order; "default"
                            means the image's own apt configuration
                            [default archive.ubuntu.com mirrors.edge.kernel.org]
  --rounds N                how many times to walk the mirror list [2]
  --stall-timeout SEC       abort when the download makes no progress [60]
  --attempt-timeout SEC     hard limit for the network part of one attempt [600]
  --install-recommends BOOL install recommended packages [false]
  --lock-timeout SEC        DPkg::Lock::Timeout [120]
  --isolate BOOL            keep the system apt configuration untouched [true];
                            false prepends the working mirror to
                            /etc/apt/apt-mirrors.txt
  --extra-sources BOOL      keep non-Ubuntu sources in isolated attempts [false]
EOF
}

log() { echo "apt-install-unstuck: $*"; }
warn() {
  if [[ -n ${GITHUB_ACTIONS:-} ]]; then echo "::warning::apt-install-unstuck: $*"; else echo "apt-install-unstuck: warning: $*" >&2; fi
}
die() {
  if [[ -n ${GITHUB_ACTIONS:-} ]]; then echo "::error::apt-install-unstuck: $*"; else echo "apt-install-unstuck: error: $*" >&2; fi
  exit 1
}
group() { if [[ -n ${GITHUB_ACTIONS:-} ]]; then echo "::group::$*"; else log "$*"; fi; }
endgroup() { [[ -n ${GITHUB_ACTIONS:-} ]] && echo "::endgroup::" || true; }

bool() {
  case ${2,,} in
    true | yes | 1) printf -v "$1" true ;;
    false | no | 0 | "") printf -v "$1" false ;;
    *) die "invalid boolean for $opt: $2" ;;
  esac
}
uint() {
  [[ $2 =~ ^[0-9]+$ ]] || die "invalid number for $opt: $2"
  printf -v "$1" %d "$((10#$2))"
}

while (($#)); do
  case $1 in
    -h | --help) usage; exit 0 ;;
    --) shift; PACKAGES+=("$@"); break ;;
    --*=*) opt=${1%%=*}; val=${1#*=}; shift ;;
    --*) (($# >= 2)) || die "missing value for $1"; opt=$1; val=$2; shift 2 ;;
    *) PACKAGES+=("$1"); shift; continue ;;
  esac
  case $opt in
    --mirrors) MIRRORS=$val ;;
    --rounds) uint ROUNDS "$val" ;;
    --stall-timeout) uint STALL_TIMEOUT "$val" ;;
    --attempt-timeout) uint ATTEMPT_TIMEOUT "$val" ;;
    --install-recommends) bool INSTALL_RECOMMENDS "$val" ;;
    --lock-timeout) uint LOCK_TIMEOUT "$val" ;;
    --isolate) bool ISOLATE "$val" ;;
    --extra-sources) bool EXTRA_SOURCES "$val" ;;
    *) die "unknown option: $opt" ;;
  esac
done

((${#PACKAGES[@]})) || die "no packages given"
read -ra MIRROR_LIST <<<"$MIRRORS"
((${#MIRROR_LIST[@]})) || die "no mirrors given"
((ROUNDS >= 1)) || die "rounds must be at least 1"

if [[ $(id -u) -eq 0 ]]; then
  SUDO=()
elif command -v sudo >/dev/null; then
  SUDO=(sudo -n)
else
  die "must run as root or have sudo"
fi
priv() { "${SUDO[@]}" "$@"; }

RECOMMENDS=(--no-install-recommends)
$INSTALL_RECOMMENDS && RECOMMENDS=(--install-recommends)
APT_ENV=(env DEBIAN_FRONTEND=noninteractive)
BASE_OPTS=(-o "DPkg::Lock::Timeout=$LOCK_TIMEOUT")

START=$SECONDS
SUMMARY_ROWS=()
RESULTS=()
OK_MIRROR=
ATTEMPTS=0

write_outputs() {
  local duration=$((SECONDS - START))
  if [[ -n ${GITHUB_OUTPUT:-} ]]; then
    {
      echo "mirror=$OK_MIRROR"
      echo "attempts=$ATTEMPTS"
      echo "duration=$duration"
      echo "results=${RESULTS[*]}"
    } >>"$GITHUB_OUTPUT"
  fi
  if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
    {
      echo "### apt-install-unstuck"
      echo
      echo "Packages: \`${PACKAGES[*]}\`"
      echo
      echo "| # | Mirror | Result | Phase | Duration |"
      echo "|---|--------|--------|-------|----------|"
      printf '%s\n' "${SUMMARY_ROWS[@]}"
      echo
      if [[ -n $OK_MIRROR ]]; then
        echo "Installed from \`$OK_MIRROR\` after $ATTEMPTS attempt(s) in ${duration}s."
      else
        echo "**Failed** after $ATTEMPTS attempt(s) in ${duration}s."
      fi
    } >>"$GITHUB_STEP_SUMMARY"
  fi
}

# --- Non-Ubuntu: plain apt ---------------------------------------------------

# shellcheck disable=SC1091
OS_ID=$(. /etc/os-release 2>/dev/null && echo "${ID:-}") || OS_ID=
if [[ $OS_ID != ubuntu ]]; then
  warn "not Ubuntu (ID=${OS_ID:-unknown}); running plain apt-get without mirror switching"
  t0=$SECONDS
  ATTEMPTS=1
  if priv "${APT_ENV[@]}" apt-get "${BASE_OPTS[@]}" update &&
    priv "${APT_ENV[@]}" apt-get "${BASE_OPTS[@]}" install -y "${RECOMMENDS[@]}" "${PACKAGES[@]}"; then
    OK_MIRROR=default
    SUMMARY_ROWS+=("| 1 | default | ok | install | $((SECONDS - t0))s |")
    RESULTS+=("default=ok")
    write_outputs
    exit 0
  fi
  SUMMARY_ROWS+=("| 1 | default | failed | install | $((SECONDS - t0))s |")
  RESULTS+=("default=failed")
  write_outputs
  die "apt-get failed"
fi

# shellcheck disable=SC1091
VERSION_ID=$(. /etc/os-release && echo "$VERSION_ID")
# shellcheck disable=SC1091
CODENAME=$(. /etc/os-release && echo "${VERSION_CODENAME:-${UBUNTU_CODENAME:-}}")
ARCH=$(dpkg --print-architecture)
# Before 26.04, architectures other than amd64/i386 live on the ports archive.
PORTS=false
case $ARCH in
  amd64 | i386) ;;
  *) dpkg --compare-versions "$VERSION_ID" lt 26.04 && PORTS=true ;;
esac
UPDATE_OPTS=()
dpkg --compare-versions "$(dpkg-query -W -f='${Version}' apt)" ge 2.1.16 && UPDATE_OPTS=(--error-on=any)

# --- Mirrors -----------------------------------------------------------------

resolve_mirror() {
  local m=${1%/}
  case $m in
    *://*) echo "$m" ;;
    */*) echo "https://$m" ;;
    archive.ubuntu.com | *.archive.ubuntu.com | security.ubuntu.com)
      if $PORTS; then echo http://ports.ubuntu.com/ubuntu-ports; else echo "http://$m/ubuntu"; fi ;;
    ports.ubuntu.com) echo http://ports.ubuntu.com/ubuntu-ports ;;
    *) if $PORTS; then echo "https://$m/ubuntu-ports"; else echo "https://$m/ubuntu"; fi ;;
  esac
}

# Returns 1 only when the mirror answers that it does not carry this release.
# Timeouts and other errors are left to the attempt itself.
probe_mirror() {
  command -v curl >/dev/null || return 0
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -r 0-0 "$1/dists/$CODENAME/InRelease" || true)
  [[ $code != 404 && $code != 410 ]]
}

TMPROOT=
cleanup() { [[ -n $TMPROOT ]] && priv rm -rf "$TMPROOT" || true; }
trap cleanup EXIT

# Generates sources pointing at $1 into $TMPROOT; sets ISO_OPTS and LISTS_DIR.
prepare_isolated() {
  local base=$1 f dest
  cleanup
  TMPROOT=$(mktemp -d /tmp/apt-install-unstuck.XXXXXX)
  chmod 755 "$TMPROOT"
  mkdir -p "$TMPROOT/sources.list.d" "$TMPROOT/lists/partial"
  : >"$TMPROOT/sources.list"

  for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
    [[ -f $f ]] || continue
    if [[ $f == /etc/apt/sources.list ]]; then dest=$TMPROOT/sources.list; else dest=$TMPROOT/sources.list.d/${f##*/}; fi
    if grep -Eq "$UBUNTU_URI_RE" "$f"; then
      if [[ $f == *.sources ]] || $EXTRA_SOURCES; then
        sed -E "s#$UBUNTU_URI_RE#$base#g" "$f" >"$dest"
      else
        grep -E "^[[:space:]]*deb(-src)?[[:space:]].*($UBUNTU_URI_RE)" "$f" | sed -E "s#$UBUNTU_URI_RE#$base#g" >"$dest"
      fi
    elif $EXTRA_SOURCES; then
      cp "$f" "$dest"
    fi
  done
  chmod -R a+rX "$TMPROOT"

  # The image layout changed if anything still points at Azure.
  if grep -rhvE '^[[:space:]]*#' "$TMPROOT/sources.list" "$TMPROOT/sources.list.d" | grep -qi 'azure'; then
    grep -rn -i azure "$TMPROOT/sources.list" "$TMPROOT/sources.list.d" >&2 || true
    die "generated sources still reference azure.archive.ubuntu.com; the runner image layout may have changed"
  fi
  if ! grep -rhvE '^[[:space:]]*#' "$TMPROOT/sources.list" "$TMPROOT/sources.list.d" | grep -qF "$base"; then
    die "no Ubuntu archive found in /etc/apt/sources.list or /etc/apt/sources.list.d"
  fi

  if id _apt >/dev/null 2>&1; then
    priv chown _apt "$TMPROOT/lists/partial"
    priv chmod 700 "$TMPROOT/lists/partial"
  fi
  LISTS_DIR=$TMPROOT/lists
  ISO_OPTS=(
    -o "Dir::Etc::SourceList=$TMPROOT/sources.list"
    -o "Dir::Etc::SourceParts=$TMPROOT/sources.list.d"
    -o "Dir::State::Lists=$TMPROOT/lists"
    -o "Dir::Cache::pkgcache="
    -o "Dir::Cache::srcpkgcache="
  )
}

persist_mirror() {
  local base=${1%/} f=/etc/apt/apt-mirrors.txt
  if [[ ! -f $f ]] || ! grep -qsF 'mirror+file:/etc/apt/apt-mirrors.txt' /etc/apt/sources.list /etc/apt/sources.list.d/*; then
    warn "the image does not use $f; not persisting $base"
    return
  fi
  {
    printf '%s/\tpriority:1\n' "$base"
    grep -vF "$base/" "$f" | awk -F'\tpriority:' 'NF == 2 { printf "%s\tpriority:%d\n", $1, $2 + 1; next } { print }'
  } | priv tee "$f.new" >/dev/null
  priv mv "$f.new" "$f"
  log "prepended $base to $f"
}

# --- Watching ----------------------------------------------------------------

alive() {
  local st
  st=$(sed 's/.*) //' "/proc/$1/stat" 2>/dev/null) || return 1
  [[ ${st%% *} != Z ]]
}

descendants() {
  # shellcheck disable=SC2016
  cat /proc/[0-9]*/stat 2>/dev/null | awk -v root="$1" '
    { pid = $1; sub(/.*\) /, ""); kids[$2] = kids[$2] " " pid }
    END {
      n = split(kids[root], q, " ")
      for (i = 1; i <= n; i++) { print q[i]; m = split(kids[q[i]], more, " "); for (j = 1; j <= m; j++) q[++n] = more[j] }
    }'
}

# True while an apt acquire method (http, https, ...) is running under $1, i.e.
# apt is actually fetching rather than waiting for a lock or building caches.
fetching() {
  local p argv0
  for p in $(descendants "$1"); do
    argv0=$(tr '\0' '\n' <"/proc/$p/cmdline" 2>/dev/null | head -n1) || continue
    [[ $argv0 == */apt/methods/* ]] && return 0
  done
  return 1
}

dir_bytes() { priv du -sb "$@" 2>/dev/null | awk '{ s += $1 } END { print s + 0 }'; }

# run_watched TIMEOUT DIR... -- COMMAND...
# Sets WATCH_RESULT to ok, failed, stalled or timeout.
run_watched() {
  local tmo=$1 dirs=() pid rc=0 t0=$SECONDS last_poll=$SECONDS last_size=-1 last_change=$SECONDS size waited stalled=false
  shift
  while [[ $1 != -- ]]; do dirs+=("$1"); shift; done
  shift
  # Background jobs of a non-interactive shell start with SIGINT ignored, and
  # apt would inherit that; restore the default so SIGINT can stop it.
  priv env --default-signal=INT timeout --signal=INT --kill-after="$KILL_AFTER" "$tmo" "$@" &
  pid=$!
  while alive "$pid"; do
    sleep 1
    ((SECONDS - last_poll >= POLL_INTERVAL)) || continue
    last_poll=$SECONDS
    size=$(dir_bytes "${dirs[@]}")
    if [[ $size != "$last_size" ]] || ! fetching "$pid"; then
      last_size=$size
      last_change=$SECONDS
    elif ((SECONDS - last_change >= STALL_TIMEOUT)); then
      warn "no download progress for ${STALL_TIMEOUT}s; interrupting apt"
      stalled=true
      priv kill -INT "$pid" 2>/dev/null || true
      waited=0
      while alive "$pid" && ((waited < KILL_AFTER)); do sleep 1; waited=$((waited + 1)); done
      if alive "$pid"; then
        warn "apt did not exit after SIGINT; killing it"
        # shellcheck disable=SC2046
        priv kill -KILL $(descendants "$pid") "$pid" 2>/dev/null || true
      fi
      break
    fi
  done
  wait "$pid" || rc=$?
  if $stalled; then
    WATCH_RESULT=stalled
  elif ((rc != 0 && SECONDS - t0 >= tmo)); then
    WATCH_RESULT=timeout
  elif ((rc != 0)); then
    WATCH_RESULT=failed
  else
    WATCH_RESULT=ok
  fi
}

# --- Attempts ----------------------------------------------------------------

record() { # mirror result phase seconds
  SUMMARY_ROWS+=("| $ATTEMPTS | \`$1\` | $2 | $3 | $4s |")
  RESULTS+=("$1=$2")
  log "attempt $ATTEMPTS: $1: $2 ($3, $4s)"
}

declare -A MISSING=()
for ((round = 1; round <= ROUNDS; round++)); do
  for token in "${MIRROR_LIST[@]}"; do
    if [[ $token == default ]]; then
      label=default
      ISO_OPTS=()
      LISTS_DIR=$(apt-config shell D Dir::State::Lists/d | sed -E "s/^D='(.*)'$/\1/")
    else
      label=$(resolve_mirror "$token")
      [[ -z ${MISSING[$label]:-} ]] || continue
      if ! probe_mirror "$label"; then
        warn "$label does not carry $CODENAME/$ARCH; skipping"
        MISSING[$label]=1
        SUMMARY_ROWS+=("| - | \`$label\` | skipped (not found) | probe | 0s |")
        continue
      fi
    fi

    ATTEMPTS=$((ATTEMPTS + 1))
    t0=$SECONDS
    group "attempt $ATTEMPTS (round $round): $label"
    [[ $label == default ]] || prepare_isolated "$label"
    OPTS=("${BASE_OPTS[@]}" "${ISO_OPTS[@]}")
    ARCHIVES_DIR=$(apt-config "${OPTS[@]}" shell D Dir::Cache::Archives/d | sed -E "s/^D='(.*)'$/\1/")

    run_watched "$ATTEMPT_TIMEOUT" "$LISTS_DIR" -- "${APT_ENV[@]}" apt-get "${OPTS[@]}" update "${UPDATE_OPTS[@]}"
    phase=update
    if [[ $WATCH_RESULT == ok ]]; then
      left=$((ATTEMPT_TIMEOUT - (SECONDS - t0)))
      if ((left <= 0)); then
        WATCH_RESULT=timeout
      else
        phase=download
        run_watched "$left" "$ARCHIVES_DIR" -- "${APT_ENV[@]}" apt-get "${OPTS[@]}" install -y --download-only "${RECOMMENDS[@]}" "${PACKAGES[@]}"
      fi
    fi
    if [[ $WATCH_RESULT != ok ]]; then
      endgroup
      record "$label" "$WATCH_RESULT" "$phase" "$((SECONDS - t0))"
      continue
    fi

    # Everything is downloaded; the rest is local and not watched.
    if ! priv "${APT_ENV[@]}" apt-get "${OPTS[@]}" install -y --no-download "${RECOMMENDS[@]}" "${PACKAGES[@]}"; then
      endgroup
      record "$label" failed install "$((SECONDS - t0))"
      write_outputs
      die "installation failed (not a mirror problem; not retrying)"
    fi
    endgroup
    record "$label" ok install "$((SECONDS - t0))"
    OK_MIRROR=$label
    if ! $ISOLATE && [[ $label != default ]]; then persist_mirror "$label"; fi
    write_outputs
    exit 0
  done
done

write_outputs
die "all mirrors failed after $ATTEMPTS attempt(s)"
