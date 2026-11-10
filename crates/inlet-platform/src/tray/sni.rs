//! A StatusNotifierItem with a com.canonical.dbusmenu menu, over zbus.
//!
//! This is the protocol KDE Plasma, most Wayland bars, XFCE, Cinnamon and GNOME
//! with the AppIndicator extension use for tray icons. The icon is sent as pixels
//! (`IconPixmap`), so no icon theme or file on disk is needed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use zbus::blocking::Connection;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::TrayItem;
use crate::events::emit;
use crate::{Error, Icon, PlatformEvent, Result};

pub(crate) const ITEM_PATH: &str = "/StatusNotifierItem";
pub(crate) const MENU_PATH: &str = "/MenuBar";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

type Pixmap = Vec<(i32, i32, Vec<u8>)>;

struct State {
    pixmap: Pixmap,
    tooltip: String,
}

fn pixmap(icon: &Icon) -> Pixmap {
    vec![(
        icon.width() as i32,
        icon.height() as i32,
        icon.to_argb32_be(),
    )]
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Item {
    state: Arc<Mutex<State>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        emit(PlatformEvent::TrayClicked);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(property)]
    fn category(&self) -> String {
        "Communications".into()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        crate::APP_ID.into()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        crate::APP_NAME.into()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmap {
        lock(&self.state).pixmap.clone()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmap, String, String) {
        (
            String::new(),
            Vec::new(),
            lock(&self.state).tooltip.clone(),
            String::new(),
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("constant object path")
    }

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Converts a value without file descriptors; that conversion cannot fail.
fn owned(value: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(value).expect("values without file descriptors convert")
}

type Props = HashMap<String, OwnedValue>;

/// A dbusmenu layout node: id, properties, children (each a variant holding a node).
type Layout = (i32, Props, Vec<OwnedValue>);

/// The menu: id 0 is the root, entry `n` (from 1) is `items[n - 1]`.
struct Menu {
    items: Vec<TrayItem>,
}

impl Menu {
    fn props(&self, id: i32) -> Option<Props> {
        let mut props = Props::new();
        if id == 0 {
            props.insert("children-display".into(), owned(Value::from("submenu")));
            return Some(props);
        }
        match self.items.get(usize::try_from(id).ok()?.checked_sub(1)?)? {
            TrayItem::Action { label, enabled, .. } => {
                // dbusmenu reads "_" as a mnemonic marker; double it to show it.
                props.insert("label".into(), owned(Value::from(label.replace('_', "__"))));
                props.insert("enabled".into(), owned(Value::from(*enabled)));
            }
            TrayItem::Separator => {
                props.insert("type".into(), owned(Value::from("separator")));
            }
        }
        props.insert("visible".into(), owned(Value::from(true)));
        Some(props)
    }

    fn ids(&self) -> std::ops::RangeInclusive<i32> {
        1..=self.items.len() as i32
    }

    fn filtered(&self, id: i32, names: &[String]) -> Option<Props> {
        let mut props = self.props(id)?;
        if !names.is_empty() {
            props.retain(|k, _| names.contains(k));
        }
        Some(props)
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let props = self
            .filtered(parent_id, &property_names)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu id".into()))?;
        let children = if parent_id == 0 && recursion_depth != 0 {
            self.ids()
                .filter_map(|id| {
                    let p = self.filtered(id, &property_names)?;
                    Some(owned(Value::from((id, p, Vec::<OwnedValue>::new()))))
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok((1, (parent_id, props, children)))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Props)> {
        let ids: Vec<i32> = if ids.is_empty() {
            self.ids().collect()
        } else {
            ids
        };
        ids.into_iter()
            .filter_map(|id| Some((id, self.filtered(id, &property_names)?)))
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        self.props(id)
            .and_then(|mut p| p.remove(&name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu id or property".into()))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let index = usize::try_from(id).ok().and_then(|i| i.checked_sub(1));
        if let Some(TrayItem::Action {
            id, enabled: true, ..
        }) = index.and_then(|i| self.items.get(i))
        {
            emit(PlatformEvent::TrayMenu(id.clone()));
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let mut unknown = Vec::new();
        for (id, event_id, data, timestamp) in events {
            if self.props(id).is_none() {
                unknown.push(id);
            } else {
                self.event(id, event_id, data, timestamp);
            }
        }
        unknown
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

pub(crate) struct Tray {
    conn: Connection,
    state: Arc<Mutex<State>>,
}

impl Tray {
    pub(crate) fn new(icon: &Icon, items: &[TrayItem]) -> Result<Self> {
        let conn = Connection::session()?;
        Self::on(conn, icon, items)
    }

    /// Builds the tray on a given connection (the tests use a private bus).
    pub(crate) fn on(conn: Connection, icon: &Icon, items: &[TrayItem]) -> Result<Self> {
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        let state = Arc::new(Mutex::new(State {
            pixmap: pixmap(icon),
            tooltip: crate::APP_NAME.to_string(),
        }));
        conn.object_server().at(
            ITEM_PATH,
            Item {
                state: state.clone(),
            },
        )?;
        conn.object_server().at(
            MENU_PATH,
            Menu {
                items: items.to_vec(),
            },
        )?;
        // The name the specification asks for: org.kde.StatusNotifierItem-<pid>-<n>.
        let name = format!(
            "org.kde.StatusNotifierItem-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        conn.request_name(name.as_str())?;
        let registered = conn.call_method(
            Some(WATCHER),
            "/StatusNotifierWatcher",
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(name.as_str(),),
        );
        if let Err(err) = registered {
            log::info!("no StatusNotifierWatcher: {err}");
            return Err(Error::Unsupported(
                "this desktop has no tray host (StatusNotifierWatcher)",
            ));
        }
        Ok(Self { conn, state })
    }

    pub(crate) fn set_icon(&self, icon: &Icon) -> Result<()> {
        lock(&self.state).pixmap = pixmap(icon);
        let iface = self.conn.object_server().interface::<_, Item>(ITEM_PATH)?;
        zbus::block_on(Item::new_icon(iface.signal_emitter()))?;
        Ok(())
    }

    pub(crate) fn set_tooltip(&self, text: &str) -> Result<()> {
        lock(&self.state).tooltip = text.to_string();
        let iface = self.conn.object_server().interface::<_, Item>(ITEM_PATH)?;
        zbus::block_on(Item::new_tool_tip(iface.signal_emitter()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// A stand-in for the desktop's tray host: it records who registers.
    struct Watcher {
        registered: mpsc::Sender<String>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: String) {
            let _ = self.registered.send(service);
        }
    }

    fn session() -> Option<Connection> {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("skipped: no D-Bus session bus (run under dbus-run-session)");
            return None;
        }
        Some(Connection::session().expect("session bus"))
    }

    fn next_event(rx: &async_channel::Receiver<PlatformEvent>) -> PlatformEvent {
        for _ in 0..200 {
            if let Ok(event) = rx.try_recv() {
                return event;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no platform event");
    }

    /// One test for the whole round trip, because the event queue is shared by the
    /// process and parallel tests would take each other's events.
    #[test]
    fn linux_tray_registers_and_reports_clicks() {
        let Some(host) = session() else { return };
        let (tx, registered) = mpsc::channel();
        host.object_server()
            .at("/StatusNotifierWatcher", Watcher { registered: tx })
            .unwrap();
        host.request_name(WATCHER).unwrap();

        let base = Icon::from_rgba([10, 20, 30, 255].repeat(16 * 16), 16, 16).unwrap();
        let items = vec![
            TrayItem::action("open", "Open Inlet"),
            TrayItem::separator(),
            TrayItem::Action {
                id: "off".into(),
                label: "Disabled".into(),
                enabled: false,
            },
            TrayItem::action("quit", "Quit_Inlet"),
        ];
        let tray = super::super::Tray {
            base: base.clone(),
            inner: Tray::on(Connection::session().unwrap(), &base, &items).unwrap(),
        };
        let service = registered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            service.starts_with("org.kde.StatusNotifierItem-"),
            "{service}"
        );

        let call = |path: &str, iface: &str, method: &str, body: &dyn erased::Body| {
            body.call(&host, &service, path, iface, method)
        };

        // Properties the host reads.
        let id: OwnedValue = call(
            ITEM_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            &("org.kde.StatusNotifierItem", "Id"),
        )
        .body()
        .deserialize()
        .unwrap();
        assert_eq!(String::try_from(id).unwrap(), crate::APP_ID);
        let expected = |state| owned(Value::from(pixmap(&super::super::icon_for(&base, state))));
        assert_eq!(
            icon_pixmap(&call),
            expected(super::super::TrayState::Normal)
        );

        // The menu layout: four children under the root, with labels.
        let reply = call(
            MENU_PATH,
            "com.canonical.dbusmenu",
            "GetLayout",
            &(0i32, -1i32, Vec::<String>::new()),
        );
        let (revision, (root, _props, children)): (u32, Layout) =
            reply.body().deserialize().unwrap();
        assert_eq!((revision, root, children.len()), (1, 0, 4));
        let groups: Vec<(i32, Props)> = call(
            MENU_PATH,
            "com.canonical.dbusmenu",
            "GetGroupProperties",
            &(
                vec![1i32, 2, 4],
                vec!["label".to_string(), "type".to_string()],
            ),
        )
        .body()
        .deserialize()
        .unwrap();
        let label = |p: &Props, k: &str| {
            p.get(k)
                .map(|v| String::try_from(v.try_clone().unwrap()).unwrap())
        };
        assert_eq!(label(&groups[0].1, "label").as_deref(), Some("Open Inlet"));
        assert_eq!(label(&groups[1].1, "type").as_deref(), Some("separator"));
        assert_eq!(label(&groups[2].1, "label").as_deref(), Some("Quit__Inlet"));

        // Clicks become events; disabled entries and separators send nothing.
        let events = crate::events();
        while events.try_recv().is_ok() {}
        let click = |id: i32| {
            call(
                MENU_PATH,
                "com.canonical.dbusmenu",
                "Event",
                &(id, "clicked", Value::from(0i32), 0u32),
            );
        };
        click(2);
        click(3);
        click(4);
        assert_eq!(next_event(&events), PlatformEvent::TrayMenu("quit".into()));
        call(
            ITEM_PATH,
            "org.kde.StatusNotifierItem",
            "Activate",
            &(0i32, 0i32),
        );
        assert_eq!(next_event(&events), PlatformEvent::TrayClicked);
        assert!(events.try_recv().is_err());

        // A new state changes the pixels the host reads.
        tray.set_state(super::super::TrayState::Mention);
        tray.set_tooltip("Inlet \u{2013} 1 mention");
        assert_eq!(
            icon_pixmap(&call),
            expected(super::super::TrayState::Mention)
        );
    }

    fn icon_pixmap(
        call: &dyn Fn(&str, &str, &str, &dyn erased::Body) -> zbus::message::Message,
    ) -> OwnedValue {
        call(
            ITEM_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            &("org.kde.StatusNotifierItem", "IconPixmap"),
        )
        .body()
        .deserialize()
        .unwrap()
    }

    /// Lets the test pass differently typed bodies through one closure.
    mod erased {
        use zbus::blocking::Connection;
        use zbus::message::Message;

        pub(super) trait Body {
            fn call(
                &self,
                conn: &Connection,
                dest: &str,
                path: &str,
                iface: &str,
                method: &str,
            ) -> Message;
        }

        impl<T: serde::Serialize + zbus::zvariant::DynamicType> Body for T {
            fn call(
                &self,
                conn: &Connection,
                dest: &str,
                path: &str,
                iface: &str,
                method: &str,
            ) -> Message {
                conn.call_method(Some(dest), path, Some(iface), method, self)
                    .unwrap_or_else(|err| panic!("{iface}.{method}: {err}"))
            }
        }
    }
}
