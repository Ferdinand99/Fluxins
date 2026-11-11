# Building Inlet

Inlet is a Rust workspace. The pinned toolchain is in `rust-toolchain.toml`; `cargo build` on a machine with the gpui system libraries works as usual. This page describes an optional, reproducible way to build, test and check the project on Linux inside one pinned Docker image.

## Build in Docker

`scripts/docker-build.sh` runs builds and tests in the pinned builder image on a Docker host. The host can be your local Docker or any remote one (select it with the usual `DOCKER_HOST` or `DOCKER_CONTEXT`). Heavy gpui builds take a lot of CPU and memory, so a shared build machine is a good fit.

```sh
scripts/docker-build.sh build                       # cargo build --workspace --locked
scripts/docker-build.sh test                        # cargo test --workspace --locked
scripts/docker-build.sh test --locked -p inlet-emoji
scripts/docker-build.sh clippy                      # --workspace --all-targets --locked -- -D warnings
scripts/docker-build.sh fmt                         # cargo fmt --all --check
scripts/docker-build.sh deny                        # cargo deny --all-features --locked check
scripts/docker-build.sh notices                     # regenerate THIRD-PARTY-NOTICES in your tree
scripts/docker-build.sh notices --check             # fail if the committed file is out of date
scripts/docker-build.sh screenshot                  # demo build under Xvfb, PNGs in target/build-out/screens/
scripts/docker-build.sh fluxer-test                 # tests against a private Fluxer instance (see below)
scripts/docker-build.sh --exec <program> [args]     # any program inside the builder
```

Any other first word is passed to cargo (`scripts/docker-build.sh check -p inlet-protocol`). Arguments you give replace the defaults shown above. The tool output goes to stderr; stdout is reserved for the files the script hands back.

Requirements: `docker`, `git`, `tar` and `bash` (Git Bash on Windows is fine). The script uses whatever Docker host the `docker` command is set up for. Settings you do not want to track (a Docker context, a Docker config directory, the test instance below) can go into an untracked file named `.inlet-build.env` in the repository root, a file of `NAME=value` lines (read as data, not run as shell code; variables already set in the environment take precedence); `INLET_BUILD_ENV` names another file. On Windows with Git Bash the script sets `MSYS_NO_PATHCONV=1` so container paths are not rewritten.

```sh
# .inlet-build.env (not tracked)
DOCKER_CONTEXT=my-build-host
```

### What the script does

- **One builder image.** `docker/builder.Dockerfile` defines it: the Rust toolchain from `rust-toolchain.toml`, the gpui system libraries, clang 21 from apt.llvm.org (for the LiveKit voice crate), cargo-deny, cargo-about 0.9.3 with the `cli` feature, Xvfb with a software Vulkan driver, ImageMagick, xdotool and an emoji font. It is tagged `inlet-builder:<hash>`, where the hash covers `docker/builder.Dockerfile` and `docker/builder-entry.sh`. Changing either file produces a new tag next to the old one, so one person's change never replaces the image another run is using. The first build of a tag takes several minutes; later runs reuse it. When `rust-toolchain.toml` moves to a new version, update the `FROM` line and the `inlet.rust` label in the Dockerfile (the script refuses to run otherwise).
- **No bind mounts, no secrets.** The working tree (tracked files plus untracked files that are not ignored) is streamed to the container as a tar. Nothing from your machine is mounted, and nothing secret is in the image. Run it from the worktree you want to test; `INLET_ROOT` can point at another tree.
- **At most two heavy builds at once per Docker host.** The container waits for one of two build slots. A slot is a lock file in the Docker volume `inlet-locks` (`flock`, released by the kernel when the run ends or dies, so a killed run never blocks a slot). Waiters are served in order. A waiting container prints `waiting for a build slot`; it uses no CPU. `fmt` takes no slot because it needs no dependencies.
- **Two shared caches.** Slot `a` uses the volume `inlet-target-a`, slot `b` uses `inlet-target-b`; each holds the cargo target directory and the cargo home (registry and git checkouts). They are reused by every run and every worktree. Source file modification times are not kept when the tree is unpacked, so a different checkout never gets a stale build; dependencies stay cached and only the workspace crates are re-checked.
- **Limits.** `--cpus 3 --memory 8g` and at most 3 compile jobs per run (`INLET_CPUS`, 1 to 4, and `INLET_MEMORY` change this). The run refuses to start when the Docker host has less than 100 GB free.
- **Labels and names.** Every container has `owner=inlet`, `dev.run` and `dev.title`, and runs with `--rm`. Set `DEV_RUN` (for example `inlet-m1-t12-test`) and `DEV_TITLE` to name the run; the container is named `inlet-<run>-<pid>`. Without `DEV_RUN` the script uses `inlet-adhoc-<command>`.
- **Never prunes.** The script creates only the image, the three volumes and its own containers, all labelled `owner=inlet`. It never prunes and never touches anything else on the host. `scripts/docker-build.sh --status` lists what Inlet owns and what is running.

### Getting files back

`notices` writes the regenerated `THIRD-PARTY-NOTICES` into your tree. `screenshot` writes PNGs to `target/build-out/screens/` (`target/` is ignored by git). For anything else, set `INLET_EXPORT` to space-separated repository paths, for example `INLET_EXPORT="Cargo.lock" scripts/docker-build.sh update -p serde`, and they are copied back after a successful run (`INLET_EXPORT_ALWAYS=1` also after a failed one).

Scripts started with `--exec` run in the repository root and find the cargo target directory at `/src/target`.

`scripts/third-party-notices.sh` sorts the licence blocks, the crates inside them and the overview, so two runs on the same dependencies give identical bytes.

### Screenshots

`scripts/docker-build.sh screenshot` builds the app with `--features demo` (set `INLET_PROFILE=release` for a release build), then runs `scripts/xvfb-screens.sh <binary>` under Xvfb with the lavapipe software renderer. Extra arguments are passed on to that script after the binary path.

CI also compares fixed scenes with committed baselines on Windows, macOS and Linux. When a change alters how one looks, or you add a scene, see "Adding or updating a scene" in [`tests/visual/README.md`](../tests/visual/README.md).

### Checking the setup

`scripts/docker-build.sh --self-test` starts three runs at once and checks that only two hold a slot at the same time, that the third waits, that the labels are present, and that no container is left behind. It occupies both slots for about 30 seconds.

## Test against a private Fluxer instance

`scripts/docker-build.sh fluxer-test` runs `cargo test` against a self-hosted Fluxer test instance with a private CA. You provide the instance; the script needs three settings, usually kept in `.inlet-build.env`:

| Variable | Meaning |
|---|---|
| `INLET_TEST_HOST` | Name of the instance (default `fluxer.test`) |
| `INLET_TEST_HOST_IP` | Address that name resolves to (required) |
| `INLET_TEST_CA_FILE` | Local path of the instance's public CA certificate in PEM form (required) |

The script passes the certificate (a public file; a file holding a private key is refused) into the container, sets `INLET_TEST_EXTRA_CA_FILE` to a copy of it and `INLET_TEST_ORIGIN` to the host name, and starts the container with `--add-host <host>:<ip>`. Without arguments it runs the ignored `test_instance` test of `inlet-protocol` with the `test-extra-ca` feature; with arguments it runs `cargo test <arguments>`. The extra-CA feature exists for tests only and is refused in release builds.

## Environment reference

| Variable | Default | Meaning |
|---|---|---|
| `DEV_RUN`, `DEV_TITLE` | `inlet-adhoc-<command>` | Run id and title (labels `dev.run`, `dev.title`) |
| `INLET_CPUS` | `3` | CPU limit and cargo jobs, 1 to 4 |
| `INLET_MEMORY` | `8g` | Container memory limit |
| `INLET_PASS_ENV` | | Names of variables to pass into the container |
| `INLET_EXPORT`, `INLET_EXPORT_ALWAYS` | | Repository paths to hand back; also after a failed run |
| `INLET_NO_SLOT` | `0` | `1` runs without a slot (light commands only) |
| `INLET_LOCK_TIMEOUT` | `7200` | Seconds to wait for a slot |
| `INLET_PROFILE` | `debug` | `screenshot` build profile |
| `INLET_ROOT` | this repository | Source tree to send |
| `INLET_DOCKER_ARGS` | | Extra arguments for every docker call |
| `INLET_BUILD_ENV` | `.inlet-build.env` | Untracked settings file |
| `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG` | Docker's own | Which Docker host to use |

