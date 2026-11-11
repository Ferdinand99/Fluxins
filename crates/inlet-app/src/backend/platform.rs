// SPDX-License-Identifier: AGPL-3.0-or-later
//! Desktop integration through `inlet-platform`: what arrives from the operating system
//! and from later starts of Inlet, the links that come with it, and the desktop settings
//! on disk. The parts here have no gpui and are tested on their own; `crate::desktop`
//! wires them to the window, the tray and the badge.
//!
//! - [`links`]: deep links and invite links. Every link asks before it does anything.
//! - [`window_state`]: where the window was, per monitor.
//! - [`prefs`]: the desktop settings in `settings.json`.

pub mod links;
pub mod prefs;
pub mod window_state;

use std::sync::mpsc;

use futures::channel::mpsc as fmpsc;
use inlet_platform::{deep_link, DeepLink, PlatformEvent};

use super::Backend;

/// Commands of this domain. None: the platform calls run on the UI thread.
pub enum Cmd {}

/// What the desktop integration hands to the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    /// Inlet was started again. The window comes to the front; a link it was started
    /// with is the only thing taken from its command line.
    SecondLaunch(SecondLaunch),
    /// Something the operating system said: a tray click, a tray menu entry, a
    /// notification click, or a link macOS handed to the bundle.
    Os(PlatformEvent),
}

impl Ev {
    /// Whether this event brings the window to the front.
    pub fn shows_window(&self) -> bool {
        match self {
            Ev::SecondLaunch(_) => true,
            Ev::Os(PlatformEvent::TrayClicked | PlatformEvent::NotificationClicked(_)) => true,
            Ev::Os(PlatformEvent::DeepLink(_)) => true,
            Ev::Os(PlatformEvent::TrayMenu(id)) => id == TRAY_OPEN,
        }
    }
}

/// Tray menu entry: show the window.
pub const TRAY_OPEN: &str = "open";
/// Tray menu entry: quit Inlet.
pub const TRAY_QUIT: &str = "quit";

/// The command line of a later start, as far as the first instance looks at it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SecondLaunch {
    /// The `inlet://` link, already validated. Only what follows `--` counts when
    /// there is a `--` (the registered handlers pass the link that way).
    pub link: Option<DeepLink>,
    /// A Windows jump list entry (`--dock-action <n>`, see `inlet_platform::jump_list`).
    /// Only an index into the entries Inlet set itself; anything else is ignored.
    pub dock_action: Option<usize>,
}

/// Jump list entry: show the window.
#[cfg_attr(not(windows), allow(dead_code))]
pub const DOCK_OPEN: usize = 0;
/// Jump list entry: quit Inlet.
pub const DOCK_QUIT: usize = 1;
/// How many jump list entries there are; a larger index is ignored.
pub const DOCK_ENTRIES: usize = 2;

impl SecondLaunch {
    /// Reads a forwarded command line. No other flag of another process is looked at:
    /// it cannot change settings, start hidden, or do anything but show the window and
    /// offer a link.
    pub fn from_args(args: &[String]) -> Self {
        let dock_action = inlet_platform::flag_value(args, "--dock-action")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n < DOCK_ENTRIES);
        SecondLaunch {
            link: deep_link::extract(args),
            dock_action,
        }
    }
}

/// How many forwarded starts may wait for the window. More are dropped: a flood of
/// starts cannot grow memory, and the window only needs to come up once.
pub const FORWARD_QUEUE: usize = 16;

/// Hands the command lines of later starts to the window, from a thread of its own:
/// the single-instance listener gives them on a blocking channel, the window reads a
/// bounded async one. Ends when either side goes away.
pub fn forward_second_launches(
    from: mpsc::Receiver<Vec<String>>,
    mut to: fmpsc::Sender<Ev>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("inlet-second-start".into())
        .spawn(move || {
            while let Ok(args) = from.recv() {
                let launch = SecondLaunch::from_args(&args);
                if let Err(err) = to.try_send(Ev::SecondLaunch(launch)) {
                    if err.is_disconnected() {
                        return;
                    }
                    log::warn!("dropped a forwarded start: too many are waiting");
                }
            }
        })
}

pub(super) fn handle(_backend: &mut Backend, cmd: Cmd) {
    match cmd {}
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use futures::StreamExt;
    use inlet_platform::{Instance, SingleInstance};

    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn next(rx: &mut fmpsc::Receiver<Ev>) -> Ev {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if let Ok(ev) = rx.try_recv() {
                return ev;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("nothing was forwarded");
    }

    /// A second start goes through the real single-instance channel: it exits, and the
    /// first one gets the link and brings its window up.
    #[test]
    fn second_launch_focuses_the_first() {
        let id = format!("no.opland.inlet-test-second-launch-{}", std::process::id());
        let Instance::Primary(listener) = SingleInstance::new(&id).start(&[]).unwrap() else {
            panic!("the first start is not the primary");
        };
        let (tx, mut rx) = fmpsc::channel(FORWARD_QUEUE);
        forward_second_launches(listener, tx).unwrap();

        let second = SingleInstance::new(&id)
            .start(&strings(&["--url", "--", "inlet://channels/1/2"]))
            .unwrap();
        assert!(matches!(second, Instance::Secondary), "{second:?}");
        let ev = next(&mut rx);
        assert!(ev.shows_window());
        assert_eq!(
            ev,
            Ev::SecondLaunch(SecondLaunch {
                link: Some(DeepLink::Channel {
                    guild: Some(1),
                    channel: 2,
                    message: None
                }),
                dock_action: None,
            })
        );

        // A plain second start (no link) brings the window up too.
        let third = SingleInstance::new(&id).start(&[]).unwrap();
        assert!(matches!(third, Instance::Secondary));
        let ev = next(&mut rx);
        assert_eq!(ev, Ev::SecondLaunch(SecondLaunch::default()));
        assert!(ev.shows_window());
    }

    #[test]
    fn flags_of_a_later_start_are_not_read() {
        let launch = SecondLaunch::from_args(&strings(&[
            "--url",
            "--",
            "inlet://channels/1/2\"",
            "--autostart",
            "--dock-action",
            "0",
        ]));
        assert_eq!(launch, SecondLaunch::default());
        let launch = SecondLaunch::from_args(&strings(&["--dock-action", "1"]));
        assert_eq!(launch.dock_action, Some(1));
        let launch = SecondLaunch::from_args(&strings(&["--dock-action", "99"]));
        assert_eq!(launch.dock_action, None);
    }

    #[test]
    fn forwarded_starts_wait_in_a_bounded_queue() {
        let (from_tx, from) = mpsc::channel();
        let (tx, mut rx) = fmpsc::channel(FORWARD_QUEUE);
        let thread = forward_second_launches(from, tx).unwrap();
        for _ in 0..200 {
            from_tx
                .send(strings(&["--url", "--", "inlet://invite/abc"]))
                .unwrap();
        }
        drop(from_tx);
        thread.join().unwrap();
        let mut queued = 0;
        while rx.try_recv().is_ok() {
            queued += 1;
        }
        // The channel holds its capacity plus one slot for its one sender.
        assert!(queued <= FORWARD_QUEUE + 1, "{queued}");
        assert!(queued > 0);
        assert!(futures::executor::block_on(rx.next()).is_none());
    }

    #[test]
    fn tray_entries_and_clicks_show_the_window() {
        assert!(Ev::Os(PlatformEvent::TrayClicked).shows_window());
        assert!(Ev::Os(PlatformEvent::TrayMenu(TRAY_OPEN.into())).shows_window());
        assert!(!Ev::Os(PlatformEvent::TrayMenu(TRAY_QUIT.into())).shows_window());
    }
}
