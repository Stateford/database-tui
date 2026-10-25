//! The terminal while dbtui runs. Raw mode, the alternate screen and
//! bracketed paste are set up at start and undone on the way out, however
//! dbtui ends: by quitting, by a panic in the UI, or by a signal (SIGTERM,
//! SIGHUP).
//!
//! A panic on another thread, in database work for instance, does not end
//! dbtui, so it must not touch the terminal either. Work the app starts is
//! run through [`catch_panic`], which turns a panic into an error for
//! whatever waits for the result; any other panic is kept for the app to
//! report ([`take_background_panic`]). Nothing is printed over the screen.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::thread::ThreadId;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::FutureExt;
use ratatui::DefaultTerminal;
use ratatui::backend::CrosstermBackend;

/// The thread that draws the screen: a panic there ends dbtui.
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();
/// Whether the terminal is set up for the UI, and must be restored.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The last panic on another thread that nothing caught, for the app.
static BACKGROUND_PANIC: Mutex<Option<String>> = Mutex::new(None);

thread_local! {
    /// How many [`catch_panic`] calls this thread is running work for.
    static CATCHING: Cell<usize> = const { Cell::new(0) };
    /// The message of the panic [`catch_panic`] is about to catch.
    static CAUGHT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Sets the terminal up for the UI (raw mode, alternate screen, bracketed
/// paste), with dbtui's panic hook installed first. Call it on the thread
/// that draws the screen.
pub fn init() -> io::Result<DefaultTerminal> {
    install_panic_hook();
    ACTIVE.store(true, Ordering::SeqCst);
    let terminal = (|| {
        enable_raw_mode()?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
        // Not every terminal (or Windows console) supports it.
        let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
        ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))
    })();
    if terminal.is_err() {
        restore();
    }
    terminal
}

/// Undoes [`init`]. Harmless when there is nothing to undo.
pub fn restore() {
    ACTIVE.store(false, Ordering::SeqCst);
    reset();
}

fn reset() {
    // Raw mode first: it matters most, should the rest fail.
    let _ = disable_raw_mode();
    // Left on, the terminal would wrap whatever is pasted into the shell.
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Replaces the standard panic hook (and ratatui's, which restores the
/// terminal whichever thread panics while the app goes on drawing):
///
/// - a panic in work run by [`catch_panic`] is kept for it, to report;
/// - one on another thread while the UI runs is kept for the app to report
///   ([`take_background_panic`]): the app goes on, its screen untouched;
/// - otherwise (the UI thread, or no UI) the terminal is restored, and the
///   panic reported as usual.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    let _ = UI_THREAD.set(std::thread::current().id());
    INSTALLED.call_once(|| {
        let standard = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let message = describe(info);
            let ui_thread = UI_THREAD.get() == Some(&std::thread::current().id());
            if CATCHING.with(Cell::get) > 0 {
                CAUGHT.with(|caught| *caught.borrow_mut() = Some(message));
            } else if !ui_thread && ACTIVE.load(Ordering::SeqCst) {
                if let Ok(mut slot) = BACKGROUND_PANIC.lock() {
                    *slot = Some(message);
                }
            } else {
                if ACTIVE.swap(false, Ordering::SeqCst) {
                    reset();
                }
                standard(info);
            }
        }));
    });
}

/// A panic's message and where it happened.
fn describe(info: &PanicHookInfo<'_>) -> String {
    let message = payload_text(info.payload());
    match info.location() {
        Some(at) => format!("{message} (at {}:{})", at.file(), at.line()),
        None => message,
    }
}

fn payload_text(payload: &(dyn Any + Send)) -> String {
    match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(text), _) => text.to_string(),
        (_, Some(text)) => text.clone(),
        _ => "unknown panic".to_string(),
    }
}

/// Counts, for the panic hook, that this thread runs work [`catch_panic`]
/// watches.
struct Catching;

impl Catching {
    fn enter() -> Catching {
        CATCHING.with(|c| c.set(c.get() + 1));
        Catching
    }
}

impl Drop for Catching {
    fn drop(&mut self) {
        CATCHING.with(|c| c.set(c.get() - 1));
    }
}

/// The message of a panic caught on this thread.
fn caught(payload: &(dyn Any + Send)) -> String {
    CAUGHT.with(|caught| caught.borrow_mut().take()).unwrap_or_else(|| payload_text(payload))
}

/// Runs `work`; should it panic (a bug in a database driver, say), returns
/// the panic's message instead. Whatever waits for the work's result then
/// gets an answer, rather than waiting forever.
pub async fn catch_panic<T>(work: impl Future<Output = T>) -> Result<T, String> {
    let mut work = std::pin::pin!(work);
    let watched = std::future::poll_fn(|cx| {
        let _catching = Catching::enter();
        work.as_mut().poll(cx)
    });
    AssertUnwindSafe(watched).catch_unwind().await.map_err(|payload| caught(&*payload))
}

/// [`catch_panic`] for blocking work.
pub fn catch_panic_blocking<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    let _catching = Catching::enter();
    std::panic::catch_unwind(AssertUnwindSafe(work)).map_err(|payload| caught(&*payload))
}

/// A panic on another thread, outside [`catch_panic`], since the last call
/// (in a database driver's own thread, say): the app reports it.
pub fn take_background_panic() -> Option<String> {
    BACKGROUND_PANIC.lock().ok()?.take()
}

/// A signal that asks dbtui to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    pub name: &'static str,
    pub number: i32,
}

impl Stop {
    /// The exit status of a process ended by the signal, as shells give it.
    pub fn exit_code(self) -> u8 {
        (128 + self.number).clamp(0, 255) as u8
    }
}

/// Listens for the signals that ask dbtui to stop: SIGTERM, and SIGHUP
/// when its terminal goes away. They are handled like quitting, so that the
/// terminal is restored. Nothing is listened for on Windows.
pub struct Signals {
    #[cfg(unix)]
    streams: Vec<(tokio::signal::unix::Signal, Stop)>,
}

impl Signals {
    /// Starts listening; needs the async runtime.
    pub fn new() -> io::Result<Signals> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut streams = Vec::new();
            for (kind, name, number) in
                [(SignalKind::terminate(), "SIGTERM", libc::SIGTERM), (SignalKind::hangup(), "SIGHUP", libc::SIGHUP)]
            {
                streams.push((signal(kind)?, Stop { name, number }));
            }
            Ok(Signals { streams })
        }
        #[cfg(not(unix))]
        Ok(Signals {})
    }

    /// Runs `work`, unless a signal asks dbtui to stop first. Anything dbtui
    /// waits for once the UI is gone goes through this: the signals are
    /// handled, so they would otherwise no longer end the process.
    pub async fn run_until<T>(&mut self, work: impl Future<Output = T>) -> Result<T, Stop> {
        tokio::select! {
            out = work => Ok(out),
            stop = self.recv() => Err(stop),
        }
    }

    /// The next signal asking dbtui to stop.
    pub async fn recv(&mut self) -> Stop {
        #[cfg(unix)]
        {
            let waits = self.streams.iter_mut().map(|(stream, stop)| {
                Box::pin(async move {
                    if stream.recv().await.is_none() {
                        std::future::pending::<()>().await;
                    }
                    *stop
                })
            });
            futures::future::select_all(waits).await.0
        }
        #[cfg(not(unix))]
        std::future::pending().await
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    async fn driver_bug() -> u8 {
        tokio::task::yield_now().await;
        panic!("driver bug")
    }

    #[tokio::test]
    async fn panics_in_work_become_errors() {
        let error = catch_panic(driver_bug()).await.unwrap_err();
        assert!(error.starts_with("driver bug"), "{error}");
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
        assert_eq!(catch_panic_blocking(|| -> u8 { panic!("{} failed", "keyring") }), Err("keyring failed".into()));
        assert_eq!(CATCHING.with(Cell::get), 0);
    }

    /// Audit L6: a panic on another thread leaves the terminal and the screen
    /// alone and is kept for the app; one in the UI thread restores the
    /// terminal, bracketed paste included, before it is reported. The hook
    /// is process-wide, so this runs in a child process.
    #[test]
    fn panic_hook_restores_the_terminal_only_for_the_ui() {
        const CHILD: &str = "DBTUI_TEST_PANIC_HOOK";
        let name =
            format!("{}::panic_hook_restores_the_terminal_only_for_the_ui", module_path!().split_once("::").unwrap().1);
        if std::env::var_os(CHILD).is_some() {
            install_panic_hook();
            ACTIVE.store(true, Ordering::SeqCst);
            let worker = std::thread::spawn(|| panic!("in a worker"));
            assert!(worker.join().is_err());
            // The location uses the platform's path separator.
            let kept = take_background_panic().unwrap().replace('\\', "/");
            assert!(kept.starts_with("in a worker (at src/terminal.rs:"), "{kept}");
            assert!(ACTIVE.load(Ordering::SeqCst), "the UI goes on");
            let caught = catch_panic_blocking(|| panic!("caught"));
            assert!(caught.unwrap_err().starts_with("caught (at "));
            assert!(take_background_panic().is_none());
            println!("--- before the UI panic");
            panic!("in the UI");
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([&name, "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(!out.status.success());
        let (before, after) = stdout.split_once("--- before the UI panic").expect("the child ran");
        // Nothing reached the screen from the worker or the caught panic.
        assert!(!before.contains('\x1b') && !stderr.contains("in a worker") && !stderr.contains("caught"), "{stderr}");
        // A Windows console may be reset through its API instead, which
        // writes nothing to the (piped) output.
        #[cfg(unix)]
        assert!(after.contains("\x1b[?2004l") && after.contains("\x1b[?1049l"), "{after:?}");
        #[cfg(not(unix))]
        let _ = after;
        assert!(stderr.contains("in the UI"), "{stderr}");
    }

    #[test]
    fn signal_exit_codes() {
        assert_eq!(Stop { name: "SIGTERM", number: 15 }.exit_code(), 143);
        assert_eq!(Stop { name: "SIGHUP", number: 1 }.exit_code(), 129);
    }

    /// SIGTERM and SIGHUP are received as requests to stop (and no longer
    /// end the process before the terminal is restored).
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_signals_are_received() {
        let mut signals = Signals::new().unwrap();
        for (number, name) in [(libc::SIGHUP, "SIGHUP"), (libc::SIGTERM, "SIGTERM")] {
            // SAFETY: plain system call; the signal is handled above.
            unsafe { libc::raise(number) };
            let stop = tokio::time::timeout(std::time::Duration::from_secs(5), signals.recv()).await.unwrap();
            assert_eq!(stop, Stop { name, number });
        }
        // Waiting for something once the UI is gone (a connection to close,
        // the keyring) still ends on a signal.
        assert_eq!(signals.run_until(async { 7 }).await, Ok(7));
        let stuck = async {
            // SAFETY: as above.
            unsafe { libc::raise(libc::SIGTERM) };
            std::future::pending::<()>().await
        };
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(5), signals.run_until(stuck)).await.unwrap();
        assert_eq!(stopped, Err(Stop { name: "SIGTERM", number: libc::SIGTERM }));
    }
}
