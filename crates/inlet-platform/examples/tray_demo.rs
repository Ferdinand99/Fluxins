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
    deep_link, flag_value, gpui_glue, idle_time, single_instance, Badge, Icon, Instance,
    Notification, PlatformEvent, TrayItem, TrayState,
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
    // Options only before `--`: a link that broke out of its quotes cannot set them.
    let exit_after = flag_value(&args, "--exit-after").and_then(|n| n.parse::<u64>().ok());

    let forwarded = match single_instance(inlet_platform::APP_ID) {
        Ok(Instance::Secondary) => {
            println!("another instance is running; handed it the command line");
            return;
        }
        Ok(Instance::Primary(rx)) => rx,
        // Never carry on as a second primary: another instance may be running.
        Err(err) => {
            eprintln!("Inlet is already running but does not respond ({err}).");
            std::process::exit(1);
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

        // The gpui_glue calls are safe from anywhere, also from this callback and
        // inside `cx.update`: tray and badge calls run in their own task, and
        // Windows toasts come from the crate's notification thread.
        let menu = vec![
            TrayItem::action("open", "Open Inlet"),
            TrayItem::action("unread", "Show unread"),
            TrayItem::action("normal", "Clear"),
            TrayItem::separator(),
            TrayItem::action("quit", "Quit Inlet"),
        ];
        let tray_task = gpui_glue::create_tray(cx, demo_icon(32), menu);
        gpui_glue::set_badge(cx, Badge::Count(3));

        let events = inlet_platform::events();
        cx.spawn(async move |cx| {
            let tray = match tray_task.await {
                Ok(tray) => {
                    println!("tray icon created");
                    cx.update(|cx| {
                        gpui_glue::set_tray_state(cx, &tray, TrayState::Mention);
                        gpui_glue::set_tray_tooltip(cx, &tray, "Inlet \u{2013} 3 mentions");
                    });
                    Some(tray)
                }
                Err(err) => {
                    println!("no tray icon: {err}");
                    None
                }
            };
            println!("idle time: {:?}", idle_time());

            // Platform events, including the tray menu.
            while let Ok(event) = events.recv().await {
                println!("event: {event:?}");
                cx.update(|cx| match event {
                    PlatformEvent::TrayMenu(id) if id == "quit" => cx.quit(),
                    PlatformEvent::TrayMenu(id) if id == "unread" => {
                        if let Some(tray) = &tray {
                            gpui_glue::set_tray_state(cx, tray, TrayState::Unread);
                        }
                        gpui_glue::set_badge(cx, Badge::Dot);
                    }
                    PlatformEvent::TrayMenu(id) if id == "normal" => {
                        if let Some(tray) = &tray {
                            gpui_glue::set_tray_state(cx, tray, TrayState::Normal);
                        }
                        gpui_glue::set_badge(cx, None);
                    }
                    _ => {}
                });
            }
        })
        .detach();

        // Command lines from later starts.
        let (tx, handed_over) = async_channel::unbounded::<Vec<String>>();
        std::thread::spawn(move || {
            while let Ok(args) = forwarded.recv() {
                if tx.send_blocking(args).is_err() {
                    break;
                }
            }
        });
        cx.spawn(async move |_cx| {
            while let Ok(args) = handed_over.recv().await {
                match deep_link::extract(&args) {
                    Some(link) => println!("second start handed over {}", link.to_url()),
                    None => println!("second start without a link"),
                }
            }
        })
        .detach();

        // After a moment: flash the window (visible when it is in the background),
        // set the badge again now that the taskbar button exists, and post a
        // notification, all from inside `cx.update`.
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            println!("posting the notification");
            cx.update(|cx| {
                gpui_glue::set_badge(cx, Badge::Count(3));
                let _ = window.update(cx, |_, window, _| gpui_glue::flash_window(window));
                let posted = Notification::new(
                    "demo:1",
                    "Inlet demo",
                    "This notification came from inlet-platform.",
                )
                .and_then(|n| gpui_glue::notify(cx, &n));
                if let Err(err) = posted {
                    println!("notification not shown: {err}");
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
