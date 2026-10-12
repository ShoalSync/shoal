//! Logging shared by `tidefelld` and the bindings (ADR 0030): readable standard error, a
//! rotating file of JSON lines, logcat on Android, a level that can change at runtime, and the
//! most recent warnings and errors in memory.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter, Layer, Registry};

/// Size at which the log file is rotated.
const FILE_BYTES: u64 = 5 * 1024 * 1024;
/// Log files kept, the current one included.
const FILES: usize = 5;
/// Warnings and errors kept in memory.
const PROBLEMS: usize = 500;

/// How to log.
#[derive(Debug, Clone)]
pub struct LogOptions {
    /// The directory whose `logs/` subdirectory holds the log files.
    pub data_dir: PathBuf,
    /// Filter, such as `info` or `info,tidefell_net=debug`.
    pub level: String,
    /// Write the rotating file. `None` writes it unless the journal already keeps the logs.
    pub file: Option<bool>,
    /// Also log to standard error, readable.
    pub stderr: bool,
}

/// A warning or error kept in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// `WARN` or `ERROR`.
    pub level: String,
    /// Where it was logged, such as `tidefell_engine::scan`.
    pub target: String,
    /// The message.
    pub message: String,
    /// The structured fields, such as the folder and path.
    pub fields: BTreeMap<String, String>,
}

static LEVEL: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();
/// This process's crash file: later panics are added to it, so a cascade of panics on poisoned
/// locks cannot push out the file with the cause.
static CRASH_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
static CURRENT: Mutex<String> = Mutex::new(String::new());
static FILE: OnceLock<PathBuf> = OnceLock::new();
static RECENT: Mutex<VecDeque<Problem>> = Mutex::new(VecDeque::new());

/// Starts logging for this process. Only the first call has an effect; later calls return
/// false.
pub fn init_logging(options: &LogOptions) -> io::Result<bool> {
    if LEVEL.get().is_some() {
        return Ok(false);
    }
    let filter = parse_filter(&options.level)?;
    let (filter, handle) = reload::Layer::new(filter);
    let journal = std::env::var_os("JOURNAL_STREAM").is_some();
    let logs = options.data_dir.join("logs");
    let writes_file = options.file.unwrap_or(!journal);
    let file = if writes_file {
        let writer = RotatingFile::open(&logs)?;
        Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(false)
                .with_span_list(false)
                .with_writer(Mutex::new(writer)),
        )
    } else {
        None
    };
    let stderr = options.stderr.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            // The journal adds its own timestamps.
            .with_ansi(!journal)
    });
    #[cfg(target_os = "android")]
    let outputs = Layer::and_then(
        Layer::and_then(file, stderr),
        paranoid_android::layer("tidefell"),
    );
    #[cfg(not(target_os = "android"))]
    let outputs = Layer::and_then(file, stderr);
    // The level applies to the outputs only, so warnings and errors are always kept in memory.
    let registry = tracing_subscriber::registry()
        .with(outputs.with_filter(filter))
        .with(Recent.with_filter(LevelFilter::WARN));
    if registry.try_init().is_err() {
        return Ok(false);
    }
    let _ = LEVEL.set(handle);
    if writes_file {
        let _ = FILE.set(logs.join("tidefell.log"));
    }
    *CURRENT.lock().unwrap() = options.level.clone();
    Ok(true)
}

/// Changes the level while running.
pub fn set_log_level(level: &str) -> io::Result<()> {
    let filter = parse_filter(level)?;
    let handle = LEVEL
        .get()
        .ok_or_else(|| io::Error::other("logging is not started"))?;
    handle.reload(filter).map_err(io::Error::other)?;
    *CURRENT.lock().unwrap() = level.to_owned();
    Ok(())
}

/// The current level, or `None` before logging starts.
pub fn log_level() -> Option<String> {
    LEVEL.get().map(|_| CURRENT.lock().unwrap().clone())
}

/// The file logs are written to, if any.
pub fn log_file() -> Option<PathBuf> {
    FILE.get().cloned()
}

/// The most recent warnings and errors, oldest first.
pub fn recent_problems() -> Vec<Problem> {
    RECENT.lock().unwrap().iter().cloned().collect()
}

fn parse_filter(level: &str) -> io::Result<EnvFilter> {
    EnvFilter::try_new(level)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{level:?}: {e}")))
}

/// Keeps the most recent warnings and errors.
struct Recent;

impl<S: Subscriber> Layer<S> for Recent {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::WARN {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let problem = Problem {
            time_ms: crate::now_ms(),
            level: level.to_string(),
            target: event.metadata().target().to_owned(),
            message: fields.message,
            fields: fields.fields,
        };
        let mut recent = RECENT.lock().unwrap();
        if recent.len() == PROBLEMS {
            recent.pop_front();
        }
        recent.push_back(problem);
    }
}

#[derive(Default)]
struct Fields {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }
}

/// `tidefell.log`, moved to `tidefell.1.log` and so on when it reaches [`FILE_BYTES`].
struct RotatingFile {
    dir: PathBuf,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join("tidefell.log");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        for n in (1..FILES).rev() {
            let from = if n == 1 {
                self.dir.join("tidefell.log")
            } else {
                self.dir.join(format!("tidefell.{}.log", n - 1))
            };
            let _ = fs::rename(from, self.dir.join(format!("tidefell.{n}.log")));
        }
        self.file = File::create(self.dir.join("tidefell.log"))?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written > 0 && self.written + buf.len() as u64 > FILE_BYTES {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Crash files kept.
const CRASHES: usize = 10;

/// A crash recorded in `<data_dir>/crashes/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    /// Its file's name, which names it in [`crash_report`].
    pub id: String,
    /// The crash file.
    pub file: PathBuf,
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// What went wrong.
    pub message: String,
    /// Whether the user has already been told.
    pub seen: bool,
}

static PANIC_HOOK: OnceLock<()> = OnceLock::new();

thread_local! {
    /// Set while running code whose panics are caught and handled, which are not crashes.
    static EXPECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f`, catching a panic as an error rather than reporting it as a crash: for libraries
/// that panic on bad input, such as redb on a damaged file (ADR 0034).
pub(crate) fn catching<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    let before = EXPECTED.with(|e| e.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    EXPECTED.with(|e| e.set(before));
    result
}

/// Writes a crash file to `<data_dir>/crashes/` for every panic, then runs the previous hook.
/// Only the first call has an effect.
pub fn install_panic_hook(data_dir: &Path) {
    let dir = data_dir.join("crashes");
    PANIC_HOOK.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if EXPECTED.with(std::cell::Cell::get) {
                return;
            }
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic without a message".into());
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let thread = std::thread::current();
            let backtrace = std::backtrace::Backtrace::force_capture();
            let details = format!(
                "thread: {}\nlocation: {location}\n\nbacktrace:\n{backtrace}",
                thread.name().unwrap_or("unnamed")
            );
            let mut first = CRASH_FILE.lock().unwrap_or_else(|e| e.into_inner());
            match first.as_ref() {
                Some(file) => {
                    let later = format!("\n--- a later panic\nmessage: {message}\n{details}\n");
                    let _ = OpenOptions::new()
                        .append(true)
                        .open(file)
                        .and_then(|mut f| f.write_all(later.as_bytes()));
                }
                None => *first = write_crash(&dir, "rust", &message, &details).ok(),
            }
            drop(first);
            previous(info);
        }));
    });
}

/// Records a crash: the panic hook's, or the app's for an uncaught exception.
pub fn write_crash(dir: &Path, source: &str, message: &str, details: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let time_ms = crate::now_ms();
    let path = dir.join(format!("crash-{time_ms:013}-{source}.txt"));
    let text = format!(
        "Tidefell stopped unexpectedly\nversion: {}\nplatform: {}-{}\ntime_ms: {time_ms}\nmessage: {}\n{details}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        message.replace('\n', " "),
    );
    fs::write(&path, text)?;
    let mut names = crash_files(dir);
    while names.len() > CRASHES {
        let _ = fs::remove_file(dir.join(names.remove(0)));
    }
    Ok(path)
}

/// Crashes recorded in `<data_dir>/crashes/`, oldest first.
pub fn crashes(data_dir: &Path) -> Vec<Crash> {
    let dir = data_dir.join("crashes");
    let seen_until = fs::read_to_string(dir.join("seen"))
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(i64::MIN);
    crash_files(&dir)
        .into_iter()
        .filter_map(|name| {
            let file = dir.join(&name);
            let text = fs::read_to_string(&file).ok()?;
            let field = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(str::to_owned)
            };
            let time_ms = field("time_ms: ")?.parse().ok()?;
            Some(Crash {
                id: name,
                file,
                time_ms,
                message: field("message: ").unwrap_or_default(),
                seen: time_ms <= seen_until,
            })
        })
        .collect()
}

/// Where crash reports go unless the configuration says otherwise: the project's issue tracker,
/// once the repositories are public (ADR 0030). Until then the report is shown to copy.
pub const DEFAULT_REPORT_URL: Option<&str> = None;

/// The longest pre-filled issue link made: longer ones are turned away by browsers and trackers.
const REPORT_LINK_MAX: usize = 8000;

/// A crash written up as a new issue for the project's tracker (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    /// The issue's title.
    pub title: String,
    /// Its body, in Markdown, with home directories replaced by `~`.
    pub body: String,
    /// A link that opens a new issue pre-filled with them, if a tracker is configured. Its
    /// body is cut to fit a link when it has to be; `body` is always whole.
    pub url: Option<String>,
}

/// Checks a tracker's URL: its issues page over HTTP or HTTPS, without a query, such as
/// `https://github.com/Tidefell/tidefell/issues`.
pub fn check_report_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("{url:?} is not an http or https URL"))?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err(format!("{url:?} has no host"));
    }
    if url.contains(['?', '#', ' ']) {
        return Err(format!("{url:?} must be the issues page, without a query"));
    }
    Ok(())
}

/// The crash `id` in `<data_dir>/crashes/`, written up for `report_url`, the tracker's issues
/// page: GitHub and Forgejo open `<report_url>/new?title=...&body=...` as a new issue. `None`
/// if there is no such crash.
pub fn crash_report(data_dir: &Path, id: &str, report_url: Option<&str>) -> Option<CrashReport> {
    let well_formed = id.starts_with("crash-")
        && id.ends_with(".txt")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !id.contains("..");
    if !well_formed {
        return None;
    }
    let text = fs::read_to_string(data_dir.join("crashes").join(id)).ok()?;
    let text = strip_home(&text, std::env::var_os("HOME").as_deref().map(Path::new));
    let message = text
        .lines()
        .find_map(|l| l.strip_prefix("message: "))
        .unwrap_or("Tidefell stopped unexpectedly")
        .trim();
    let mut title = format!("Crash: {message}");
    if title.chars().count() > 100 {
        title = title.chars().take(99).collect::<String>() + "…";
    }
    let lines: Vec<&str> = text.trim_end().lines().collect();
    // A fence longer than any run of backticks in the text.
    let longest = lines
        .iter()
        .flat_map(|l| l.split(|c| c != '`'))
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let head = format!("Tidefell stopped unexpectedly: {message}\n\n{fence}\n");
    let tail = |cut: bool| {
        let note = if cut {
            "(cut to fit a link: paste the rest of the crash file here)\n"
        } else {
            ""
        };
        format!(
            "{note}{fence}\n\nA diagnostics bundle helps: make one with `tidefell diagnostics`, \
             the web peer's status page or the app, look inside, and attach it here.\n"
        )
    };
    let body = format!("{head}{}\n{}", lines.join("\n"), tail(false));
    let url = report_url.map(|base| {
        let start = format!(
            "{}/new?title={}&body=",
            base.trim_end_matches('/'),
            percent_encode(&title)
        );
        if start.len() + encoded_len(&body) <= REPORT_LINK_MAX {
            return start + &percent_encode(&body);
        }
        // The most lines that fit, each followed by an encoded newline, in one pass.
        let fixed = start.len() + encoded_len(&head) + encoded_len(&tail(true));
        let mut total = fixed + 3;
        let mut shown = 0;
        for line in &lines {
            let with = total + encoded_len(line) + if shown == 0 { 0 } else { 3 };
            if with > REPORT_LINK_MAX {
                break;
            }
            total = with;
            shown += 1;
        }
        if total > REPORT_LINK_MAX {
            // Even the message is too long for a link.
            return start
                + &percent_encode("The crash report is too long for a link: paste it here.\n");
        }
        let cut = format!("{head}{}\n{}", lines[..shown].join("\n"), tail(true));
        start + &percent_encode(&cut)
    });
    Some(CrashReport { title, body, url })
}

/// `text` with home directories replaced by `~`: `home`, and any `/home/<name>`,
/// `/Users/<name>`, `/root` or `C:\Users\<name>`, which backtraces can name from the machine
/// that built Tidefell.
fn strip_home(text: &str, home: Option<&Path>) -> String {
    let mut text = text.to_owned();
    if let Some(home) = home.and_then(Path::to_str).filter(|h| h.len() > 1) {
        text = replace_at_boundary(&text, home.trim_end_matches('/'), None);
    }
    for (prefix, separator) in [("/home/", '/'), ("/Users/", '/'), ("C:\\Users\\", '\\')] {
        text = replace_at_boundary(&text, prefix, Some(separator));
    }
    replace_at_boundary(&text, "/root", None)
}

/// Replaces `prefix` with `~` where it starts a path and is followed by a separator or the end
/// of the path. With `name`, a name up to that separator follows the prefix and goes too.
fn replace_at_boundary(text: &str, prefix: &str, name: Option<char>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        let starts_path = rest[..at]
            .chars()
            .last()
            .or_else(|| out.chars().last())
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.' && c != '-');
        let after = &rest[at + prefix.len()..];
        let (skip, ok) = match name {
            Some(separator) => match after.find(separator) {
                Some(end)
                    if end > 0
                        && !after[..end].contains(|c: char| c.is_whitespace() || c == '/') =>
                {
                    (end, true)
                }
                _ => (0, false),
            },
            None => (
                0,
                after
                    .chars()
                    .next()
                    .is_none_or(|c| c == '/' || c == '\\' || c.is_whitespace() || c == ':'),
            ),
        };
        if starts_path && ok {
            out.push_str(&rest[..at]);
            out.push('~');
            rest = &after[skip..];
        } else {
            out.push_str(&rest[..at + prefix.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~".contains(&b)
}

/// Percent-encodes everything but unreserved characters, for a URL's query.
fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(encoded_len(s));
    for b in s.bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 15)] as char);
        }
    }
    out
}

/// How long `s` is once percent-encoded.
fn encoded_len(s: &str) -> usize {
    s.bytes().map(|b| if unreserved(b) { 1 } else { 3 }).sum()
}

/// Marks every crash recorded so far as seen.
pub fn dismiss_crashes(data_dir: &Path) -> io::Result<()> {
    let dir = data_dir.join("crashes");
    if let Some(last) = crashes(data_dir).last() {
        fs::write(dir.join("seen"), last.time_ms.to_string())?;
    }
    Ok(())
}

fn crash_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("crash-") && n.ends_with(".txt"))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_rotates_and_keeps_five() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RotatingFile::open(dir.path()).unwrap();
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..(FILES * 5 + 3) {
            file.write_all(&line).unwrap();
        }
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "tidefell.1.log",
                "tidefell.2.log",
                "tidefell.3.log",
                "tidefell.4.log",
                "tidefell.log"
            ]
        );
        for name in &names {
            let len = fs::metadata(dir.path().join(name)).unwrap().len();
            assert!(len <= FILE_BYTES, "{name} is {len} bytes");
        }
    }

    #[test]
    fn logs_problems_and_level() {
        let dir = tempfile::tempdir().unwrap();
        let options = LogOptions {
            data_dir: dir.path().to_owned(),
            level: "info".into(),
            file: Some(true),
            stderr: false,
        };
        assert!(init_logging(&options).unwrap());
        assert!(
            !init_logging(&options).unwrap(),
            "only the first call starts logging"
        );
        assert_eq!(log_level().as_deref(), Some("info"));

        tracing::info!(folder = "f1", "an ordinary message");
        tracing::warn!(folder = "f1", path = "a/b.txt", "cannot write");
        tracing::debug!("not at this level");
        set_log_level("debug").unwrap();
        tracing::debug!("now at this level");
        assert!(set_log_level("not a [filter").is_err());
        assert_eq!(log_level().as_deref(), Some("debug"));

        // At a level above warnings, warnings still reach the problems list, not the file.
        set_log_level("error").unwrap();
        tracing::warn!("quiet in the file");
        // Other tests in this process log too, so problems are found by message.
        let found = |message: &str| recent_problems().into_iter().find(|p| p.message == message);
        assert!(found("quiet in the file").is_some());
        set_log_level("debug").unwrap();

        let problem = found("cannot write").unwrap();
        assert_eq!(problem.level, "WARN");
        assert_eq!(
            problem.fields.get("path").map(String::as_str),
            Some("a/b.txt")
        );

        let text = fs::read_to_string(dir.path().join("logs/tidefell.log")).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let messages: Vec<&str> = lines
            .iter()
            .filter_map(|l| l["fields"]["message"].as_str())
            .collect();
        assert!(messages.contains(&"an ordinary message"));
        assert!(messages.contains(&"now at this level"));
        assert!(!messages.contains(&"not at this level"));
        assert!(!messages.contains(&"quiet in the file"));
        let warn = lines
            .iter()
            .find(|l| l["fields"]["message"] == "cannot write")
            .unwrap();
        assert_eq!(warn["fields"]["path"], "a/b.txt");
        assert_eq!(warn["level"], "WARN");
    }

    #[test]
    fn a_panic_leaves_a_crash_file() {
        let dir = tempfile::tempdir().unwrap();
        install_panic_hook(dir.path());
        let result = std::panic::catch_unwind(|| panic!("boom in a test"));
        assert!(result.is_err());
        // A panic that follows, as on a poisoned lock, is added to the same file.
        let result = std::panic::catch_unwind(|| panic!("a second boom"));
        assert!(result.is_err());
        let crashes = crashes(dir.path());
        assert_eq!(crashes.len(), 1);
        let crash = &crashes[0];
        assert!(!crash.seen);
        // Other tests in this process may panic on purpose first, in which case theirs is the
        // file's headline and both of these follow it.
        let text = fs::read_to_string(&crash.file).unwrap();
        assert!(text.contains("message: boom in a test"), "{text}");
        assert!(text.contains("location: "), "{text}");
        assert!(text.contains("backtrace:"), "{text}");
        assert!(
            text.contains("a later panic\nmessage: a second boom"),
            "{text}"
        );
        dismiss_crashes(dir.path()).unwrap();
        assert!(super::crashes(dir.path()).iter().all(|c| c.seen));
    }

    #[test]
    fn the_ten_newest_crashes_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            write_crash(dir.path(), "test", &format!("crash {i}"), "").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let names = crash_files(dir.path());
        assert_eq!(names.len(), CRASHES);
        let messages: Vec<String> = names
            .iter()
            .map(|n| fs::read_to_string(dir.path().join(n)).unwrap())
            .collect();
        assert!(messages[0].contains("message: crash 2"));
        assert!(messages[9].contains("message: crash 11"));
    }

    #[test]
    fn home_directories_are_stripped() {
        let home = Some(Path::new("/srv/me"));
        assert_eq!(
            strip_home(
                "at /srv/me/x.rs and /home/ci/.cargo/r.rs, /Users/ann/a, /root/b, \
                 C:\\Users\\bo\\c.rs",
                home
            ),
            "at ~/x.rs and ~/.cargo/r.rs, ~/a, ~/b, ~\\c.rs"
        );
        // Only whole path components.
        let kept = "/homework/a /srv/meadow x/home/y/z /rooted";
        assert_eq!(strip_home(kept, home), kept);
    }

    #[test]
    fn crashes_are_written_up() {
        let dir = tempfile::tempdir().unwrap();
        let crashes = dir.path().join("crashes");
        write_crash(
            &crashes,
            "rust",
            "index out of bounds: `x` ``` + ü",
            "thread: main\nlocation: /home/ci/src/lib.rs:3\n\nbacktrace:\n  0: f\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        write_crash(
            &crashes,
            "app",
            "NullPointerException",
            "java.lang.Npe\n\tat A",
        )
        .unwrap();
        let found = super::crashes(dir.path());
        let report = crash_report(dir.path(), &found[0].id, None).unwrap();
        assert!(report.title.starts_with("Crash: index out of bounds"));
        assert!(
            report.body.contains("location: ~/src/lib.rs:3"),
            "{}",
            report.body
        );
        assert!(
            report.body.contains("\n````\n"),
            "a fence longer than the text's"
        );
        assert!(report.body.contains("tidefell diagnostics"));
        assert_eq!(report.url, None);
        // The app's own crashes, with free-form details.
        let app = crash_report(dir.path(), &found[1].id, None).unwrap();
        assert!(app.body.contains("\tat A"));

        // A link that opens the issue, with every reserved character escaped.
        let tracker = "https://example.org/t/issues/";
        let report = crash_report(dir.path(), &found[0].id, Some(tracker)).unwrap();
        let url = report.url.unwrap();
        assert!(url.starts_with("https://example.org/t/issues/new?title=Crash%3A%20index"));
        assert!(url.contains("%C3%BC") && !url.contains(' ') && !url.contains('+'));

        // Anything but a crash file is not found.
        for id in [
            "../seen",
            "crash-1-x.txt/../../x",
            "seen",
            "crash-9-none.txt",
        ] {
            assert_eq!(crash_report(dir.path(), id, None), None, "{id}");
        }
    }

    #[test]
    fn long_reports_are_cut_to_fit_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let backtrace: String = (0..2000).map(|i| format!("  {i}: frame ü\n")).collect();
        let path = write_crash(&dir.path().join("crashes"), "rust", "boom", &backtrace).unwrap();
        let id = path.file_name().unwrap().to_str().unwrap();
        let report = crash_report(dir.path(), id, Some("https://example.org/issues")).unwrap();
        let url = report.url.unwrap();
        assert!(url.len() <= REPORT_LINK_MAX, "{}", url.len());
        assert!(url.contains("cut%20to%20fit%20a%20link"));
        assert!(
            report.body.contains("1999: frame ü"),
            "the body stays whole"
        );
        assert!(url.contains("1%3A%20frame"), "it keeps what fits");

        // A message too long for any link.
        let message = "x".repeat(9000) + " ü";
        let path = write_crash(&dir.path().join("crashes"), "rust", &message, "").unwrap();
        let id = path.file_name().unwrap().to_str().unwrap();
        let report = crash_report(dir.path(), id, Some("https://example.org/issues")).unwrap();
        let url = report.url.unwrap();
        assert!(url.len() <= REPORT_LINK_MAX, "{}", url.len());
        assert!(url.contains("too%20long%20for%20a%20link"));
        assert!(report.body.contains(&message));
    }

    #[test]
    fn encoding() {
        let text = "a b+c/ü~";
        assert_eq!(percent_encode(text), "a%20b%2Bc%2F%C3%BC~");
        assert_eq!(encoded_len(text), percent_encode(text).len());
    }

    #[test]
    fn report_urls_are_checked() {
        assert!(check_report_url("https://github.com/Tidefell/tidefell/issues").is_ok());
        assert!(check_report_url("http://git.local/o/r/issues").is_ok());
        for bad in [
            "ftp://x/issues",
            "https://",
            "https:///x",
            "https://x/issues?y=1",
            "x",
        ] {
            assert!(check_report_url(bad).is_err(), "{bad}");
        }
    }
}
