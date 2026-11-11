// SPDX-License-Identifier: AGPL-3.0-or-later
//! Desktop notifications on the UI thread: every new message of every signed-in account
//! goes through the rules in `backend::notifications::alerts`, and what they return is
//! shown through `inlet_platform` (Windows toasts from the platform crate's own thread,
//! gpui on macOS and Linux), with Inlet's own sound and a flash of the taskbar button.
//!
//! The message view hands over the account on screen ([`Inlet::alert_after`] around its
//! `apply`), the account switcher the accounts in the background. A click on a
//! notification ([`Inlet::open_notification`]) shows the window, switches to the
//! notification's account when it is another one, and opens its guild and channel.
//!
//! The settings live in the [`Alerts`] global, so the settings tab can show and change
//! them; they are kept under `"notifications"` in `settings.json`.

use std::path::PathBuf;
use std::time::Instant;

use gpui::{App, Context, Entity, Global, Window};
#[cfg(not(test))]
use inlet_platform::gpui_glue;
use inlet_platform::Notification;
use inlet_protocol::{Dispatch, Message};
use inlet_state::AccountState;

use crate::app::{Inlet, Screen};
use crate::backend::accounts::AccountKey;
use crate::backend::notifications::alerts::{
    self as rules, AlertPrefs, Delivery, Engine, Input, Probe,
};
use crate::backend::notifications::sound::SoundPlayer;
use crate::ui::overlays::account_switcher::Place;

/// The tag of the test notification; a click on it only shows the window.
const TEST_TAG: &str = "inlet-test";

/// The notification rules and settings, the sound and where the settings are kept.
pub struct Alerts {
    pub engine: Engine,
    /// `settings.json`; `None` in demo runs and tests, which write nothing.
    path: Option<PathBuf>,
    /// Started with the first sound.
    #[cfg_attr(test, allow(dead_code))]
    sound: Option<SoundPlayer>,
    /// What was shown, instead of the system, in tests.
    #[cfg(test)]
    pub sent: Vec<(Option<Notification>, bool)>,
    /// How often the window would have flashed, in tests.
    #[cfg(test)]
    pub flashes: usize,
    /// The tags removed, in tests.
    #[cfg(test)]
    pub dismissed: Vec<String>,
}

impl Global for Alerts {}

impl Alerts {
    fn new(prefs: AlertPrefs, path: Option<PathBuf>) -> Self {
        Alerts {
            engine: Engine::new(prefs),
            path,
            sound: None,
            #[cfg(test)]
            sent: Vec::new(),
            #[cfg(test)]
            flashes: 0,
            #[cfg(test)]
            dismissed: Vec::new(),
        }
    }

    pub fn prefs(&self) -> &AlertPrefs {
        &self.engine.prefs
    }
}

/// Demo builds with sample data write nothing.
fn demo_mode() -> bool {
    cfg!(feature = "demo") && std::env::var_os("INLET_DEMO").is_some()
}

/// Loads the settings; call once at start-up.
pub fn init(cx: &mut App) {
    let path = if demo_mode() {
        None
    } else {
        crate::backend::platform::prefs::settings_path()
    };
    let prefs = path.as_deref().map(rules::load_prefs).unwrap_or_default();
    cx.set_global(Alerts::new(prefs, path));
    // Demo screenshots: one test notification, so the Windows run shows a toast.
    #[cfg(feature = "demo")]
    if demo_mode() && std::env::var_os("INLET_DEMO_NOTIFY").is_some() {
        cx.spawn(async |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(3))
                .await;
            cx.update(|cx| send_test_notification(cx));
        })
        .detach();
    }
}

/// The global, made with the defaults when `init` did not run (tests).
fn alerts(cx: &mut App) -> &mut Alerts {
    if !cx.has_global::<Alerts>() {
        cx.set_global(Alerts::new(AlertPrefs::default(), None));
    }
    cx.global_mut::<Alerts>()
}

/// Changes the settings and keeps them.
pub fn change_prefs(cx: &mut App, change: impl FnOnce(&mut AlertPrefs)) {
    let a = alerts(cx);
    change(&mut a.engine.prefs);
    if let Some(path) = &a.path {
        if let Err(err) = rules::save_prefs(path, &a.engine.prefs) {
            log::warn!("could not write the notification settings: {err}");
        }
    }
}

/// A notification with a fixed text, to check that the system shows them.
pub fn send_test_notification(cx: &mut App) {
    match Notification::new(TEST_TAG, &t!("alerts-test-title"), &t!("alerts-test-body")) {
        Ok(n) => {
            let sound = alerts(cx).engine.prefs.sound;
            deliver(
                cx,
                Delivery {
                    notification: Some(n),
                    sound,
                    flash: false,
                },
            )
        }
        Err(err) => log::warn!("test notification: {err}"),
    }
}

/// Shows the notification, plays the sound and flashes the window, outside the update
/// that decided it.
fn deliver(cx: &mut App, d: Delivery) {
    #[cfg(test)]
    {
        let a = alerts(cx);
        a.flashes += usize::from(d.flash);
        a.sent.push((d.notification, d.sound));
    }
    #[cfg(not(test))]
    {
        if d.sound {
            let a = alerts(cx);
            if a.sound.is_none() {
                a.sound = SoundPlayer::start();
            }
            if let Some(player) = &a.sound {
                player.play();
            }
        }
        if let Some(n) = d.notification {
            cx.defer(move |cx| {
                if let Err(err) = gpui_glue::notify(cx, &n) {
                    log::info!("notification not shown: {err}");
                }
            });
        }
        if d.flash {
            crate::desktop::flash_main_window(cx);
        }
    }
}

impl Inlet {
    /// Follows an account's own status (Do Not Disturb), from every event it sends.
    pub(crate) fn alerts_note_status(
        &mut self,
        account: &AccountKey,
        event: &crate::backend::Event,
        cx: &mut Context<Self>,
    ) {
        if let crate::backend::Event::Messages(crate::backend::messages::Ev::Dispatch {
            dispatch,
            ..
        }) = event
        {
            alerts(cx).engine.note_status(Some(account), dispatch);
        }
    }

    /// The read state of a MESSAGE_CREATE's channel before the event is applied.
    pub(crate) fn alert_probe(
        state: &Entity<AccountState>,
        dispatch: &Dispatch,
        cx: &App,
    ) -> Option<Probe> {
        rules::probe(state.read(cx), dispatch)
    }

    /// A MESSAGE_CREATE was applied to `state`, the state of `account` (`None`: the one
    /// on screen without a key): notify when the rules say so.
    pub(crate) fn alert_after(
        &mut self,
        account: Option<AccountKey>,
        probe: Option<Probe>,
        dispatch: &Dispatch,
        state: &Entity<AccountState>,
        cx: &mut Context<Self>,
    ) {
        let (Some(probe), Dispatch::MessageCreate(m)) = (probe, dispatch) else {
            return;
        };
        let on_screen = match &account {
            Some(k) => self.switcher.is_active(k),
            None => true,
        };
        // The open channel and the thread in the side panel count as watched only on
        // screen, with the window in focus.
        let mut focused = Vec::new();
        if on_screen && self.chat_is_visible() && self.notifications.window_active {
            focused.extend(self.open_channel_id());
            focused.extend(self.threads.panel.as_ref().map(|p| p.thread));
        }
        let label = account
            .as_ref()
            .filter(|_| self.switcher.accounts.len() > 1)
            .and_then(|k| self.switcher.accounts.iter().find(|a| &a.key == k))
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty());
        let delivery = self.decide_alert(
            account.as_ref(),
            label.as_deref(),
            probe,
            m,
            &focused,
            state,
            cx,
        );
        if let Some(d) = delivery {
            deliver(cx, d);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_alert(
        &self,
        account: Option<&AccountKey>,
        label: Option<&str>,
        probe: Probe,
        m: &Message,
        focused: &[inlet_state::ChannelId],
        state: &Entity<AccountState>,
        cx: &mut Context<Self>,
    ) -> Option<Delivery> {
        // The engine is taken out of the global while the state is read.
        let mut engine = std::mem::take(&mut alerts(cx).engine);
        let delivery = engine.on_message(
            Input {
                account,
                account_label: label,
                state: state.read(cx),
                probe,
                message: m,
                focused,
            },
            Instant::now(),
        );
        alerts(cx).engine = engine;
        delivery
    }

    /// The open channel of the account on screen was read: its notification goes away
    /// (Windows and macOS; on Linux gpui cannot remove one).
    pub(crate) fn dismiss_channel_alert(
        &mut self,
        channel: inlet_state::ChannelId,
        cx: &mut Context<Self>,
    ) {
        let account = self.switcher.active.clone();
        let Some(tag) = alerts(cx).engine.forget_channel(account.as_ref(), channel) else {
            return;
        };
        #[cfg(test)]
        alerts(cx).dismissed.push(tag);
        #[cfg(not(test))]
        cx.defer(move |cx| {
            if let Err(err) = gpui_glue::dismiss(cx, &tag) {
                log::info!("notification not removed: {err}");
            }
        });
    }

    /// A notification was clicked: its account and channel come up. A click on one the
    /// app no longer knows (shown before a restart, or of an account signed out since)
    /// only shows the window.
    pub(crate) fn open_notification(
        &mut self,
        tag: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = cx
            .try_global::<Alerts>()
            .and_then(|a| a.engine.target(tag))
            .cloned()
        else {
            return;
        };
        if self.screen != Screen::Main {
            return;
        }
        // Everything over the chat closes (layers top down, the About view), so the
        // channel is what comes up.
        self.show_about = false;
        for _ in 0..16 {
            if !self.layer_open() {
                break;
            }
            self.close_top_layer(cx);
        }
        match target.account {
            Some(key) if !self.switcher.is_active(&key) => {
                if !self.switcher.accounts.iter().any(|a| a.key == key) {
                    return;
                }
                // The account opens at the notification's place instead of its last one.
                self.switcher.places.leave(
                    key.clone(),
                    Place {
                        guild: target.guild,
                        channel: Some(target.channel),
                    },
                );
                self.switch_account(key, window, cx);
            }
            _ => self.follow_channel_link(target.guild, target.channel, window, cx),
        }
        // A message in a thread: its channel comes up with the thread in the side panel.
        // The account's state is the one on screen by now, also after a switch.
        if let Some(thread) = target
            .thread
            .as_deref()
            .and_then(inlet_state::ChannelId::parse)
        {
            self.open_thread(thread, window, cx);
        }
        cx.notify();
    }
}

/// The real path, against a mock instance (REST) and a mock gateway: the backend thread
/// with two stored accounts (A active, B started in the background, passive), the
/// gateway events as Fluxer sends them, the view's own event handling, and a click as
/// the platform reports it.
#[cfg(test)]
mod tests {

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use gpui::{div, prelude::*, AnyWindowHandle, Context, Entity, TestAppContext, Window};
    use inlet_platform::{Badge, PlatformEvent, TrayState};
    use serde_json::{json, Value};

    use super::Alerts;
    use crate::app::{Inlet, Screen};
    use crate::backend::accounts::testkit::{mock_instance, serve_gateway_then};
    use crate::backend::accounts::{AccountKey, Accounts};
    use crate::backend::platform::Ev;
    use crate::token_store::{self, AccountList, AccountRecord, MemoryStore};

    /// A window for the test: `Inlet` lives in it as an entity, not drawn.
    struct Probe;

    impl gpui::Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// READY for `user` with guild `guild` and two text channels, `general` and `news`,
    /// and the account's own `status`.
    fn ready(user: &str, guild: &str, general: &str, news: &str, status: &str) -> Value {
        json!({
            "session_id": format!("s{user}"), "version": 1,
            "user": {"id": user, "username": format!("user{user}"), "discriminator": "0001",
                     "avatar": null, "flags": 0},
            "user_settings": {"status": status},
            "guilds": [{"id": guild,
                        "properties": {"id": guild, "name": format!("Guild {guild}"), "owner_id": "9",
                                       "features": []},
                        "channels": [
                            {"id": general, "type": 0, "guild_id": guild, "name": "general", "position": 0},
                            {"id": news, "type": 0, "guild_id": guild, "name": "news", "position": 1}
                        ],
                        "roles": [{"id": guild, "name": "@everyone",
                                   "permissions": "2251799813685247", "position": 0}],
                        "members": [{"user": {"id": user, "username": format!("user{user}")},
                                     "roles": [], "joined_at": "2026-01-01T00:00:00+00:00"}],
                        "member_count": 3, "joined_at": "2026-01-01T00:00:00Z"}],
            "private_channels": [], "read_states": [], "user_guild_settings": []
        })
    }

    /// A MESSAGE_CREATE from Grace (99) that mentions `user`.
    fn mention(id: u64, guild: &str, channel: &str, user: &str, text: &str) -> (String, Value) {
        (
            "MESSAGE_CREATE".to_string(),
            json!({
                "id": ((1_800_000_000_000u64 << 22) + id).to_string(),
                "channel_id": channel, "guild_id": guild,
                "author": {"id": "99", "username": "grace", "global_name": "Grace"},
                "type": 0, "content": format!("<@{user}> {text}"),
                "timestamp": "2027-01-15T08:00:00Z",
                "mentions": [{"id": user, "username": format!("user{user}")}],
                "attachments": [], "embeds": []
            }),
        )
    }

    /// Two signed-in accounts on one mock instance: A (user 11, guild 600) is active, B
    /// (user 12, guild 700) runs in the background. After its READY each gateway sends the
    /// events in `then` (by token).
    struct Rig {
        _rt: tokio::runtime::Runtime,
        _server: wiremock::MockServer,
        window: AnyWindowHandle,
        inlet: Entity<Inlet>,
        ka: AccountKey,
        kb: AccountKey,
        list: PathBuf,
    }

    impl Rig {
        fn start(cx: &mut TestAppContext, then: HashMap<String, Vec<(String, Value)>>) -> Rig {
            Self::start_with(cx, "online", then)
        }

        /// As [`Rig::start`], with B's own status `b_status`.
        fn start_with(
            cx: &mut TestAppContext,
            b_status: &str,
            then: HashMap<String, Vec<(String, Value)>>,
        ) -> Rig {
            cx.executor().allow_parking();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let listener = rt
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let (server, instance) = rt.block_on(mock_instance(
                &format!("ws://127.0.0.1:{port}/gateway"),
                &[("tok-a", "11"), ("tok-b", "12")],
            ));
            {
                let _guard = rt.enter();
                serve_gateway_then(
                    listener,
                    HashMap::from([
                        (
                            "tok-a".to_string(),
                            ready("11", "600", "601", "602", "online"),
                        ),
                        (
                            "tok-b".to_string(),
                            ready("12", "700", "701", "702", b_status),
                        ),
                    ]),
                    then,
                );
            }
            let origin = instance.origin().to_string();
            let ka = AccountKey::new(&origin, "11").unwrap();
            let kb = AccountKey::new(&origin, "12").unwrap();
            let store = Arc::new(MemoryStore::default());
            for (k, token) in [(&ka, "tok-a"), (&kb, "tok-b")] {
                token_store::save_session(
                    &*store,
                    &k.entry(),
                    &inlet_protocol::Token::new(token),
                    &instance.session_pins(),
                    false,
                )
                .unwrap();
            }
            let list = std::env::temp_dir()
                .join(format!("inlet-alerts-{}-{port}", std::process::id()))
                .join("accounts.json");
            let record = |k: &AccountKey, name: &str| AccountRecord {
                origin: origin.clone(),
                user_id: k.user_id.to_string(),
                name: name.into(),
                handle: name.into(),
            };
            token_store::save_list(
                &list,
                &AccountList {
                    accounts: vec![record(&ka, "Ann"), record(&kb, "Bea")],
                    active: Some(ka.entry()),
                    legacy_pending: None,
                },
            )
            .unwrap();

            let window = cx.add_window(|_, _| Probe);
            let (store2, list2) = (store.clone(), list.clone());
            let inlet = window
                .update(cx, |_, window, cx| {
                    cx.new(|cx| {
                        Inlet::new_with(window, cx, move |tx| {
                            crate::backend::start_with(tx, move |ui| {
                                Accounts::new(store2, Some(list2), Some(ui))
                            })
                        })
                    })
                })
                .unwrap();
            Rig {
                _rt: rt,
                _server: server,
                window: window.into(),
                inlet,
                ka,
                kb,
                list,
            }
        }

        fn wait(
            &self,
            cx: &mut TestAppContext,
            what: &str,
            done: &dyn Fn(&Inlet, &gpui::App) -> bool,
        ) {
            for _ in 0..600 {
                cx.run_until_parked();
                if cx.update(|cx| {
                    let app: &gpui::App = cx;
                    done(self.inlet.read(app), app)
                }) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("timed out waiting for: {what}");
        }

        fn act(
            &self,
            cx: &mut TestAppContext,
            f: impl FnOnce(&mut Inlet, &mut Window, &mut Context<Inlet>),
        ) {
            let inlet = self.inlet.clone();
            cx.update_window(self.window, |_, window, cx| {
                inlet.update(cx, |this, cx| f(this, window, cx))
            })
            .unwrap();
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.list.parent().unwrap());
        }
    }

    fn sent(cx: &gpui::App) -> Vec<(Option<inlet_platform::Notification>, bool)> {
        cx.try_global::<Alerts>()
            .map(|a| a.sent.clone())
            .unwrap_or_default()
    }

    /// A mention in an account that runs in the background notifies (with the account's
    /// name, as two are signed in), and clicking the notification switches to that
    /// account and opens the channel of the message, not the one it had open.
    #[gpui::test]
    fn background_account_mention_notifies_and_click_switches_account(cx: &mut TestAppContext) {
        let rig = Rig::start(
            cx,
            HashMap::from([(
                "tok-b".to_string(),
                vec![mention(1, "700", "702", "12", "the build is green")],
            )]),
        );
        let kb = rig.kb.clone();
        rig.wait(cx, "A on screen, B in the background", &|i, _| {
            i.screen == Screen::Main && i.switcher.is_active(&rig.ka) && i.account.is_some()
        });
        rig.wait(cx, "the notification of B's mention", &|_, cx| {
            sent(cx).iter().any(|(n, _)| n.is_some())
        });
        let shown = cx.update(|cx| sent(cx));
        assert_eq!(shown.len(), 1, "one notification");
        let (n, sound) = &shown[0];
        let n = n.clone().unwrap();
        assert!(sound, "it plays the sound");
        // With two accounts signed in, the account's name follows the place.
        assert!(
            n.title().starts_with("Grace (#news, Guild 700)"),
            "{}",
            n.title()
        );
        assert!(n.body().contains("the build is green"), "{}", n.body());
        // B is still in the background: nothing switched by itself.
        cx.update(|cx| assert!(rig.inlet.read(cx).switcher.is_active(&rig.ka)));

        // The click, as the platform reports it.
        let tag = n.tag().to_string();
        rig.act(cx, move |this, window, cx| {
            this.on_platform_event(Ev::Os(PlatformEvent::NotificationClicked(tag)), window, cx)
        });
        rig.wait(cx, "B on screen in #news", &|i, _| {
            i.switcher.is_active(&kb)
                && i.selected_guild.as_deref() == Some("700")
                && i.selected_channel.as_deref() == Some("702")
        });
        drop(rig);
    }

    /// The tray and the badge count the mentions of every signed-in account: the active
    /// one's and those of the accounts in the background.
    #[gpui::test]
    fn badge_counts_mentions_across_accounts(cx: &mut TestAppContext) {
        let rig = Rig::start(
            cx,
            HashMap::from([
                (
                    "tok-a".to_string(),
                    vec![mention(1, "600", "602", "11", "one for Ann")],
                ),
                (
                    "tok-b".to_string(),
                    vec![
                        mention(2, "700", "701", "12", "one for Bea"),
                        mention(3, "700", "702", "12", "and another"),
                    ],
                ),
            ]),
        );
        rig.wait(cx, "A on screen, B in the background", &|i, _| {
            i.screen == Screen::Main && i.switcher.is_active(&rig.ka) && i.account.is_some()
        });
        rig.wait(cx, "three mentions on the badge", &|i, cx| {
            i.desktop_unread(cx).mentions == 3
        });
        let unread = cx.update(|cx| rig.inlet.read(cx).desktop_unread(cx));
        assert_eq!(unread.badge(), Badge::Count(3));
        assert_eq!(unread.tray_state(), TrayState::Mention);
        // The switcher shows B's two on B's row.
        cx.update(|cx| assert_eq!(rig.inlet.read(cx).switcher.background_mentions(), 2));
        // Each mention notified once.
        rig.wait(cx, "three notifications", &|_, cx| sent(cx).len() == 3);
        drop(rig);
    }

    /// Do Not Disturb of an account in the background, from its READY, silences its
    /// mention; the settings update that ends it (as the echo of a status change) lets
    /// the next one through. A click on that notification after the account was signed
    /// out does nothing but show the window.
    #[gpui::test]
    fn background_dnd_is_followed_and_a_stale_click_does_nothing(cx: &mut TestAppContext) {
        let rig = Rig::start_with(
            cx,
            "dnd",
            HashMap::from([(
                "tok-b".to_string(),
                vec![
                    mention(1, "700", "702", "12", "while in dnd"),
                    (
                        "USER_SETTINGS_UPDATE".to_string(),
                        json!({"status": "online"}),
                    ),
                    mention(2, "700", "702", "12", "after dnd"),
                ],
            )]),
        );
        let ka = rig.ka.clone();
        let kb = rig.kb.clone();
        rig.wait(cx, "both mentions counted for B", &|i, _| {
            i.screen == Screen::Main
                && i.switcher.is_active(&rig.ka)
                && i.switcher.background_mentions() == 2
        });
        rig.wait(cx, "one notification", &|_, cx| !sent(cx).is_empty());
        let shown = cx.update(|cx| sent(cx));
        assert_eq!(shown.len(), 1, "only the mention after DND notifies");
        let n = shown[0].0.clone().unwrap();
        assert!(n.body().contains("after dnd"), "{}", n.body());

        // B is signed out; its notification is clicked afterwards.
        rig.act(cx, move |this, _, cx| {
            this.on_account_removed(kb.clone(), None, cx)
        });
        let tag = n.tag().to_string();
        rig.act(cx, move |this, window, cx| {
            this.on_platform_event(Ev::Os(PlatformEvent::NotificationClicked(tag)), window, cx)
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let i = rig.inlet.read(cx);
            assert!(i.switcher.is_active(&ka));
            assert!(i.screen == Screen::Main);
        });
        drop(rig);
    }
}
