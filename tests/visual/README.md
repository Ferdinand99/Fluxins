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

## When the look changes on purpose

1. Push the change and let CI fail on the screenshots.
2. Check the artifacts `visual-<os>` of that run: every difference must be wanted.
3. Run `scripts/update-visual-baselines.sh <run id>` (GitHub CLI) on the branch the run
   was made for: it shows the run's repository, branch and commit, refuses runs from
   forks, other branches or other commits (unless `--other-run`), and copies the
   screenshots to `baseline/<os>/`. Look at them and commit them with the change.

Baselines are never taken from a local machine and never updated automatically.

## Adding a scene

Add `scenes/NN-<name>.toml` (numbers after the existing ones), push, and take the
baselines from that run as above. A scene without a baseline fails the check.
