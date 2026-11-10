#!/usr/bin/env bash
# Build, test and check Inlet in one pinned builder image on a Docker host (local or
# remote). One shared builder image, two shared cache volumes and a lock that lets at
# most two heavy runs go at once on that host.
#
#   scripts/docker-build.sh build [cargo args]       cargo build   (default: --workspace --locked)
#   scripts/docker-build.sh test [cargo args]        cargo test    (default: --workspace --locked)
#   scripts/docker-build.sh clippy [cargo args]      cargo clippy  (default: --workspace --all-targets --locked -- -D warnings)
#   scripts/docker-build.sh fmt [cargo args]         cargo fmt     (default: --all --check; takes no slot)
#   scripts/docker-build.sh deny [cargo-deny args]   cargo deny    (default: --all-features --locked check)
#   scripts/docker-build.sh notices [--check]        regenerate THIRD-PARTY-NOTICES here, or only compare
#   scripts/docker-build.sh screenshot [args]        demo build under Xvfb; PNGs land in target/build-out/screens
#   scripts/docker-build.sh fluxer-test [cargo test args]
#                                                    tests against a private Fluxer test instance
#   scripts/docker-build.sh <other cargo subcommand> [args]   anything else goes to cargo
#   scripts/docker-build.sh --exec <program> [args]  run any program in the builder instead of cargo
#   scripts/docker-build.sh --self-test              check the lock, labels and clean-up (about a minute)
#   scripts/docker-build.sh --status                 what Inlet owns on the Docker host, and what runs
#   scripts/docker-build.sh --build-image            build the builder image only
#
# How it works: the working tree (tracked and untracked-but-not-ignored files) is
# streamed over stdin as a tar into a container (no bind mounts). The container waits
# for one of two build slots, then runs with the slot's cache volume
# inlet-target-<a|b> (cargo target dir and cargo home). The tool output goes to
# stderr; stdout carries the files handed back (THIRD-PARTY-NOTICES, screenshots, and
# anything named in INLET_EXPORT), which this script unpacks into the working tree.
# Everything it starts is labelled owner=inlet and removed on exit (--rm).
#
# Docker host: whatever the docker CLI uses (DOCKER_HOST, DOCKER_CONTEXT, DOCKER_CONFIG).
# Optional settings can live in an untracked file, .inlet-build.env in the repository
# root (or the file named by INLET_BUILD_ENV): NAME=value lines; variables already set
# in the environment take precedence.
#
# Env:
#   DEV_RUN             run id, ^[a-z0-9][a-z0-9-]{0,62}$ (e.g. inlet-m1-t12-test); label dev.run
#                       and container name inlet-<run>-<pid>. Default inlet-adhoc-<command>
#   DEV_TITLE           short title, label dev.title (dashboards that read dev.* labels show it)
#   INLET_CPUS          CPU limit and cargo jobs, 1 to 4 (default 3)
#   INLET_MEMORY        memory limit (default 8g)
#   INLET_PASS_ENV      space-separated names of variables to pass into the container
#   INLET_EXPORT        space-separated repo paths to hand back after a successful run
#   INLET_EXPORT_ALWAYS 1 = hand back after a failed run too
#   INLET_NO_SLOT       1 = run without a build slot (light commands only, own tmpfs)
#   INLET_LOCK_TIMEOUT  seconds to wait for a slot (default 7200)
#   INLET_PROFILE       screenshot: debug (default) or release
#   INLET_ROOT          source tree to send (default: the repo this script is in)
#   INLET_DOCKER_ARGS   extra arguments for every docker call (for example "--context ci")
#   MSYS_NO_PATHCONV    default 1 (Git Bash must not rewrite container paths)
#   fluxer-test:
#   INLET_TEST_HOST     name of the test instance (default fluxer.test)
#   INLET_TEST_HOST_IP  address the name resolves to (required, no default)
#   INLET_TEST_CA_FILE  local path of the instance's public CA certificate (required)
set -euo pipefail

# Settings that must not be tracked (Docker context, config dir, test instance): plain
# NAME=value lines, never run as shell code. Variables already in the environment win.
_env_file="${INLET_BUILD_ENV:-$(cd "$(dirname "$0")/.." && pwd)/.inlet-build.env}"
if [ -f "$_env_file" ]; then
  _dq='"'
  _sq="'"
  while IFS= read -r _line || [ -n "$_line" ]; do
    case "$_line" in
      [A-Za-z_]*=*)
        _name="${_line%%=*}"
        _val="${_line#*=}"
        case "$_val" in
          "$_dq"*"$_dq") _val="${_val#"$_dq"}"; _val="${_val%"$_dq"}" ;;
          "$_sq"*"$_sq") _val="${_val#"$_sq"}"; _val="${_val%"$_sq"}" ;;
        esac
        if printf '%s' "$_name" | grep -Eq '^[A-Za-z_][A-Za-z0-9_]*$' && [ -z "${!_name+x}" ]; then
          export "$_name=$_val"
        fi ;;
    esac
  done < <(tr -d '\r' < "$_env_file")
fi

export MSYS_NO_PATHCONV="${MSYS_NO_PATHCONV:-1}" MSYS2_ARG_CONV_EXCL='*'

script_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$script_dir/.." && pwd)"
root="${INLET_ROOT:-$repo_dir}"

# shellcheck disable=SC2206
docker_args=(${INLET_DOCKER_ARGS:-})
d() { docker "${docker_args[@]}" "$@"; }
die() { echo "docker-build.sh: $*" >&2; exit "${2:-2}"; }

# ---- the builder image -------------------------------------------------------------
# Tag = hash of everything in docker/ that goes into the image, so versions coexist.
hash="$(cat "$repo_dir/docker/builder.Dockerfile" "$repo_dir/docker/builder-entry.sh" | tr -d '\r' | sha256sum | cut -c1-12)"
image="inlet-builder:${hash}"
have_image() { d image inspect "$image" >/dev/null 2>&1; }

check_toolchain() {
  # The image carries one pinned toolchain; refuse to run a tree that wants another.
  local want have
  want="$(sed -n 's/^channel *= *"\(.*\)".*/\1/p' "$root/rust-toolchain.toml" 2>/dev/null | head -1)"
  have="$(sed -n 's/^ *inlet\.rust=\(.*\)$/\1/p' "$repo_dir/docker/builder.Dockerfile" | tr -d '\r' | head -1)"
  if [ -n "$want" ] && [ "$want" != "$have" ]; then
    die "rust-toolchain.toml wants $want but docker/builder.Dockerfile has $have; update its FROM line and the inlet.rust label, then run again"
  fi
}

# docker.exe on Windows needs a Windows path for the build context and Dockerfile.
native_path() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }

build_image() {
  echo "docker-build.sh: building $image (the first build takes several minutes)" >&2
  d build --label owner=inlet -t "$image" -f "$(native_path "$repo_dir/docker/builder.Dockerfile")" "$(native_path "$repo_dir/docker")" >&2
}

ensure_image() { check_toolchain; have_image || build_image; }

# Volumes are created up front and labelled (docker run would create them unlabelled).
ensure_volumes() {
  local v
  for v in inlet-target-a inlet-target-b inlet-locks; do
    d volume inspect "$v" >/dev/null 2>&1 || d volume create --label owner=inlet "$v" >/dev/null
  done
}

# ---- labels and names --------------------------------------------------------------
dev_run_re='^[a-z0-9][a-z0-9-]{0,62}$'
check_dev_run() {
  # bash's own matcher, so an embedded newline cannot slip past the anchors
  if ! (LC_ALL=C; [[ $1 =~ $dev_run_re ]]); then die "DEV_RUN must match $dev_run_re"; fi
}

# ---- status ------------------------------------------------------------------------
status() {
  echo "images (owner=inlet):"
  d image ls --filter label=owner=inlet --format '  {{.Repository}}:{{.Tag}}  {{.Size}}  {{.CreatedSince}}'
  echo "volumes (owner=inlet):"
  d volume ls --filter label=owner=inlet --format '  {{.Name}}'
  echo "containers (owner=inlet, running or waiting for a slot):"
  d ps -a --filter label=owner=inlet \
    --format '  {{.Names}}  {{.Status}}  {{.Label "dev.title"}}'
  echo "(slots: at most 2 of these run at once; the others print 'waiting for a build slot')"
}

# ---- self test ---------------------------------------------------------------------
self_test() {
  ensure_image
  ensure_volumes
  local tmp n fail=0 pids=() names
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  local tag="selftest-$$"
  echo "self-test: three calls at once, each holds a slot for 12 s" >&2
  for n in 1 2 3; do
    DEV_RUN="inlet-$tag-$n" DEV_TITLE="docker-build.sh self-test $n of 3" \
      "$0" --exec sh -c 'echo "inside: $(hostname)"; sleep 12' >"$tmp/out$n" 2>"$tmp/err$n" &
    pids+=($!)
  done

  # While they run: labels must exist on every container of this test.
  local waited=0 seen=0
  while [ "$waited" -lt 90 ]; do
    names="$(d ps -a --filter "label=dev.run" --filter label=owner=inlet --format '{{.Names}}' | grep -E "^inlet-$tag-" || true)"
    seen="$(printf '%s\n' "$names" | grep -c . || true)"
    [ "$seen" -ge 3 ] && break
    sleep 1; waited=$((waited + 1))
  done
  if [ "$seen" -ge 3 ]; then
    local c
    for c in $names; do
      local lab
      lab="$(d inspect --format '{{index .Config.Labels "owner"}}|{{index .Config.Labels "dev.run"}}|{{index .Config.Labels "dev.title"}}' "$c")"
      case "$lab" in
        inlet\|inlet-"$tag"-*\|docker-build.sh\ self-test*) echo "ok: labels on $c: $lab" >&2 ;;
        *) echo "FAIL: labels on $c: $lab" >&2; fail=1 ;;
      esac
    done
  else
    echo "FAIL: only $seen of 3 labelled containers showed up" >&2; fail=1
  fi

  local p
  for p in "${pids[@]}"; do wait "$p" || { echo "FAIL: a call exited non-zero" >&2; fail=1; }; done

  # Slot timeline from the container's own log lines.
  local acq rel
  : >"$tmp/events"
  for n in 1 2 3; do
    acq="$(sed -n 's/.*slot=\([ab]\) acquired epoch=\([0-9.]*\).*/\1 \2/p' "$tmp/err$n" | head -1)"
    rel="$(sed -n 's/.*done rc=[0-9]* epoch=\([0-9.]*\).*/\1/p' "$tmp/err$n" | head -1)"
    if [ -z "$acq" ] || [ -z "$rel" ]; then echo "FAIL: call $n logged no slot events" >&2; sed 's/^/  | /' "$tmp/err$n" >&2; fail=1; continue; fi
    echo "$acq $rel" >>"$tmp/events"   # slot acquired released
  done
  if [ "$(wc -l <"$tmp/events")" -eq 3 ]; then
    # At every acquire time, count the runs that hold a slot; it must never exceed 2.
    local max
    max="$(awk '{a[NR]=$2; r[NR]=$3} END {m=0; for (i=1;i<=NR;i++){c=0; for (j=1;j<=NR;j++) if (a[j]<=a[i] && r[j]>a[i]) c++; if (c>m) m=c} print m}' "$tmp/events")"
    if [ "$max" -le 2 ]; then echo "ok: at most $max runs held a slot at once" >&2; else echo "FAIL: $max runs held a slot at once" >&2; fail=1; fi
    # Two runs on one slot must not overlap.
    if awk '{s[$1]=s[$1] " " $2 " " $3} END {bad=0; for (k in s) {n=split(s[k], f, " "); for (i=1;i<n;i+=2) for (j=i+2;j<n;j+=2) if (f[i] < f[j+1] && f[j] < f[i+1]) bad=1} exit bad}' "$tmp/events"; then
      echo "ok: no two runs shared a slot at the same time" >&2
    else
      echo "FAIL: two runs overlapped on one slot" >&2; fail=1
    fi
    # The third call to get a slot got it only after another had finished.
    local third first_done
    third="$(sort -k2,2n "$tmp/events" | tail -1 | cut -d' ' -f2)"
    first_done="$(sort -k3,3n "$tmp/events" | head -1 | cut -d' ' -f3)"
    if awk -v t="$third" -v d="$first_done" 'BEGIN {exit !(t >= d)}'; then
      echo "ok: the third call waited for a free slot" >&2
    else
      echo "FAIL: the third call got a slot before any run finished" >&2; fail=1
    fi
  fi

  # Nothing of ours may be left behind.
  local left
  left="$(d ps -a --filter label=owner=inlet --format '{{.Names}}' | grep -E "^inlet-$tag-" || true)"
  if [ -z "$left" ]; then echo "ok: no container left behind" >&2; else echo "FAIL: left behind: $left" >&2; fail=1; fi

  if [ "$fail" -eq 0 ]; then echo "self-test passed" >&2; else echo "self-test FAILED" >&2; return 1; fi
}

# ---- argument handling -------------------------------------------------------------
sub="${1:-}"
case "$sub" in
  "" | -h | --help)
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2
    [ -n "$sub" ] && exit 0
    exit 2 ;;
  --status) status; exit 0 ;;
  --build-image) ensure_image; exit 0 ;;
  --self-test) self_test; exit $? ;;
esac

mode=cargo
slot=1
extra_env=()
extra_run=()
case "$sub" in
  --exec)
    shift
    [ "$#" -gt 0 ] || die "usage: scripts/docker-build.sh --exec <program> [args]"
    mode=exec; sub=exec ;;
  build) shift; [ "$#" -gt 0 ] || set -- --workspace --locked; set -- build "$@" ;;
  test) shift; [ "$#" -gt 0 ] || set -- --workspace --locked; set -- test "$@" ;;
  clippy) shift; [ "$#" -gt 0 ] || set -- --workspace --all-targets --locked -- -D warnings; set -- clippy "$@" ;;
  fmt) shift; [ "$#" -gt 0 ] || set -- --all --check; set -- fmt "$@"; slot=0 ;;
  deny) shift; [ "$#" -gt 0 ] || set -- --all-features --locked check; set -- deny "$@" ;;
  notices)
    shift; mode=notices
    case "${1:-}" in
      "") ;;
      --check) extra_env+=(-e INLET_NOTICES_CHECK=1) ;;
      *) die "usage: scripts/docker-build.sh notices [--check]" ;;
    esac
    set -- ;;
  screenshot)
    shift; mode=screenshot
    extra_env+=(-e "INLET_PROFILE=${INLET_PROFILE:-debug}") ;;
  fluxer-test)
    shift
    t_host="${INLET_TEST_HOST:-fluxer.test}"
    t_ip="${INLET_TEST_HOST_IP:-}"
    t_ca="${INLET_TEST_CA_FILE:-}"
    [ -n "$t_ip" ] || die "fluxer-test: set INLET_TEST_HOST_IP to the address of $t_host"
    [ -n "$t_ca" ] || die "fluxer-test: set INLET_TEST_CA_FILE to the local path of the instance's public CA certificate"
    [ -f "$t_ca" ] || die "fluxer-test: $t_ca is not a file"
    if [ "$#" -eq 0 ]; then
      if grep -q '^test-extra-ca' "$root/crates/inlet-protocol/Cargo.toml" 2>/dev/null; then
        set -- -p inlet-protocol --features test-extra-ca --locked --test test_instance -- --ignored --nocapture
      else
        die "fluxer-test: pass the cargo test arguments (the default needs the test-extra-ca feature of inlet-protocol)"
      fi
    fi
    # Only a public certificate goes into the container.
    ca_pem="$(cat "$t_ca")"
    case "$ca_pem" in *"BEGIN CERTIFICATE"*) ;; *) die "fluxer-test: $t_ca is not a PEM certificate" 3 ;; esac
    case "$ca_pem" in *"PRIVATE KEY"*) die "fluxer-test: $t_ca holds a private key; refusing" 3 ;; esac
    extra_env+=(-e "INLET_TEST_CA_PEM=$ca_pem" -e "INLET_TEST_ORIGIN=$t_host")
    extra_run+=(--add-host "$t_host:$t_ip")
    set -- test "$@" ;;
  --*) die "unknown option $sub (see scripts/docker-build.sh --help)" ;;
  *) ;;  # any other cargo subcommand: passed through
esac
[ "${INLET_NO_SLOT:-0}" = 1 ] && slot=0
[ "$mode" = cargo ] || [ "$mode" = exec ] || [ "$mode" = notices ] || [ "$mode" = screenshot ] || die "internal: mode"

# ---- limits, labels, export list ---------------------------------------------------
cpus="${INLET_CPUS:-3}"
memory="${INLET_MEMORY:-8g}"
case "$cpus" in [1-4]) ;; *) die "INLET_CPUS must be 1 to 4 (at most 4 compile jobs per run)" ;; esac

run_id="${DEV_RUN:-inlet-adhoc-$(printf '%s' "$sub" | tr -c 'a-z0-9' '-' | cut -c1-30)}"
check_dev_run "$run_id"
title="${DEV_TITLE:-Inlet $sub}"
title="${title//$'\n'/ }"
case "$run_id" in inlet-*) cname="$run_id-$$" ;; *) cname="inlet-$run_id-$$" ;; esac

pass_env=()
for name in ${INLET_PASS_ENV:-}; do
  printf '%s' "$name" | grep -Eq '^[A-Za-z_][A-Za-z0-9_]*$' || die "INLET_PASS_ENV holds variable names only"
  pass_env+=(-e "$name")
done

export_paths="${INLET_EXPORT:-}"
if [ -n "$export_paths" ]; then
  if ! printf '%s' "$export_paths" | grep -Eq '^[A-Za-z0-9_./ -]+$' \
     || printf '%s' "$export_paths" | grep -Eq '(^| )/|\.\.'; then
    die "INLET_EXPORT must be relative repo paths"
  fi
fi

ensure_image
ensure_volumes

run=(docker "${docker_args[@]}" run --rm -i
  --name "$cname"
  --label owner=inlet
  --label "dev.run=$run_id"
  --label "dev.title=$title"
  --cpus "$cpus"
  --memory "$memory"
  -e INLET_MODE="$mode"
  -e INLET_SLOT="$slot"
  -e INLET_LOCK_TIMEOUT="${INLET_LOCK_TIMEOUT:-7200}"
  -e INLET_EXPORT="$export_paths"
  -e INLET_EXPORT_ALWAYS="${INLET_EXPORT_ALWAYS:-}"
  -e CARGO_BUILD_JOBS="$cpus"
  -e CARGO_TERM_COLOR=never
  -v inlet-locks:/locks
  -v inlet-target-a:/cache/a
  -v inlet-target-b:/cache/b
  --tmpfs /src:exec,size=2g
  --tmpfs /tmp:exec,size=2g
  "${extra_env[@]}"
  "${extra_run[@]}"
  "${pass_env[@]}"
  "$image"
  "$@")

# Tracked and untracked-but-not-ignored files, so .gitignore and .git/info/exclude
# keep local-only folders out.
pack() {
  (cd "$root" && git ls-files -z --cached --others --exclude-standard | tar --null -T - -cf -)
}

# Tool output goes to stderr; stdout is the tar of handed-back files.
set +e
pack | "${run[@]}" | tar -x -m -C "$root" -f - 2>/dev/null
st=("${PIPESTATUS[@]}")
set -e
exit "${st[1]}"
