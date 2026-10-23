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

use crate::approve::{Approval, Approving};
use crate::setup::{self, Joining};
use crate::storage::{self, Answer, Request, Row, Session};
use crate::supervisor::{self, Command, Paths, Snapshot};
use crate::view::{view, View};

/// What a menu item does.
enum Action {
    FirstDevice,
    Join,
    AddDevice,
    Open(PathBuf),
    /// An encrypted folder's Storage window: its ID and name.
    Storage(String, String),
    Pause,
    Resume,
    Restart,
    Quit,
}

/// A menu shown, its folders' submenus, and its items with what each does.
type Shown = (
    nwg::Menu,
    Vec<nwg::Menu>,
    Vec<(nwg::MenuItem, Option<Action>)>,
);

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
    paths: Paths,
    /// Wakes the UI with what pairing as a new device shows.
    join_notice: nwg::Notice,
    joining: Arc<Mutex<Vec<Joining>>>,
    join: RefCell<Option<JoinDialog>>,
    /// The dialog was closed, and goes on the next notice.
    join_closing: RefCell<bool>,
    /// Wakes the UI with what approving another device shows.
    approve_notice: nwg::Notice,
    approving: Arc<Mutex<Vec<Approving>>>,
    approve: RefCell<Option<ApproveDialog>>,
    /// What the approve dialog's buttons asked, done on the next notice.
    approve_asked: RefCell<Option<Asked>>,
    /// Wakes the UI with what the daemon answered the Storage window.
    storage_notice: nwg::Notice,
    storage_answers: Arc<Mutex<Vec<Answer>>>,
    storage: RefCell<Option<StorageWindow>>,
    /// What the Storage window asked, done on the next notice.
    storage_asked: RefCell<Vec<StorageAsked>>,
}

/// What the Storage window asked.
#[derive(Clone, Copy)]
enum StorageAsked {
    Selected,
    Set,
    Default,
    Drop,
    Close,
}

/// An encrypted folder's Storage window: its tree with each entry's priority, and what each
/// storage peer keeps.
struct StorageWindow {
    window: nwg::Window,
    tree: nwg::TreeView,
    /// The tree's items, in the order of `rows`.
    items: Vec<nwg::TreeItem>,
    rows: Vec<Row>,
    selected: nwg::Label,
    source: nwg::Label,
    priority: nwg::TextInput,
    set: nwg::Button,
    default: nwg::Button,
    _priority_hint: nwg::Label,
    _peers_label: nwg::Label,
    usage: nwg::ListBox<String>,
    peer: nwg::ComboBox<String>,
    _below_label: nwg::Label,
    below: nwg::TextInput,
    drop: nwg::Button,
    status: nwg::Label,
    handler: nwg::EventHandler,
    /// The storage peers' device IDs, in the order `peer` lists them.
    devices: Vec<String>,
    session: Session,
}

impl Drop for StorageWindow {
    fn drop(&mut self) {
        nwg::unbind_event_handler(&self.handler);
    }
}

/// What a button of the approve dialog asked.
#[derive(Clone, Copy)]
enum Asked {
    Next,
    Approve,
    Decline,
    Close,
}

/// The window for approving another device: its pairing code entered, then its confirmation
/// code checked.
struct ApproveDialog {
    _window: nwg::Window,
    text: nwg::Label,
    code: nwg::TextInput,
    next: nwg::Button,
    approve: nwg::Button,
    decline: nwg::Button,
    handler: nwg::EventHandler,
    approval: Option<Approval>,
}

impl Drop for ApproveDialog {
    fn drop(&mut self) {
        nwg::unbind_event_handler(&self.handler);
    }
}

/// The window shown while this device joins your others: the code to give one that holds
/// your user key, then the confirmation code to check.
struct JoinDialog {
    window: nwg::Window,
    _intro: nwg::Label,
    code: nwg::TextInput,
    confirm: nwg::Label,
    _cancel: nwg::Button,
    handler: nwg::EventHandler,
    /// `tidefelld pair`, stopped when the dialog goes.
    _join: setup::Join,
}

impl Drop for JoinDialog {
    fn drop(&mut self) {
        nwg::unbind_event_handler(&self.handler);
    }
}

/// Runs the tray until it is quit, supervising the daemon at `paths`.
pub fn run(paths: Paths) -> Result<(), String> {
    run_with(paths, |_| {})
}

/// [`run`], giving `started` a way to ask what the menu asks, once the icon shows.
pub fn run_with(paths: Paths, started: impl FnOnce(mpsc::Sender<Command>)) -> Result<(), String> {
    run_opening(paths, None, started)
}

/// [`run_with`], opening the Storage window of folder `storage`, its ID and name, at once.
pub fn run_opening(
    paths: Paths,
    storage: Option<(&str, &str)>,
    started: impl FnOnce(mpsc::Sender<Command>),
) -> Result<(), String> {
    nwg::init().map_err(|e| e.to_string())?;
    // Windows' own font for dialogs, not the old default nwg would otherwise give its controls.
    let mut font = nwg::Font::default();
    if nwg::Font::builder()
        .family("Segoe UI")
        .size(17)
        .build(&mut font)
        .is_ok()
    {
        nwg::Font::set_global_default(Some(font));
    }
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

    let mut join_notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&window)
        .build(&mut join_notice)
        .map_err(|e| e.to_string())?;

    let mut approve_notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&window)
        .build(&mut approve_notice)
        .map_err(|e| e.to_string())?;

    let mut storage_notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&window)
        .build(&mut storage_notice)
        .map_err(|e| e.to_string())?;

    let latest = Arc::new(Mutex::new(None));
    let (commands, receiver) = mpsc::channel(8);
    let supervisor = std::thread::spawn({
        let (paths, latest, sender) = (paths.clone(), latest.clone(), notice.sender());
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
        paths,
        join_notice,
        joining: Arc::new(Mutex::new(Vec::new())),
        join: RefCell::new(None),
        join_closing: RefCell::new(false),
        approve_notice,
        approving: Arc::new(Mutex::new(Vec::new())),
        approve: RefCell::new(None),
        approve_asked: RefCell::new(None),
        storage_notice,
        storage_answers: Arc::new(Mutex::new(Vec::new())),
        storage: RefCell::new(None),
        storage_asked: RefCell::new(Vec::new()),
    });
    let handler = nwg::full_bind_event_handler(&app.window.handle, {
        let app = Rc::downgrade(&app);
        move |event, _, handle| {
            let Some(app) = app.upgrade() else {
                return;
            };
            match event {
                nwg::Event::OnNotice if handle == app.notice.handle => app.refresh(),
                nwg::Event::OnNotice if handle == app.join_notice.handle => app.joined(),
                nwg::Event::OnNotice if handle == app.approve_notice.handle => app.approved(),
                nwg::Event::OnNotice if handle == app.storage_notice.handle => app.stored(),
                nwg::Event::OnContextMenu if handle == app.tray.handle => app.show_menu(),
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == app.tray.handle =>
                {
                    app.show_menu()
                }
                nwg::Event::OnMenuItemSelected => app.chosen(&app, handle),
                _ => {}
            }
        }
    });
    if let Some((id, name)) = storage {
        Tray::open_storage(&app, id, name);
    }
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
        // A copy: the menu runs events of its own while it shows, which may refresh the view.
        let shown = self.shown.borrow().clone();
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
        let mut add_to = |parent: &nwg::Menu, text: &str, action: Option<Action>| {
            let mut item = nwg::MenuItem::default();
            if nwg::MenuItem::builder()
                .text(text)
                .disabled(action.is_none())
                .parent(parent)
                .build(&mut item)
                .is_ok()
            {
                items.push((item, action));
            }
        };
        add_to(&menu, &shown.summary, None);
        if shown.setup {
            add_to(
                &menu,
                "Set up as my first device…",
                Some(Action::FirstDevice),
            );
            add_to(&menu, "Join my other devices…", Some(Action::Join));
        }
        let mut submenus = Vec::new();
        for folder in &shown.folders {
            let mut sub = nwg::Menu::default();
            if nwg::Menu::builder()
                .text(&folder.label)
                .parent(&menu)
                .build(&mut sub)
                .is_err()
            {
                continue;
            }
            add_to(
                &sub,
                "Open in Explorer",
                Some(Action::Open(folder.path.clone())),
            );
            if folder.storage {
                add_to(
                    &sub,
                    "Storage…",
                    Some(Action::Storage(folder.id.clone(), folder.name.clone())),
                );
            }
            submenus.push(sub);
        }
        let mut add = |text: &str, action: Option<Action>| add_to(&menu, text, action);
        if shown.running {
            add("Add a device…", Some(Action::AddDevice));
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
        *self.menu.borrow_mut() = Some((menu, submenus, items));
    }

    fn chosen(&self, app: &Rc<Tray>, handle: nwg::ControlHandle) {
        let menu = self.menu.borrow();
        let Some((_, _, items)) = menu.as_ref() else {
            return;
        };
        let Some((_, Some(action))) = items.iter().find(|(item, _)| item.handle == handle) else {
            return;
        };
        let command = match action {
            Action::FirstDevice => {
                drop(menu);
                self.first_device();
                return;
            }
            Action::Join => {
                drop(menu);
                Tray::join(app);
                return;
            }
            Action::AddDevice => {
                drop(menu);
                Tray::add_device(app);
                return;
            }
            Action::Open(path) => {
                let _ = std::process::Command::new("explorer").arg(path).spawn();
                return;
            }
            Action::Storage(id, name) => {
                let (id, name) = (id.clone(), name.clone());
                drop(menu);
                Tray::open_storage(app, &id, &name);
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

impl Tray {
    /// Sets this device up as the first of your devices, once the user agrees.
    fn first_device(&self) {
        let key = self.paths.data_dir.join("user.key");
        let agreed = nwg::message(&nwg::MessageParams {
            title: "Set up Tidefell",
            content: &format!(
                "This makes your user key, {}, which proves which devices are yours. Back it \
                 up somewhere safe: without it, no new device can join yours.\n\nSet up this \
                 device as your first?",
                key.display()
            ),
            buttons: nwg::MessageButtons::OkCancel,
            icons: nwg::MessageIcons::Info,
        });
        if agreed != nwg::MessageChoice::Ok {
            return;
        }
        match setup::first_device(&self.paths, &setup::device_name()) {
            // The supervisor starts the daemon once it sees the configuration.
            Ok(()) => self.tray.show(
                "This device is set up. Make a folder with `tidefell folder create <path> --partial <bytes>`.",
                Some("Tidefell"),
                Some(nwg::TrayNotificationFlags::INFO_ICON),
                None,
            ),
            Err(e) => {
                nwg::error_message("Tidefell", &format!("Cannot set up this device: {e}"));
            }
        }
    }

    /// Starts joining your other devices, showing the code to approve on one of them.
    fn join(app: &Rc<Tray>) {
        if app.join.borrow().is_some() {
            return;
        }
        match JoinDialog::open(app) {
            Ok(dialog) => *app.join.borrow_mut() = Some(dialog),
            Err(e) => {
                nwg::error_message("Tidefell", &format!("Cannot start pairing: {e}"));
            }
        }
    }

    /// Shows what pairing said since last time.
    fn joined(&self) {
        if std::mem::take(&mut *self.join_closing.borrow_mut()) {
            *self.join.borrow_mut() = None;
        }
        let events = std::mem::take(&mut *self.joining.lock().unwrap());
        for event in events {
            let open = self.join.borrow();
            let Some(dialog) = open.as_ref() else {
                return;
            };
            match event {
                Joining::Code(code) => dialog.code.set_text(&format!("tidefell pair {code}")),
                Joining::Confirm(code) => dialog.confirm.set_text(&format!(
                    "A device is approving this one. Check it shows confirmation code {} {}.",
                    &code[..code.len().min(3)],
                    &code[code.len().min(3)..]
                )),
                Joining::Joined => {
                    drop(open);
                    *self.join.borrow_mut() = None;
                    nwg::simple_message(
                        "Tidefell",
                        "This device has joined your devices, and Tidefell starts now. Join \
                         a folder with `tidefell folder join <id> <path> --partial <bytes>`.",
                    );
                    return;
                }
                Joining::Failed(reason) => {
                    drop(open);
                    *self.join.borrow_mut() = None;
                    nwg::error_message("Tidefell", &format!("Pairing stopped: {reason}"));
                    return;
                }
            }
        }
    }
}

impl JoinDialog {
    fn open(app: &Rc<Tray>) -> Result<Self, String> {
        let e = |e: nwg::NwgError| e.to_string();
        let mut window = nwg::Window::default();
        nwg::Window::builder()
            .title("Join your other devices")
            .size((520, 220))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .build(&mut window)
            .map_err(e)?;
        let mut intro = nwg::Label::default();
        nwg::Label::builder()
            .text(
                "On a device that holds your user key, choose \"Add a device\" in its tray and \
                 enter this code, or run:",
            )
            .position((16, 12))
            .size((488, 40))
            .parent(&window)
            .build(&mut intro)
            .map_err(e)?;
        let mut code = nwg::TextInput::default();
        nwg::TextInput::builder()
            .text("Making a pairing code…")
            .readonly(true)
            .position((16, 58))
            .size((488, 26))
            .parent(&window)
            .build(&mut code)
            .map_err(e)?;
        let mut confirm = nwg::Label::default();
        nwg::Label::builder()
            .text("Waiting for that device. The code is good for a few minutes.")
            .position((16, 96))
            .size((488, 50))
            .parent(&window)
            .build(&mut confirm)
            .map_err(e)?;
        let mut cancel = nwg::Button::default();
        nwg::Button::builder()
            .text("Cancel")
            .position((404, 160))
            .size((100, 30))
            .parent(&window)
            .build(&mut cancel)
            .map_err(e)?;

        let (joining, sender) = (app.joining.clone(), app.join_notice.sender());
        let join = setup::Join::start(&app.paths, &setup::device_name(), move |event| {
            joining.lock().unwrap().push(event);
            sender.notice();
        })?;
        let handler = nwg::full_bind_event_handler(&window.handle, {
            let app = Rc::downgrade(app);
            let (window_handle, cancel_handle) = (window.handle, cancel.handle);
            move |event, _, handle| {
                let close = match event {
                    nwg::Event::OnWindowClose => handle == window_handle,
                    nwg::Event::OnButtonClick => handle == cancel_handle,
                    _ => false,
                };
                if let (true, Some(app)) = (close, app.upgrade()) {
                    // Gone on the next notice: a handler cannot unbind itself.
                    if let Some(dialog) = app.join.borrow().as_ref() {
                        dialog.window.set_visible(false);
                    }
                    *app.join_closing.borrow_mut() = true;
                    app.join_notice.sender().notice();
                }
            }
        });
        Ok(Self {
            window,
            _intro: intro,
            code,
            confirm,
            _cancel: cancel,
            handler,
            _join: join,
        })
    }
}

impl Tray {
    /// Opens the window for approving another device.
    fn add_device(app: &Rc<Tray>) {
        if app.approve.borrow().is_some() {
            return;
        }
        match ApproveDialog::open(app) {
            Ok(dialog) => *app.approve.borrow_mut() = Some(dialog),
            Err(e) => {
                nwg::error_message("Tidefell", &format!("Cannot add a device: {e}"));
            }
        }
    }

    /// Does what the approve dialog asked, and shows what approving said.
    fn approved(&self) {
        let asked = self.approve_asked.borrow_mut().take();
        let mut open = self.approve.borrow_mut();
        let Some(dialog) = open.as_mut() else {
            return;
        };
        match asked {
            Some(Asked::Close) => {
                // Dropping the approval declines what was not decided.
                *open = None;
                return;
            }
            Some(Asked::Next) => {
                let code = dialog.code.text();
                if code.trim().is_empty() {
                    return;
                }
                let (approving, sender) = (self.approving.clone(), self.approve_notice.sender());
                dialog.approval = Some(Approval::start(&self.paths.data_dir, &code, move |e| {
                    approving.lock().unwrap().push(e);
                    sender.notice();
                }));
                dialog.next.set_enabled(false);
                dialog.code.set_readonly(true);
                dialog.text.set_text("Asking the new device…");
            }
            Some(Asked::Approve) => {
                if let Some(approval) = &dialog.approval {
                    approval.approve();
                }
                dialog.approve.set_enabled(false);
                dialog.decline.set_enabled(false);
            }
            Some(Asked::Decline) => {
                if let Some(approval) = &dialog.approval {
                    approval.decline();
                }
                dialog.approve.set_enabled(false);
                dialog.decline.set_enabled(false);
            }
            None => {}
        }
        let events = std::mem::take(&mut *self.approving.lock().unwrap());
        for event in events {
            match event {
                Approving::Confirm {
                    name, confirmation, ..
                } => {
                    dialog.text.set_text(&format!(
                        "\"{name}\" wants to join your devices. Approve it only if it shows \
                         confirmation code {} {}.",
                        &confirmation[..confirmation.len().min(3)],
                        &confirmation[confirmation.len().min(3)..]
                    ));
                    dialog.approve.set_visible(true);
                    dialog.decline.set_visible(true);
                    dialog.next.set_visible(false);
                }
                Approving::Approved => {
                    *open = None;
                    // Released first: a message box runs events of its own.
                    drop(open);
                    nwg::simple_message("Tidefell", "The device is one of yours now.");
                    return;
                }
                Approving::Declined => {
                    *open = None;
                    return;
                }
                Approving::Failed(reason) => {
                    *open = None;
                    drop(open);
                    nwg::error_message("Tidefell", &format!("Cannot add the device: {reason}"));
                    return;
                }
            }
        }
    }
}

impl ApproveDialog {
    fn open(app: &Rc<Tray>) -> Result<Self, String> {
        let e = |e: nwg::NwgError| e.to_string();
        let mut window = nwg::Window::default();
        nwg::Window::builder()
            .title("Add a device")
            .size((520, 200))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .build(&mut window)
            .map_err(e)?;
        let mut text = nwg::Label::default();
        nwg::Label::builder()
            .text(
                "On the new device, choose \"Join my other devices\" in its tray, or run \
                 `tidefelld pair`, and enter the pairing code it shows:",
            )
            .position((16, 12))
            .size((488, 54))
            .parent(&window)
            .build(&mut text)
            .map_err(e)?;
        let mut code = nwg::TextInput::default();
        nwg::TextInput::builder()
            .position((16, 72))
            .size((488, 26))
            .focus(true)
            .parent(&window)
            .build(&mut code)
            .map_err(e)?;
        let button = |label: &str, x: i32, visible: bool, out: &mut nwg::Button| {
            let mut flags = nwg::ButtonFlags::TAB_STOP;
            if visible {
                flags |= nwg::ButtonFlags::VISIBLE;
            }
            nwg::Button::builder()
                .text(label)
                .position((x, 140))
                .size((100, 30))
                .flags(flags)
                .parent(&window)
                .build(out)
        };
        let (mut next, mut approve, mut decline) = Default::default();
        button("Next", 404, true, &mut next).map_err(e)?;
        button("Approve", 296, false, &mut approve).map_err(e)?;
        button("Decline", 404, false, &mut decline).map_err(e)?;
        let handler = nwg::full_bind_event_handler(&window.handle, {
            let app = Rc::downgrade(app);
            let handles = (window.handle, next.handle, approve.handle, decline.handle);
            move |event, _, handle| {
                let asked = match event {
                    nwg::Event::OnWindowClose if handle == handles.0 => Asked::Close,
                    nwg::Event::OnButtonClick if handle == handles.1 => Asked::Next,
                    nwg::Event::OnButtonClick if handle == handles.2 => Asked::Approve,
                    nwg::Event::OnButtonClick if handle == handles.3 => Asked::Decline,
                    _ => return,
                };
                if let Some(app) = app.upgrade() {
                    // Done on the next notice: a handler cannot unbind itself.
                    *app.approve_asked.borrow_mut() = Some(asked);
                    app.approve_notice.sender().notice();
                }
            }
        });
        Ok(Self {
            _window: window,
            text,
            code,
            next,
            approve,
            decline,
            handler,
            approval: None,
        })
    }
}

impl Tray {
    /// Opens an encrypted folder's Storage window, in place of one already open.
    fn open_storage(app: &Rc<Tray>, id: &str, name: &str) {
        *app.storage.borrow_mut() = None;
        app.storage_answers.lock().unwrap().clear();
        match StorageWindow::open(app, id, name) {
            Ok(window) => *app.storage.borrow_mut() = Some(window),
            Err(e) => {
                nwg::error_message("Tidefell", &format!("Cannot open the Storage window: {e}"));
            }
        }
    }

    /// Does what the Storage window asked, and shows what the daemon answered.
    fn stored(&self) {
        let asked = std::mem::take(&mut *self.storage_asked.borrow_mut());
        for asked in asked {
            match asked {
                StorageAsked::Close => {
                    *self.storage.borrow_mut() = None;
                    return;
                }
                StorageAsked::Selected => {
                    if let Some(window) = self.storage.borrow().as_ref() {
                        window.show_selected();
                    }
                }
                StorageAsked::Set | StorageAsked::Default => {
                    let open = self.storage.borrow();
                    let Some(window) = open.as_ref() else {
                        return;
                    };
                    let Some(row) = window.selected_row() else {
                        window.status.set_text("Choose a file or folder first.");
                        continue;
                    };
                    let priority = match asked {
                        StorageAsked::Set => match storage::parse_priority(&window.priority.text())
                        {
                            Ok(p) => Some(p),
                            Err(e) => {
                                window.status.set_text(&e);
                                continue;
                            }
                        },
                        _ => None,
                    };
                    window.status.set_text("Saving…");
                    window.session.ask(Request::Set {
                        pattern: storage::pattern_for(&row.path, row.directory),
                        priority,
                    });
                }
                StorageAsked::Drop => {
                    let (device, below) = {
                        let open = self.storage.borrow();
                        let Some(window) = open.as_ref() else {
                            return;
                        };
                        let Some(device) = window
                            .peer
                            .selection()
                            .and_then(|i| window.devices.get(i).cloned())
                        else {
                            window.status.set_text("Choose a storage peer first.");
                            continue;
                        };
                        match storage::parse_priority(&window.below.text()) {
                            Ok(below) => (device, below),
                            Err(e) => {
                                window.status.set_text(&e);
                                continue;
                            }
                        }
                    };
                    // Asked with nothing borrowed: a message box runs events of its own.
                    if ask(&storage::drop_question(&device, below)) {
                        if let Some(window) = self.storage.borrow().as_ref() {
                            window.status.set_text("Asking the storage peer…");
                            window.session.ask(Request::Drop {
                                device,
                                below,
                                force: false,
                            });
                        }
                    }
                }
            }
        }
        let answers = std::mem::take(&mut *self.storage_answers.lock().unwrap());
        for answer in answers {
            match answer {
                Answer::Loaded(priorities, reports) => {
                    let mut open = self.storage.borrow_mut();
                    let Some(window) = open.as_mut() else {
                        return;
                    };
                    window.show(&priorities, &reports);
                    window.status.set_text("");
                }
                Answer::Dropped {
                    device,
                    below,
                    result,
                } => {
                    let force = if result.done {
                        nwg::simple_message("Tidefell", &storage::dropped(&result));
                        false
                    } else {
                        ask(&storage::at_risk_question(&result))
                    };
                    if let Some(window) = self.storage.borrow().as_ref() {
                        window.session.ask(if force {
                            Request::Drop {
                                device,
                                below,
                                force: true,
                            }
                        } else {
                            Request::Load
                        });
                    }
                }
                Answer::Failed(reason) => {
                    if let Some(window) = self.storage.borrow().as_ref() {
                        window.status.set_text(&reason);
                    }
                }
            }
        }
    }
}

/// Asks `question`, Yes or No.
fn ask(question: &str) -> bool {
    nwg::message(&nwg::MessageParams {
        title: "Tidefell",
        content: question,
        buttons: nwg::MessageButtons::YesNo,
        icons: nwg::MessageIcons::Question,
    }) == nwg::MessageChoice::Yes
}

impl StorageWindow {
    fn open(app: &Rc<Tray>, id: &str, name: &str) -> Result<Self, String> {
        let e = |e: nwg::NwgError| e.to_string();
        let mut window = nwg::Window::default();
        nwg::Window::builder()
            .title(&format!("Storage: {name}"))
            .size((860, 580))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .build(&mut window)
            .map_err(e)?;
        let mut tree = nwg::TreeView::default();
        nwg::TreeView::builder()
            .position((16, 16))
            .size((440, 548))
            .parent(&window)
            .build(&mut tree)
            .map_err(e)?;
        let label = |text: &str, (x, y): (i32, i32), (w, h): (i32, i32), out: &mut nwg::Label| {
            nwg::Label::builder()
                .text(text)
                .position((x, y))
                .size((w, h))
                .parent(&window)
                .build(out)
        };
        let button = |text: &str, (x, y): (i32, i32), w: i32, out: &mut nwg::Button| {
            nwg::Button::builder()
                .text(text)
                .position((x, y))
                .size((w, 28))
                .parent(&window)
                .build(out)
        };
        let input = |(x, y): (i32, i32), w: i32, out: &mut nwg::TextInput| {
            nwg::TextInput::builder()
                .position((x, y))
                .size((w, 26))
                .parent(&window)
                .build(out)
        };
        let (mut selected, mut source, mut priority_hint) = Default::default();
        label(
            "Choose a file or folder.",
            (472, 16),
            (372, 40),
            &mut selected,
        )
        .map_err(e)?;
        label("", (472, 58), (372, 22), &mut source).map_err(e)?;
        label(
            "Priority, 0 to 255: higher is kept first when a storage peer runs short of room; \
             0 is never stored there.",
            (472, 84),
            (372, 40),
            &mut priority_hint,
        )
        .map_err(e)?;
        let mut priority = nwg::TextInput::default();
        input((472, 128), 60, &mut priority).map_err(e)?;
        let (mut set, mut default) = Default::default();
        button("Set", (540, 127), 80, &mut set).map_err(e)?;
        button("Use the default", (628, 127), 130, &mut default).map_err(e)?;
        let mut peers_label = nwg::Label::default();
        label(
            "What each storage peer keeps, by priority:",
            (472, 176),
            (372, 22),
            &mut peers_label,
        )
        .map_err(e)?;
        let mut usage = nwg::ListBox::default();
        nwg::ListBox::builder()
            .collection(vec!["Asking the storage peers…".to_owned()])
            .position((472, 200))
            .size((372, 230))
            .parent(&window)
            .build(&mut usage)
            .map_err(e)?;
        let mut peer = nwg::ComboBox::default();
        nwg::ComboBox::builder()
            .position((472, 442))
            .size((150, 26))
            .parent(&window)
            .build(&mut peer)
            .map_err(e)?;
        let mut below_label = nwg::Label::default();
        label("below", (630, 446), (44, 22), &mut below_label).map_err(e)?;
        let mut below = nwg::TextInput::default();
        input((676, 442), 50, &mut below).map_err(e)?;
        let mut drop = nwg::Button::default();
        button("Drop…", (734, 441), 110, &mut drop).map_err(e)?;
        let mut status = nwg::Label::default();
        label("Loading…", (472, 484), (372, 80), &mut status).map_err(e)?;

        let (answers, sender) = (app.storage_answers.clone(), app.storage_notice.sender());
        let session = Session::start(&app.paths.data_dir, id, move |answer| {
            answers.lock().unwrap().push(answer);
            sender.notice();
        });
        session.ask(Request::Load);
        let handler = nwg::full_bind_event_handler(&window.handle, {
            let app = Rc::downgrade(app);
            let handles = (
                window.handle,
                tree.handle,
                set.handle,
                default.handle,
                drop.handle,
            );
            move |event, _, handle| {
                let asked = match event {
                    nwg::Event::OnWindowClose if handle == handles.0 => StorageAsked::Close,
                    nwg::Event::OnTreeItemSelectionChanged if handle == handles.1 => {
                        StorageAsked::Selected
                    }
                    nwg::Event::OnButtonClick if handle == handles.2 => StorageAsked::Set,
                    nwg::Event::OnButtonClick if handle == handles.3 => StorageAsked::Default,
                    nwg::Event::OnButtonClick if handle == handles.4 => StorageAsked::Drop,
                    _ => return,
                };
                if let Some(app) = app.upgrade() {
                    // Done on the next notice: a handler cannot unbind itself.
                    app.storage_asked.borrow_mut().push(asked);
                    app.storage_notice.sender().notice();
                }
            }
        });
        Ok(Self {
            window,
            tree,
            items: Vec::new(),
            rows: Vec::new(),
            selected,
            source,
            priority,
            set,
            default,
            _priority_hint: priority_hint,
            _peers_label: peers_label,
            usage,
            peer,
            _below_label: below_label,
            below,
            drop,
            status,
            handler,
            devices: Vec::new(),
            session,
        })
    }

    /// The row of the item chosen in the tree, if one is.
    fn selected_row(&self) -> Option<&Row> {
        let item = self.tree.selected_item()?;
        let index = usize::try_from(self.tree.item_param(&item)?).ok()?;
        self.rows.get(index)
    }

    /// Shows the chosen item's priority, and where it comes from.
    fn show_selected(&self) {
        match self.selected_row() {
            Some(row) => {
                self.selected.set_text(&row.path);
                self.source.set_text(&format!(
                    "{} ({}), {}",
                    row.priority,
                    storage::word(row.priority),
                    row.source
                ));
                self.priority.set_text(&row.priority.to_string());
                self.set.set_enabled(true);
                self.default.set_enabled(row.set_here);
            }
            None => {
                self.selected.set_text("Choose a file or folder.");
                self.source.set_text("");
                self.set.set_enabled(false);
                self.default.set_enabled(false);
            }
        }
    }

    /// Shows the folder's tree and what its storage peers keep, keeping the item chosen.
    fn show(
        &mut self,
        priorities: &tidefell_sdk::types::Priorities,
        reports: &[tidefell_sdk::types::StoragePeerReport],
    ) {
        let chosen = self.selected_row().map(|r| r.path.clone());
        self.tree.clear();
        self.rows = storage::rows(priorities);
        self.items = Vec::with_capacity(self.rows.len());
        for (index, row) in self.rows.iter().enumerate() {
            let parent = row.parent.and_then(|p| self.items.get(p));
            let item = self.tree.insert_item_with_param(
                &row.label,
                parent,
                nwg::TreeInsert::Last,
                index as isize,
            );
            self.items.push(item);
        }
        if let Some(index) = chosen.and_then(|c| self.rows.iter().position(|r| r.path == c)) {
            self.tree.select_item(&self.items[index]);
            self.tree.ensure_visible(&self.items[index]);
        }
        self.show_selected();
        self.usage.set_collection(storage::usage(reports));
        let chosen_peer = self
            .peer
            .selection()
            .and_then(|i| self.devices.get(i).cloned());
        self.devices = reports.iter().map(|r| r.device.clone()).collect();
        self.peer
            .set_collection(self.devices.iter().map(|d| storage::short(d)).collect());
        let index = chosen_peer
            .and_then(|c| self.devices.iter().position(|d| *d == c))
            .or((!self.devices.is_empty()).then_some(0));
        self.peer.set_selection(index);
        self.drop.set_enabled(!self.devices.is_empty());
        self.window.set_visible(true);
    }
}
