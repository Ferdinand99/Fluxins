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
use std::time::Duration;

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
/// How long a caller waits for the notification thread.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

enum Command {
    Show(Notification, mpsc::Sender<Result<()>>),
    Dismiss(String, mpsc::Sender<Result<()>>),
}

fn worker() -> Result<&'static mpsc::Sender<Command>> {
    static WORKER: OnceLock<Option<mpsc::Sender<Command>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
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

fn ask(command: impl FnOnce(mpsc::Sender<Result<()>>) -> Command) -> Result<()> {
    let (reply, answer) = mpsc::channel();
    worker()?
        .send(command(reply))
        .map_err(|_| Error::Os("the notification thread has stopped".into()))?;
    answer
        .recv_timeout(REPLY_TIMEOUT)
        .map_err(|_| Error::Os("the notification thread did not answer".into()))?
}

pub(crate) fn show(notification: &Notification) -> Result<()> {
    let notification = notification.clone();
    ask(|reply| Command::Show(notification, reply))
}

pub(crate) fn dismiss(tag: &str) -> Result<()> {
    let tag = tag.to_string();
    ask(|reply| Command::Dismiss(tag, reply))
}

struct State {
    notifier: Option<ToastNotifier>,
    shown: VecDeque<(String, ToastNotification)>,
}

fn run(commands: mpsc::Receiver<Command>) {
    // SAFETY: initialises WinRT for this thread only, in the multithreaded apartment.
    if let Err(err) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        log::warn!("could not initialise WinRT for notifications: {err}");
    }
    let mut state = State {
        notifier: None,
        shown: VecDeque::new(),
    };
    for command in commands {
        match command {
            Command::Show(notification, reply) => {
                let _ = reply.send(state.show(&notification));
            }
            Command::Dismiss(tag, reply) => {
                let _ = reply.send(state.dismiss(&tag));
            }
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
        let n =
            Notification::new("inlet-test:1", "Inlet test", "From the platform tests.").unwrap();
        show(&n).unwrap();
        dismiss("inlet-test:1").unwrap();
    }
}
