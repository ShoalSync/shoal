//! The notification-area icon and its menu (ADR 0042), with `native-windows-gui`.
//!
//! The UI thread only shows: the supervisor runs on a thread of its own, with the runtime that
//! talks to the daemon, and wakes the UI with a notice when it has something new. Nothing on the
//! UI thread waits on the daemon, whose pipe can be slow to answer while it starts.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use native_windows_gui as nwg;
use tokio::sync::mpsc;

use crate::supervisor::{self, Command, Paths, Snapshot};
use crate::view::{view, View};

/// What a menu item does.
enum Action {
    Open(PathBuf),
    Pause,
    Resume,
    Restart,
    Quit,
}

/// A menu shown, and its items with what each does.
type Shown = (nwg::Menu, Vec<(nwg::MenuItem, Option<Action>)>);

struct Tray {
    window: nwg::MessageWindow,
    _icon: nwg::Icon,
    tray: nwg::TrayNotification,
    notice: nwg::Notice,
    /// The menu last shown, and its items with what each does; made again each time it opens.
    /// Kept until then: dropping an item removes it.
    menu: RefCell<Option<Shown>>,
    latest: Arc<Mutex<Option<Snapshot>>>,
    shown: RefCell<View>,
    commands: mpsc::Sender<Command>,
    quitting: RefCell<bool>,
}

/// Runs the tray until it is quit, supervising the daemon at `paths`.
pub fn run(paths: Paths) -> Result<(), String> {
    run_with(paths, |_| {})
}

/// [`run`], giving `started` a way to ask what the menu asks, once the icon shows.
pub fn run_with(paths: Paths, started: impl FnOnce(mpsc::Sender<Command>)) -> Result<(), String> {
    nwg::init().map_err(|e| e.to_string())?;
    let mut window = nwg::MessageWindow::default();
    nwg::MessageWindow::builder()
        .build(&mut window)
        .map_err(|e| e.to_string())?;
    let mut icon = nwg::Icon::default();
    nwg::Icon::builder()
        .source_system(Some(nwg::OemIcon::Information))
        .build(&mut icon)
        .map_err(|e| e.to_string())?;
    let mut tray = nwg::TrayNotification::default();
    nwg::TrayNotification::builder()
        .parent(&window)
        .icon(Some(&icon))
        .tip(Some("Tidefell is starting"))
        .build(&mut tray)
        .map_err(|e| e.to_string())?;
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&window)
        .build(&mut notice)
        .map_err(|e| e.to_string())?;

    let latest = Arc::new(Mutex::new(None));
    let (commands, receiver) = mpsc::channel(8);
    let supervisor = std::thread::spawn({
        let (latest, sender) = (latest.clone(), notice.sender());
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(supervisor::supervise(paths, receiver, move |snapshot| {
                *latest.lock().unwrap() = Some(snapshot);
                sender.notice();
            }));
            // Gone: the UI stops with it.
            sender.notice();
        }
    });

    let app = Rc::new(Tray {
        window,
        _icon: icon,
        tray,
        notice,
        menu: RefCell::new(None),
        latest,
        shown: RefCell::new(view(&crate::view::Daemon::Starting, None, &[])),
        commands,
        quitting: RefCell::new(false),
    });
    let handler = nwg::full_bind_event_handler(&app.window.handle, {
        let app = Rc::downgrade(&app);
        move |event, _, handle| {
            let Some(app) = app.upgrade() else {
                return;
            };
            match event {
                nwg::Event::OnNotice if handle == app.notice.handle => app.refresh(),
                nwg::Event::OnContextMenu if handle == app.tray.handle => app.show_menu(),
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == app.tray.handle =>
                {
                    app.show_menu()
                }
                nwg::Event::OnMenuItemSelected => app.chosen(handle),
                _ => {}
            }
        }
    });
    started(app.commands.clone());
    nwg::dispatch_thread_events();
    nwg::unbind_event_handler(&handler);
    drop(app);
    let _ = supervisor.join();
    Ok(())
}

impl Tray {
    /// Shows what the supervisor saw last; stops once it has quit.
    fn refresh(&self) {
        if *self.quitting.borrow() && !self.supervising() {
            nwg::stop_thread_dispatch();
            return;
        }
        let Some(snapshot) = self.latest.lock().unwrap().take() else {
            if !self.supervising() {
                nwg::stop_thread_dispatch();
            }
            return;
        };
        let shown = view(
            &snapshot.daemon,
            snapshot.status.as_ref(),
            &snapshot.folders,
        );
        self.tray.set_tip(&shown.summary);
        if let Some(error) = snapshot.error {
            self.tray.show(
                &error,
                Some("Tidefell"),
                Some(nwg::TrayNotificationFlags::ERROR_ICON),
                None,
            );
        }
        *self.shown.borrow_mut() = shown;
    }

    /// Whether the supervisor still runs: it has not quit.
    fn supervising(&self) -> bool {
        !self.commands.is_closed()
    }

    fn show_menu(&self) {
        let shown = self.shown.borrow();
        let mut menu = nwg::Menu::default();
        if nwg::Menu::builder()
            .popup(true)
            .parent(&self.window)
            .build(&mut menu)
            .is_err()
        {
            return;
        }
        let mut items = Vec::new();
        let mut add = |text: &str, action: Option<Action>| {
            let mut item = nwg::MenuItem::default();
            if nwg::MenuItem::builder()
                .text(text)
                .disabled(action.is_none())
                .parent(&menu)
                .build(&mut item)
                .is_ok()
            {
                items.push((item, action));
            }
        };
        add(&shown.summary, None);
        for folder in &shown.folders {
            add(
                &format!("Open {}", folder.label),
                Some(Action::Open(folder.path.clone())),
            );
        }
        if shown.running {
            if shown.paused {
                add("Resume syncing", Some(Action::Resume));
            } else {
                add("Pause syncing", Some(Action::Pause));
            }
        } else if shown.failed {
            add("Start Tidefell again", Some(Action::Restart));
        }
        add("Quit Tidefell", Some(Action::Quit));
        let (x, y) = nwg::GlobalCursor::position();
        menu.popup(x, y);
        *self.menu.borrow_mut() = Some((menu, items));
    }

    fn chosen(&self, handle: nwg::ControlHandle) {
        let menu = self.menu.borrow();
        let Some((_, items)) = menu.as_ref() else {
            return;
        };
        let Some((_, Some(action))) = items.iter().find(|(item, _)| item.handle == handle) else {
            return;
        };
        let command = match action {
            Action::Open(path) => {
                let _ = std::process::Command::new("explorer").arg(path).spawn();
                return;
            }
            Action::Pause => Command::Pause,
            Action::Resume => Command::Resume,
            Action::Restart => Command::Restart,
            Action::Quit => {
                *self.quitting.borrow_mut() = true;
                self.tray.set_tip("Tidefell is stopping");
                Command::Quit
            }
        };
        let _ = self.commands.try_send(command);
    }
}
