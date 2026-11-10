//! Windows toasts from this crate's own thread.
//!
//! gpui can show toasts, but its call runs on the UI thread while the app is
//! borrowed, and the WinRT call pumps window messages there: gpui then drops frames
//! ("RefCell already borrowed"). Here a dedicated thread in the multithreaded
//! apartment owns the notifier, so the UI thread never runs WinRT code and never
//! pumps messages for a toast. Clicks come back as
//! [`PlatformEvent::NotificationClicked`].

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::OnceLock;

use windows::core::{h, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager, ToastNotifier};

use crate::events::emit;
use crate::{Error, Notification, PlatformEvent, Result};

/// Toast group for everything Inlet shows, so dismissing finds them.
const GROUP: &str = "inlet";
/// Toasts kept alive so their click handlers stay registered.
const KEEP: usize = 64;
/// Commands waiting for the notification thread. Beyond this the newest is dropped:
/// the UI thread never waits for a toast.
const QUEUE: usize = 64;

enum Command {
    Show(Notification),
    Dismiss(String),
}

fn worker() -> Result<&'static mpsc::SyncSender<Command>> {
    static WORKER: OnceLock<Option<mpsc::SyncSender<Command>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel(QUEUE);
            std::thread::Builder::new()
                .name("inlet-notifications".into())
                .spawn(move || run(rx))
                .map_err(|err| log::warn!("could not start the notification thread: {err}"))
                .ok()?;
            Some(tx)
        })
        .as_ref()
        .ok_or(Error::Unsupported(
            "the notification thread could not start",
        ))
}

/// Queues a command without waiting. A full queue drops it (logged); errors from
/// showing the toast are logged on the notification thread.
fn enqueue(command: Command) -> Result<()> {
    match worker()?.try_send(command) {
        Ok(()) => Ok(()),
        Err(mpsc::TrySendError::Full(_)) => {
            log::warn!("notification queue full; dropped the newest notification");
            Ok(())
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            Err(Error::Os("the notification thread has stopped".into()))
        }
    }
}

pub(crate) fn show(notification: &Notification) -> Result<()> {
    enqueue(Command::Show(notification.clone()))
}

pub(crate) fn dismiss(tag: &str) -> Result<()> {
    enqueue(Command::Dismiss(tag.to_string()))
}

struct State {
    notifier: Option<ToastNotifier>,
    shown: VecDeque<(String, ToastNotification)>,
}

fn init_thread() {
    // SAFETY: initialises WinRT for this thread only, in the multithreaded apartment.
    if let Err(err) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        log::warn!("could not initialise WinRT for notifications: {err}");
    }
}

fn run(commands: mpsc::Receiver<Command>) {
    init_thread();
    let mut state = State::new();
    for command in commands {
        let result = match command {
            Command::Show(notification) => state.show(&notification),
            Command::Dismiss(tag) => state.dismiss(&tag),
        };
        if let Err(err) = result {
            log::warn!("notification failed: {err}");
        }
    }
}

/// An unpackaged app needs its AppUserModelID registered under HKCU before toasts
/// show its name.
fn register_app_id() -> Result<()> {
    let key = windows_registry::CURRENT_USER.create(format!(
        r"Software\Classes\AppUserModelId\{}",
        crate::APP_ID
    ))?;
    key.set_string("DisplayName", crate::APP_NAME)?;
    Ok(())
}

impl State {
    fn new() -> Self {
        Self {
            notifier: None,
            shown: VecDeque::new(),
        }
    }

    fn notifier(&mut self) -> Result<ToastNotifier> {
        if let Some(notifier) = &self.notifier {
            return Ok(notifier.clone());
        }
        register_app_id()?;
        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(crate::APP_ID))?;
        self.notifier = Some(notifier.clone());
        Ok(notifier)
    }

    fn show(&mut self, notification: &Notification) -> Result<()> {
        let notifier = self.notifier()?;
        let toast = ToastNotification::CreateToastNotification(&document(notification)?)?;
        let tag = notification.tag().to_string();
        // Tags are at most 64 ASCII characters, which is the Windows limit.
        toast.SetTag(&HSTRING::from(tag.as_str()))?;
        toast.SetGroup(&HSTRING::from(GROUP))?;
        let clicked = tag.clone();
        toast.Activated(&TypedEventHandler::new(move |_, _| {
            emit(PlatformEvent::NotificationClicked(clicked.clone()));
            Ok(())
        }))?;
        notifier.Show(&toast)?;
        self.shown.retain(|(shown, _)| *shown != tag);
        self.shown.push_back((tag, toast));
        while self.shown.len() > KEEP {
            self.shown.pop_front();
        }
        Ok(())
    }

    fn dismiss(&mut self, tag: &str) -> Result<()> {
        self.shown.retain(|(shown, _)| shown != tag);
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &HSTRING::from(tag),
            &HSTRING::from(GROUP),
            &HSTRING::from(crate::APP_ID),
        )?;
        Ok(())
    }
}

/// The toast XML, built with the DOM so the text can never become markup.
fn document(notification: &Notification) -> Result<XmlDocument> {
    let document = XmlDocument::new()?;
    let toast = document.CreateElement(h!("toast"))?;
    document.AppendChild(&toast)?;
    let visual = document.CreateElement(h!("visual"))?;
    toast.AppendChild(&visual)?;
    let binding = document.CreateElement(h!("binding"))?;
    binding.SetAttribute(h!("template"), h!("ToastGeneric"))?;
    visual.AppendChild(&binding)?;
    for text in [notification.title(), notification.body()] {
        let element = document.CreateElement(h!("text"))?;
        element.SetInnerText(&HSTRING::from(text))?;
        binding.AppendChild(&element)?;
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_holds_the_text_as_text() {
        // SAFETY: test thread; WinRT in the multithreaded apartment.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let n = Notification::new("t:1", "<b>Alice</b>", "a < b & \"c\"").unwrap();
        let xml = document(&n).unwrap().GetXml().unwrap().to_string();
        assert!(xml.contains("&lt;b&gt;Alice&lt;/b&gt;"), "{xml}");
        assert!(xml.contains("a &lt; b &amp;"), "{xml}");
    }

    /// Shows and removes a real toast. The runner has a notification platform.
    #[test]
    #[ignore = "shows a real toast; run in CI with --include-ignored"]
    fn windows_toast_can_be_shown_and_dismissed() {
        // The same steps the notification thread runs, here with their results.
        std::thread::spawn(|| {
            init_thread();
            let mut state = State::new();
            let n = Notification::new("inlet-test:1", "Inlet test", "From the platform tests.")
                .unwrap();
            state.show(&n).unwrap();
            state.dismiss("inlet-test:1").unwrap();
        })
        .join()
        .unwrap();
        // The public calls only queue and never wait.
        let n = Notification::new("inlet-test:2", "Inlet test", "Queued.").unwrap();
        show(&n).unwrap();
        dismiss("inlet-test:2").unwrap();
    }

    #[test]
    fn a_full_queue_drops_instead_of_waiting() {
        let started = std::time::Instant::now();
        let n = Notification::new("inlet-test:q", "Inlet test", "Queue.").unwrap();
        for _ in 0..(QUEUE * 3) {
            // Dismissing an unknown tag is harmless; showing is not needed here.
            let _ = enqueue(Command::Dismiss(n.tag().to_string()));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
