//! The Windows jump list (the taskbar button's right-click menu).
//!
//! Each task starts this executable with fixed arguments (`--dock-action <n>`, say);
//! the running Inlet gets them through [`crate::single_instance`]. The list is built
//! on a thread of its own that has initialised COM (single-threaded apartment), as
//! `ICustomDestinationList` needs; gpui's own jump list runs on a background thread
//! without COM and fails there. [`set_tasks`] returns at once; errors are logged.
//! Other systems have no jump list: `Unsupported` (macOS uses a dock menu).

use crate::Result;

/// One entry under "Tasks".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpTask {
    /// What the menu shows.
    pub title: String,
    /// The command-line arguments the entry starts this executable with.
    pub args: String,
}

/// Replaces the jump list's tasks, from a thread of its own. Only the arguments and
/// titles given here are written, under Inlet's AppUserModelID.
pub fn set_tasks(tasks: Vec<JumpTask>) -> Result<()> {
    imp::set_tasks(tasks)
}

#[cfg(windows)]
mod imp {
    use windows::core::{Interface, GUID, HSTRING};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, ShellLink,
    };

    use super::JumpTask;
    use crate::{Error, Result};

    /// `PKEY_Title` (System.Title), the text a jump list task shows.
    const PKEY_TITLE: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
        pid: 2,
    };

    pub(super) fn set_tasks(tasks: Vec<JumpTask>) -> Result<()> {
        let exe = std::env::current_exe()?;
        std::thread::Builder::new()
            .name("inlet-jump-list".into())
            .spawn(move || {
                // SAFETY: called once on this new thread, balanced below.
                let init = unsafe {
                    CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
                };
                if init.is_err() {
                    log::warn!("jump list: COM could not start: {init:?}");
                    return;
                }
                if let Err(err) = build(&exe, &tasks) {
                    log::warn!("jump list not set: {err}");
                }
                // SAFETY: matches the successful CoInitializeEx on this thread.
                unsafe { CoUninitialize() };
            })
            .map_err(Error::Io)?;
        Ok(())
    }

    pub(super) fn build(exe: &std::path::Path, tasks: &[JumpTask]) -> windows::core::Result<()> {
        let exe = HSTRING::from(exe.as_os_str());
        // SAFETY: plain COM calls on a thread that initialised COM; every interface
        // is a reference-counted wrapper released when it goes out of scope.
        unsafe {
            let list: ICustomDestinationList =
                CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
            list.SetAppID(&HSTRING::from(crate::APP_ID))?;
            let mut slots = 0u32;
            let _removed: IObjectArray = list.BeginList(&mut slots)?;
            let collection: IObjectCollection =
                CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
            for task in tasks {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
                link.SetPath(&exe)?;
                link.SetArguments(&HSTRING::from(task.args.as_str()))?;
                link.SetIconLocation(&exe, 0)?;
                link.SetDescription(&HSTRING::from(task.title.as_str()))?;
                let store: IPropertyStore = link.cast()?;
                store.SetValue(&PKEY_TITLE, &PROPVARIANT::from(task.title.as_str()))?;
                store.Commit()?;
                collection.AddObject(&link)?;
            }
            let array: IObjectArray = collection.cast()?;
            list.AddUserTasks(&array)?;
            list.CommitList()?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::JumpTask;
    use crate::{Error, Result};

    pub(super) fn set_tasks(_tasks: Vec<JumpTask>) -> Result<()> {
        Err(Error::Unsupported("jump lists exist only on Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the real jump list on the test thread's own COM thread and checks that
    /// no error comes back. Windows only; it writes the jump list of the test binary.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes the real jump list; run in CI with --include-ignored"]
    fn the_jump_list_is_built_on_a_com_thread() {
        let exe = std::env::current_exe().unwrap();
        let tasks = vec![JumpTask {
            title: "Open Inlet".into(),
            args: "--dock-action 0".into(),
        }];
        let result = std::thread::spawn(move || {
            use windows::Win32::System::Com::{
                CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
            };
            // SAFETY: balanced below, on this thread.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
            let result = imp_build(&exe, &tasks);
            // SAFETY: matches the call above.
            unsafe { CoUninitialize() };
            result
        })
        .join()
        .unwrap();
        result.unwrap();
    }

    #[cfg(windows)]
    fn imp_build(exe: &std::path::Path, tasks: &[JumpTask]) -> windows::core::Result<()> {
        imp::build(exe, tasks)
    }

    #[cfg(not(windows))]
    #[test]
    fn other_systems_have_no_jump_list() {
        assert!(matches!(
            set_tasks(Vec::new()),
            Err(crate::Error::Unsupported(_))
        ));
    }
}
