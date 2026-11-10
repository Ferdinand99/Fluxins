// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sets `INLET_SOURCE_REV` for the About view's source link: the value from the
//! environment (CI sets it), else the commit from `git rev-parse HEAD`, else `v0`.
//! That part never fails the build: git may be missing or this may not be a checkout.
//!
//! On Windows it also embeds the app icon (`assets/brand/windows/inlet.ico`) and the
//! version info in `inlet.exe`. The icon is resource 1, which gpui loads for its windows.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=INLET_SOURCE_REV");
    let rev = std::env::var("INLET_SOURCE_REV")
        .ok()
        .filter(|r| !r.trim().is_empty())
        .or_else(git_head)
        .unwrap_or_else(|| "v0".to_string());
    println!("cargo:rustc-env=INLET_SOURCE_REV={}", rev.trim());

    #[cfg(windows)]
    windows_resources();
}

/// Icon and version info for `inlet.exe`. Needs the resource compiler from the
/// Windows SDK, which the MSVC toolchain installs anyway.
#[cfg(windows)]
fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    // Forward slashes: the path ends up in a .rc file, where backslashes are escapes.
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let icon = format!(
        "{}/../../assets/brand/windows/inlet.ico",
        dir.replace('\\', "/")
    );
    println!("cargo:rerun-if-changed={icon}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon)
        .set("ProductName", "Inlet")
        .set("FileDescription", "Inlet");
    res.compile()
        .expect("embed the icon and version info in inlet.exe");
}

fn git_head() -> Option<String> {
    let run = |args: &[&str]| {
        let out = Command::new("git").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let rev = run(&["rev-parse", "HEAD"])?;
    // Rebuild when HEAD moves (new commit or checkout).
    if let Some(git_dir) = run(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/refs/heads");
    }
    rev.chars().all(|c| c.is_ascii_hexdigit()).then_some(rev)
}
