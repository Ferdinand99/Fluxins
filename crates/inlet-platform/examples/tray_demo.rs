//! Tries the platform layer in a real gpui app: a window, a tray icon with a menu,
//! the badge, a notification, window flashing, single instance and idle time.
//!
//!     cargo run -p inlet-platform --features gpui --example tray_demo
//!     cargo run -p inlet-platform --features gpui --example tray_demo -- --exit-after 10
//!
//! Start it a second time with a link to see it handed to the first:
//!
//!     cargo run -p inlet-platform --features gpui --example tray_demo -- --url -- inlet://channels/@me/42

use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, size, App, Bounds, Context, SharedString, Window, WindowBounds,
    WindowOptions,
};
use inlet_platform::{
    deep_link, gpui_glue, idle_time, set_badge, single_instance, Badge, Icon, Instance,
    Notification, PlatformEvent, Tray, TrayItem, TrayState,
};

/// A filled disc with a lighter ring: the demo's tray icon. On macOS only its
/// shape counts, because the menu bar draws it as a template.
fn demo_icon(side: u32) -> Icon {
    let s = side as f32;
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let (dx, dy) = (x as f32 + 0.5 - s / 2.0, y as f32 + 0.5 - s / 2.0);
            let d = (dx * dx + dy * dy).sqrt();
            let px = if d <= s * 0.30 {
                [0x2f, 0x9e, 0x8f, 0xff]
            } else if d <= s * 0.46 {
                [0x9f, 0xe0, 0xd6, 0xff]
            } else {
                [0, 0, 0, 0]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Icon::from_rgba(rgba, side, side).expect("valid icon")
}

struct Demo {
    lines: Vec<SharedString>,
}

impl Render for Demo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .size_full()
            .bg(rgb(0x1e1f22))
            .text_color(rgb(0xdbdee1))
            .children(self.lines.iter().cloned())
    }
}

fn main() {
    env_logger_lite();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit_after = args
        .iter()
        .position(|a| a == "--exit-after")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse::<u64>().ok());

    let forwarded = match single_instance(inlet_platform::APP_ID) {
        Ok(Instance::Secondary) => {
            println!("another instance is running; handed it {args:?}");
            return;
        }
        Ok(Instance::Primary(rx)) => Some(rx),
        Err(err) => {
            println!("single instance unavailable: {err}");
            None
        }
    };
    if let Some(link) = deep_link::extract(&args) {
        println!("started with link {}", link.to_url());
    }

    let app = gpui_platform::application();
    gpui_glue::install_on_application(&app);
    app.run(move |cx: &mut App| {
        gpui_glue::install(cx);

        let bounds = Bounds::centered(None, size(px(520.), px(260.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    app_id: Some(inlet_platform::APP_ID.into()),
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|_| Demo {
                        lines: vec![
                            "Inlet platform demo".into(),
                            "The tray icon shows a red dot, the badge says 3.".into(),
                            "Use the tray menu, click the notification, or start a second copy with a link.".into(),
                        ],
                    })
                },
            )
            .expect("open the demo window");
        cx.activate(true);

        // Shell calls (tray, taskbar) may dispatch window messages while they run,
        // and gpui's window procedure needs the app, so on Windows they stay out of
        // this `run` callback and out of `cx.update`: in a task, between awaits.
        let events = inlet_platform::events();
        cx.spawn(async move |cx| {
            let menu = vec![
                TrayItem::action("open", "Open Inlet"),
                TrayItem::action("unread", "Show unread"),
                TrayItem::action("normal", "Clear"),
                TrayItem::separator(),
                TrayItem::action("quit", "Quit Inlet"),
            ];
            let tray = match Tray::new(demo_icon(32), menu) {
                Ok(tray) => {
                    tray.set_state(TrayState::Mention);
                    tray.set_tooltip("Inlet \u{2013} 3 mentions");
                    println!("tray icon created");
                    Some(tray)
                }
                Err(err) => {
                    println!("no tray icon: {err}");
                    None
                }
            };
            set_badge(Badge::Count(3));
            println!("idle time: {:?}", idle_time());

            // Platform events, including the tray menu.
            while let Ok(event) = events.recv().await {
                println!("event: {event:?}");
                match event {
                    PlatformEvent::TrayMenu(id) if id == "quit" => cx.update(|cx| cx.quit()),
                    PlatformEvent::TrayMenu(id) if id == "unread" => {
                        if let Some(tray) = &tray {
                            tray.set_state(TrayState::Unread);
                        }
                        set_badge(Badge::Dot);
                    }
                    PlatformEvent::TrayMenu(id) if id == "normal" => {
                        if let Some(tray) = &tray {
                            tray.set_state(TrayState::Normal);
                        }
                        set_badge(None);
                    }
                    _ => {}
                }
            }
        })
        .detach();

        // Command lines from later starts.
        if let Some(rx) = forwarded {
            let (tx, links) = async_channel::unbounded::<Vec<String>>();
            std::thread::spawn(move || {
                while let Ok(args) = rx.recv() {
                    if tx.send_blocking(args).is_err() {
                        break;
                    }
                }
            });
            cx.spawn(async move |_cx| {
                while let Ok(args) = links.recv().await {
                    match deep_link::extract(&args) {
                        Some(link) => println!("second start handed over {}", link.to_url()),
                        None => println!("second start without a link: {args:?}"),
                    }
                }
            })
            .detach();
        }

        // Flash the window after a moment (visible when it is in the background),
        // set the badge once the taskbar button exists, then post a notification.
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            set_badge(Badge::Count(3));
            cx.update(|cx| {
                let _ = window.update(cx, |_, window, _| gpui_glue::flash_window(window));
            });
            println!("posting the notification");
            cx.update(|cx| {
                match Notification::new(
                    "demo:1",
                    "Inlet demo",
                    "This notification came from inlet-platform.",
                ) {
                    Ok(n) => gpui_glue::notify(cx, &n),
                    Err(err) => println!("notification refused: {err}"),
                }
            });
            println!("notification posted");
        })
        .detach();

        if let Some(seconds) = exit_after {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(seconds)).await;
                println!("exiting after {seconds} s");
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
}

fn env_logger_lite() {
    struct Stdout;
    impl log::Log for Stdout {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                println!("[{}] {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: Stdout = Stdout;
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info));
}
