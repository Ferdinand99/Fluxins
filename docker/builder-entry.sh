#!/usr/bin/env bash
# Entrypoint of the Inlet builder image (installed as /usr/local/bin/inlet-run).
# Started by scripts/docker-build.sh; not meant to be run by hand.
#
# Flow: unpack the source tar from stdin, wait for a free build slot, run the
# command with its output on stderr, then write the files to export as a tar on
# stdout (stdout is reserved for that tar).
#
# Slots: at most two heavy runs at once on the whole Docker host. Each slot is a lock
# file in the volume mounted at /locks and owns one cache volume (/cache/<slot>)
# that holds the cargo target dir and the cargo home (registry and git checkouts).
# The kernel drops a flock when its holder dies, so a killed run never blocks a slot.
#
# Environment (all set by scripts/docker-build.sh):
#   INLET_MODE          cargo | exec | notices | screenshot
#   INLET_SLOT          1 = take a build slot (default), 0 = no slot (light commands)
#   INLET_LOCK_TIMEOUT  seconds to wait for a slot (default 7200)
#   INLET_MIN_FREE_GB   refuse to start when the disk has less free space (default 100)
#   INLET_EXPORT        space-separated paths under /src to hand back after the run
#   INLET_EXPORT_ALWAYS 1 = export after a failed run too
#   INLET_NOTICES_CHECK 1 = notices mode only compares, does not hand the file back
#   INLET_TEST_CA_PEM   public CA certificate for the test instance (fluxer-test)
set -uo pipefail

# fd 3 is the real stdout (the export tar); everything else goes to stderr.
exec 3>&1 1>&2

say() { echo "inlet-build: $*"; }
die() { say "$*"; exit "${2:-1}"; }

mode="${INLET_MODE:-cargo}"
slot_wanted="${INLET_SLOT:-1}"
timeout="${INLET_LOCK_TIMEOUT:-7200}"
min_free_gb="${INLET_MIN_FREE_GB:-100}"

mkdir -p /src /out /stage

# 1. Source tree, from stdin. mtimes are NOT kept: the slot's target dir is shared
#    by every worktree, and cargo trusts mtimes, so an older file with different
#    content must still count as changed. Dependencies stay cached; only the
#    workspace crates are re-checked, and rustc's incremental cache still helps.
tar -x -m -C /src || die "could not unpack the source tar" 2

# 2. Disk space (the volume mount shows the host's Docker filesystem).
avail_kb="$(df -Pk /locks | awk 'NR==2 {print $4}')"
if [ -n "$avail_kb" ] && [ "$avail_kb" -lt $((min_free_gb * 1024 * 1024)) ]; then
  die "only $((avail_kb / 1024 / 1024)) GB free on the Docker host, need $min_free_gb GB; clean up your own resources first" 3
fi

# 3. A build slot.
slot=""
if [ "$slot_wanted" = 1 ]; then
  say "waiting for a build slot"
  start="$(date +%s)"
  exec 7>>/locks/queue.lock
  # The queue lock keeps waiters in order: only the first one polls the slots.
  flock -w "$timeout" 7 || die "gave up waiting in the queue after ${timeout}s" 4
  while :; do
    for s in a b; do
      exec 8>>"/locks/slot-$s.lock"
      if flock -n 8; then slot="$s"; break; fi
    done
    [ -n "$slot" ] && break
    if [ $(($(date +%s) - start)) -ge "$timeout" ]; then die "no build slot after ${timeout}s" 4; fi
    sleep 2
  done
  exec 7>&-
  say "slot=$slot acquired epoch=$(date +%s.%N) waited=$(($(date +%s) - start))s"
  # Both cache volumes are mounted; this run uses the one of its slot.
  mkdir -p "/cache/$slot/target" "/cache/$slot/cargo"
  ln -s "/cache/$slot/target" /src/target
  export CARGO_HOME="/cache/$slot/cargo"
fi

# Optional CA for the private test instance (a public certificate).
if [ -n "${INLET_TEST_CA_PEM:-}" ]; then
  printf '%s\n' "$INLET_TEST_CA_PEM" > /tmp/inlet-test-ca.crt
  export INLET_TEST_EXTRA_CA_FILE=/tmp/inlet-test-ca.crt
  unset INLET_TEST_CA_PEM
fi

# 4. The command.
cd /src
rc=0
case "$mode" in
  cargo) cargo "$@" || rc=$? ;;
  exec) "$@" || rc=$? ;;
  notices)
    cp THIRD-PARTY-NOTICES /tmp/notices.orig 2>/dev/null || : > /tmp/notices.orig
    scripts/third-party-notices.sh || rc=$?
    if [ "$rc" -eq 0 ] && [ "${INLET_NOTICES_CHECK:-}" = 1 ]; then
      if cmp -s /tmp/notices.orig THIRD-PARTY-NOTICES; then
        say "THIRD-PARTY-NOTICES is current"
      else
        diff -u /tmp/notices.orig THIRD-PARTY-NOTICES | head -40
        say "THIRD-PARTY-NOTICES is out of date; run scripts/docker-build.sh notices"
        rc=1
      fi
    elif [ "$rc" -eq 0 ]; then
      INLET_EXPORT="${INLET_EXPORT:-} THIRD-PARTY-NOTICES"
    fi
    ;;
  screenshot)
    # Demo build (no network, no account), then scripts/xvfb-screens.sh under Xvfb.
    profile_args=()
    bin=target/debug/inlet
    if [ "${INLET_PROFILE:-debug}" = release ]; then profile_args=(--release); bin=target/release/inlet; fi
    if cargo build --locked -p inlet --features demo "${profile_args[@]}"; then
      mkdir -p /out/screens
      scripts/xvfb-screens.sh "$bin" "$@" 3>&- > /tmp/screens.tar || rc=$?
      tar -x -C /out/screens -f /tmp/screens.tar || rc=$?
    else
      rc=$?
    fi
    ;;
  *) die "unknown mode $mode" 2 ;;
esac
say "done rc=$rc epoch=$(date +%s.%N)"

# 5. Hand back files: repo paths (INLET_EXPORT) and /out (lands in target/build-out).
if { [ "$rc" -eq 0 ] || [ "${INLET_EXPORT_ALWAYS:-}" = 1 ]; }; then
  mkdir -p /stage/target/build-out
  cp -a /out/. /stage/target/build-out/
  for p in ${INLET_EXPORT:-}; do
    if [ -e "$p" ]; then cp -a --parents "$p" /stage; fi
  done
  tar -c -C /stage . >&3
fi
exit "$rc"
