# Visual checks

CI takes screenshots of fixed states of the demo build on Windows, macOS and Linux and
compares them with the baselines in `baseline/<os>/`. A difference fails the job
(`Windows/macOS/Linux screenshots (demo)`); its artifact `visual-<os>` holds the
screenshots (`actual/`), the differences (`diff/`, differing pixels red, ignored areas
blue) and `report.md`.

- `scenes/*.toml`: one file per state. The app reads the file itself when it is started
  with `INLET_DEMO_SCENE=<file>` (demo builds only). The keys are described in
  `crates/inlet-app/src/demo/scene_file.rs`: `name`, `window`, `scale`, `theme`,
  `ignore` (rectangles that are not compared), what to open (`demo`, `scene`, `guild`,
  `channel`, …) and an optional `tolerance`. CI draws every scene pixel for pixel the
  same, so the comparison is strict: a colour channel may be off by 2 and no pixel may
  differ beyond that. A scene can loosen this only a little and must compare at least
  half of its window; the report says when a scene does.
- `baseline/<os>/<name>.png`: what the scene looked like on that platform's CI runner.
  Fonts and DPI are pinned: the runner images are fixed, the screen is set to
  1920x1080 at one pixel per logical pixel, and macOS and Linux use the bundled fonts.

The scripts that take the screenshots are `scripts/windows-screenshot.ps1`,
`scripts/macos-screenshot.sh` and `scripts/xvfb-screens.sh`; `inlet-visual` (a demo
build tool in `crates/inlet-app/src/bin/visual.rs`) compares them.

## Adding or updating a scene

There are two kinds of screenshots:

- **Numbered demo runs** (`Run-Demo` lines in `scripts/windows-screenshot.ps1`, `scene`
  lines in `scripts/xvfb-screens.sh`): for checking by eye. They are not compared, only
  checked for crashes, panics and borrow errors. For a new screen of your feature, add a
  demo scene in `crates/inlet-app/src/demo/scenes.rs` and a line at the end of each
  list, numbered after the last one.
- **Scene files** (`scenes/*.toml`): compared with the baselines on every pull request.
  Add one only for a state that should never change by accident. A scene file turns
  live demo messages off and holds animated emoji on their first frame
  (`INLET_DEMO_ANIMATE_EMOJI=0`), so a picture does not depend on when it was taken.
  Anything else that moves (a playing GIF or video, a running timer) cannot be gated
  yet: keep it in the numbered demo runs, or freeze it with a demo switch first. The
  emoji, sticker and thread screens (numbered runs 27-33) are not gated yet.

When your change alters how a gated scene looks (for example a new header button),
the screenshot jobs go red; that is expected:

1. Push, and let the `Windows/macOS/Linux screenshots (demo)` jobs run.
2. Open the run's artifacts `visual-<os>`: `report.md`, and `diff/` where the differing
   pixels are red. Every red area must be the change you meant, and nothing else.
3. On the same branch and commit as that run, run
   `scripts/update-visual-baselines.sh <run id>` (GitHub CLI). It shows the run's
   repository, branch and commit, refuses runs from forks, other branches or other
   commits (unless `--other-run`), and copies the screenshots to `baseline/<os>/`.
4. Look at the new pictures and commit them with your change. The next run must be
   green on all three platforms.

To add a scene file: copy an existing one to `scenes/NN-<name>.toml` (numbered after
the last), set `name` and what to open (`scene`, `guild`, `channel`, …), keep
`window = [1280, 860]`, push, and take its baselines from that run as above. A scene
without a baseline fails, and so does a baseline without a scene file (remove both
together when you rename or drop one). Use `ignore` only for an area that really changes
from run to run, never to hide a difference.

Baselines are never taken from a local machine and never updated automatically.
