//! Start Inlet when the user logs in. Off unless the user turns it on; nothing in
//! this crate turns it on by itself.
//!
//! As in Fluxer, the login item starts the app with `--autostart`, so the app can
//! tell a login start from a normal one. Inlet also passes `--start-minimized`
//! when the user asked for that, where Fluxer reads its setting at start-up.
//!
//! - Windows: a value in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. If the
//!   user switched the entry off in Task Manager (`StartupApproved\Run`), [`get`]
//!   reports it as off, and turning it on in Inlet clears that switch.
//! - macOS: a LaunchAgent in `~/Library/LaunchAgents`, which needs no extra rights.
//!   Inside a signed app bundle `SMAppService.mainApp` is the better choice; that
//!   comes with packaging.
//! - Linux: a desktop entry in `$XDG_CONFIG_HOME/autostart`. Inside Flatpak this
//!   needs the Background portal instead (not done yet).

use std::path::{Path, PathBuf};

use crate::Result;

/// Argument the login item starts the app with.
pub const AUTOSTART_ARG: &str = "--autostart";
/// Argument added when the user wants the app to start in the tray.
pub const MINIMIZED_ARG: &str = "--start-minimized";

/// Turns starting at login on or off for this executable.
pub fn set(enabled: bool, minimized: bool) -> Result<()> {
    Autostart::for_current_exe()?.set(enabled, minimized)
}

/// Whether starting at login is on.
pub fn get() -> Result<bool> {
    Autostart::for_current_exe()?.get()
}

/// Whether this process was started by the login item.
pub fn launched_at_login<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter().any(|a| a.as_ref() == AUTOSTART_ARG)
}

/// Whether this process should start hidden in the tray: only for a login start
/// that asked for it.
pub fn start_minimized<S: AsRef<str>>(args: &[S]) -> bool {
    launched_at_login(args) && args.iter().any(|a| a.as_ref() == MINIMIZED_ARG)
}

/// The login item of one executable. [`set`] and [`get`] use the running
/// executable and Inlet's id; tests and tools can point it elsewhere.
#[derive(Clone, Debug)]
pub struct Autostart {
    name: String,
    exe: PathBuf,
    #[cfg(unix)]
    dir: Option<PathBuf>,
}

impl Autostart {
    /// `name` is the registry value name, LaunchAgent label or desktop entry name.
    pub fn new(name: impl Into<String>, exe: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            exe: exe.into(),
            #[cfg(unix)]
            dir: None,
        }
    }

    pub fn for_current_exe() -> Result<Self> {
        Ok(Self::new(crate::APP_ID, std::env::current_exe()?))
    }

    /// Uses another folder than `~/Library/LaunchAgents` or
    /// `$XDG_CONFIG_HOME/autostart`.
    #[cfg(unix)]
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// The command line of the login item.
    pub fn args(minimized: bool) -> Vec<&'static str> {
        if minimized {
            vec![AUTOSTART_ARG, MINIMIZED_ARG]
        } else {
            vec![AUTOSTART_ARG]
        }
    }

    fn check(&self) -> Result<()> {
        let ok = !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !ok {
            return Err(crate::Error::InvalidInput("autostart name"));
        }
        if !self.exe.is_absolute() {
            return Err(crate::Error::InvalidInput(
                "executable path must be absolute",
            ));
        }
        Ok(())
    }

    pub fn set(&self, enabled: bool, minimized: bool) -> Result<()> {
        self.check()?;
        if enabled {
            imp::enable(self, minimized)
        } else {
            imp::disable(self)
        }
    }

    pub fn get(&self) -> Result<bool> {
        self.check()?;
        imp::is_enabled(self)
    }

    fn exe(&self) -> &Path {
        &self.exe
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;

    use super::Autostart;
    use crate::{Error, Result};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

    fn not_found(err: &windows::core::Error) -> bool {
        err.code() == ERROR_FILE_NOT_FOUND.to_hresult()
    }

    pub(super) fn command(item: &Autostart, minimized: bool) -> Result<String> {
        let exe = item
            .exe()
            .to_str()
            .ok_or(Error::InvalidInput("executable path is not valid Unicode"))?;
        if exe.contains('"') {
            return Err(Error::InvalidInput("executable path contains a quote"));
        }
        let mut cmd = format!("\"{exe}\"");
        for arg in Autostart::args(minimized) {
            cmd.push(' ');
            cmd.push_str(arg);
        }
        Ok(cmd)
    }

    pub(super) fn enable(item: &Autostart, minimized: bool) -> Result<()> {
        let cmd = command(item, minimized)?;
        windows_registry::CURRENT_USER
            .create(RUN)?
            .set_string(&item.name, cmd)?;
        // An explicit "on" in Inlet overrides an earlier "off" in Task Manager.
        if let Ok(key) = windows_registry::CURRENT_USER.create(APPROVED) {
            match key.remove_value(&item.name) {
                Err(err) if !not_found(&err) => return Err(err.into()),
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn disable(item: &Autostart) -> Result<()> {
        let key = windows_registry::CURRENT_USER.create(RUN)?;
        match key.remove_value(&item.name) {
            Err(err) if !not_found(&err) => Err(err.into()),
            _ => Ok(()),
        }
    }

    pub(super) fn is_enabled(item: &Autostart) -> Result<bool> {
        let Ok(run) = windows_registry::CURRENT_USER.open(RUN) else {
            return Ok(false);
        };
        if run.get_string(&item.name).is_err() {
            return Ok(false);
        }
        // Task Manager stores 12 bytes; an odd first byte means switched off.
        let switched_off = windows_registry::CURRENT_USER
            .open(APPROVED)
            .and_then(|key| key.get_value(&item.name))
            .is_ok_and(|value| value.first().is_some_and(|b| b & 1 == 1));
        Ok(!switched_off)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    use super::Autostart;
    use crate::{Error, Result};

    fn dir(item: &Autostart) -> Result<PathBuf> {
        if let Some(dir) = &item.dir {
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME").ok_or(Error::Unsupported("HOME is not set"))?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents"))
    }

    fn file(item: &Autostart) -> Result<PathBuf> {
        Ok(dir(item)?.join(format!("{}.plist", item.name)))
    }

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    pub(super) fn plist(item: &Autostart, minimized: bool) -> Result<String> {
        let exe = item
            .exe()
            .to_str()
            .ok_or(Error::InvalidInput("executable path is not valid Unicode"))?;
        let mut args = format!("<string>{}</string>", xml_escape(exe));
        for arg in Autostart::args(minimized) {
            args.push_str(&format!("<string>{arg}</string>"));
        }
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\
             <key>Label</key><string>{label}</string>\
             <key>ProgramArguments</key><array>{args}</array>\
             <key>RunAtLoad</key><true/>\
             <key>ProcessType</key><string>Interactive</string>\
             </dict></plist>\n",
            label = xml_escape(&item.name),
        ))
    }

    pub(super) fn enable(item: &Autostart, minimized: bool) -> Result<()> {
        let dir = dir(item)?;
        std::fs::create_dir_all(&dir)?;
        let path = file(item)?;
        let tmp = dir.join(format!(".{}.plist.tmp", item.name));
        std::fs::write(&tmp, plist(item, minimized)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub(super) fn disable(item: &Autostart) -> Result<()> {
        match std::fs::remove_file(file(item)?) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }

    pub(super) fn is_enabled(item: &Autostart) -> Result<bool> {
        Ok(file(item)?.is_file())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;

    use super::Autostart;
    use crate::{xdg, Result};

    fn file(item: &Autostart) -> Result<PathBuf> {
        let dir = match &item.dir {
            Some(dir) => dir.clone(),
            None => xdg::config_home()?.join("autostart"),
        };
        Ok(dir.join(format!("{}.desktop", item.name)))
    }

    pub(super) fn entry(item: &Autostart, minimized: bool) -> Result<String> {
        let mut exec = xdg::exec_arg(item.exe())?;
        for arg in Autostart::args(minimized) {
            exec.push(' ');
            exec.push_str(arg);
        }
        Ok(format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
            name = crate::APP_NAME,
        ))
    }

    pub(super) fn enable(item: &Autostart, minimized: bool) -> Result<()> {
        let path = file(item)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        xdg::write_atomic(&path, entry(item, minimized)?.as_bytes())
    }

    pub(super) fn disable(item: &Autostart) -> Result<()> {
        match std::fs::remove_file(file(item)?) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }

    /// On, unless the entry is missing or switched off with `Hidden=true` or
    /// `X-GNOME-Autostart-enabled=false` (what desktop settings panels write).
    pub(super) fn is_enabled(item: &Autostart) -> Result<bool> {
        let text = match std::fs::read_to_string(file(item)?) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        let value = |key: &str| {
            text.lines().find_map(|l| {
                l.trim()
                    .strip_prefix(key)?
                    .trim_start()
                    .strip_prefix('=')
                    .map(str::trim)
            })
        };
        let hidden = value("Hidden").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let disabled =
            value("X-GNOME-Autostart-enabled").is_some_and(|v| v.eq_ignore_ascii_case("false"));
        Ok(!hidden && !disabled)
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::Autostart;
    use crate::{Error, Result};

    pub(super) fn enable(_item: &Autostart, _minimized: bool) -> Result<()> {
        Err(Error::Unsupported("autostart"))
    }

    pub(super) fn disable(_item: &Autostart) -> Result<()> {
        Ok(())
    }

    pub(super) fn is_enabled(_item: &Autostart) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_arguments() {
        assert!(launched_at_login(&["--autostart"]));
        assert!(!launched_at_login(&["--start-minimized"]));
        assert!(start_minimized(&["--autostart", "--start-minimized"]));
        // Minimised only for a login start.
        assert!(!start_minimized(&["--start-minimized"]));
        assert!(!start_minimized::<&str>(&[]));
    }

    #[test]
    fn names_and_paths_are_checked() {
        let bad_name = Autostart::new("../x", std::env::temp_dir().join("inlet"));
        assert!(bad_name.set(true, false).is_err());
        let relative = Autostart::new("inlet-test", "inlet");
        assert!(relative.get().is_err());
    }

    fn exe() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Program Files\Inlet & Co\inlet.exe")
        } else {
            PathBuf::from("/opt/Inlet & Co/in\"let")
        }
    }

    fn round_trip(item: &Autostart) {
        item.set(false, false).unwrap();
        assert!(!item.get().unwrap(), "off by default");
        item.set(true, false).unwrap();
        assert!(item.get().unwrap());
        item.set(true, true).unwrap();
        assert!(item.get().unwrap());
        item.set(false, false).unwrap();
        assert!(!item.get().unwrap());
        // Turning off twice is fine.
        item.set(false, false).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn autostart_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let item = Autostart::new("inlet-test", exe()).in_dir(dir.path().join("autostart"));
        round_trip(&item);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_entry_and_desktop_switches() {
        let dir = tempfile::tempdir().unwrap();
        let item = Autostart::new("inlet-test", exe()).in_dir(dir.path());
        item.set(true, true).unwrap();
        let path = dir.path().join("inlet-test.desktop");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("Exec=\"/opt/Inlet & Co/in\\\\\"let\" --autostart --start-minimized\n"),
            "{text}"
        );
        // A settings panel that switches it off is respected.
        std::fs::write(
            &path,
            text.replace(
                "X-GNOME-Autostart-enabled=true",
                "X-GNOME-Autostart-enabled=false",
            ),
        )
        .unwrap();
        assert!(!item.get().unwrap());
        std::fs::write(&path, format!("{text}Hidden=true\n")).unwrap();
        assert!(!item.get().unwrap());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_plist_is_escaped() {
        let item = Autostart::new("inlet-test", exe());
        let plist = imp::plist(&item, true).unwrap();
        assert!(plist.contains("<string>/opt/Inlet &amp; Co/in&quot;let</string><string>--autostart</string><string>--start-minimized</string>"), "{plist}");
        assert!(plist.contains("<key>Label</key><string>inlet-test</string>"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_command_line() {
        let item = Autostart::new("inlet-test", exe());
        assert_eq!(
            imp::command(&item, true).unwrap(),
            r#""C:\Program Files\Inlet & Co\inlet.exe" --autostart --start-minimized"#
        );
    }

    /// Writes a value with a test name to the real `HKCU\…\Run` and removes it.
    #[cfg(windows)]
    #[test]
    #[ignore = "touches the real registry; run in CI with --include-ignored"]
    fn autostart_round_trip() {
        let item = Autostart::new(format!("InletTest{}", std::process::id()), exe());
        round_trip(&item);
    }
}
