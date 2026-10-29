//! Event loop, interactive child handoff, and terminal lifecycle for the TUI.

use crate::app::{AppShell, ShellOutcome};
use crate::config::TuiConfig;
use crate::event::TuiEvent;
use crate::panel::TuiPanel;
use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

pub(crate) struct TerminalRequest {
    pub(crate) command: Command,
    pub(crate) reply: oneshot::Sender<io::Result<ExitStatus>>,
}

/// Shutdown receiver owned by the runtime.
pub(crate) struct RuntimeShutdown {
    rx: watch::Receiver<bool>,
}

impl RuntimeShutdown {
    pub(crate) fn new(rx: watch::Receiver<bool>) -> Self {
        Self { rx }
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// The modes guard restores the terminal on errors, panic and task cancellation.
pub(crate) async fn run(
    panels: Vec<Box<dyn TuiPanel>>,
    config: TuiConfig,
    shutdown: RuntimeShutdown,
    requests: mpsc::Receiver<TerminalRequest>,
) -> io::Result<()> {
    let mut modes = TerminalModes::default();
    modes.resume()?;
    modes.enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if modes.enhanced {
        // Mark before writing: even a partial write gets a defensive pop.
        modes.keyboard_active = true;
        io::stdout().execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))?;
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(
        &mut terminal,
        &mut modes,
        panels,
        &config,
        shutdown,
        requests,
    )
    .await;
    result.and(modes.suspend())
}

async fn event_loop(
    terminal: &mut Term,
    modes: &mut TerminalModes,
    panels: Vec<Box<dyn TuiPanel>>,
    config: &TuiConfig,
    mut shutdown: RuntimeShutdown,
    mut requests: mpsc::Receiver<TerminalRequest>,
) -> io::Result<()> {
    let mut shell = AppShell::new(panels, config);
    shell.init_panels();
    let mut tick = tokio::time::interval(config.tick_rate);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    terminal.draw(|frame| shell.render(frame))?;
    loop {
        if *shutdown.rx.borrow() {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.rx.changed() => break,
            Some(request) = requests.recv() => {
                if request.reply.is_closed() { continue; }
                // No input future is polled while this operation owns the
                // terminal. Unlike EventStream, there is no background reader
                // that can race a drop and steal the editor's first keystroke.
                let result = terminal_round_trip(modes, request.command, &mut shutdown.rx).await;
                let restored = result.restored;
                let _ = request.reply.send(result.child);
                restored?;
                terminal.clear()?;
                terminal.autoresize()?;
                let size = terminal.size()?;
                shell.handle_event(TuiEvent::Resize { cols: size.width, rows: size.height });
                shell.tick();
                terminal.draw(|frame| shell.render(frame))?;
            }
            event = next_terminal_event() => {
                match event {
                    Ok(ct) => {
                        if let Some(event) = translate_event(ct) {
                            match shell.handle_event(event) {
                                ShellOutcome::Quit => break,
                                ShellOutcome::ToggleMouse => {
                                    modes.mouse = shell.mouse_captured();
                                    apply_mouse_capture(terminal, modes.mouse)?;
                                }
                                ShellOutcome::Continue => {}
                            }
                        }
                    }
                    Err(err) => return Err(err),
                }
                terminal.draw(|frame| shell.render(frame))?;
            }
            _ = tick.tick() => {
                shell.tick();
                if matches!(shell.handle_event(TuiEvent::Tick), ShellOutcome::Quit) { break; }
                terminal.draw(|frame| shell.render(frame))?;
            }
        }
    }
    Ok(())
}

/// Poll without a helper thread: dropping this future fully stops input reads.
/// `read` follows only a successful poll, so it never waits for input.
async fn next_terminal_event() -> io::Result<CtEvent> {
    loop {
        if crossterm::event::poll(Duration::ZERO)? {
            return crossterm::event::read();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn translate_event(event: CtEvent) -> Option<TuiEvent> {
    match event {
        CtEvent::Key(k) if k.kind != KeyEventKind::Press => None,
        CtEvent::Key(k) => Some(TuiEvent::Key(k)),
        CtEvent::Mouse(m) => Some(TuiEvent::Mouse(m)),
        CtEvent::Resize(cols, rows) => Some(TuiEvent::Resize { cols, rows }),
        CtEvent::FocusGained | CtEvent::FocusLost => None,
        CtEvent::Paste(text) => Some(TuiEvent::Paste(text)),
    }
}

fn apply_mouse_capture(terminal: &mut Term, capture: bool) -> io::Result<()> {
    if capture {
        terminal.backend_mut().execute(EnableMouseCapture)?;
    } else {
        terminal.backend_mut().execute(DisableMouseCapture)?;
    }
    Ok(())
}

trait TerminalHandoff {
    fn suspend(&mut self) -> io::Result<()>;
    fn prepare_child(&mut self) -> io::Result<()> {
        self.suspend()
    }
    fn resume(&mut self) -> io::Result<()>;
}

#[derive(Default)]
struct TerminalModes {
    active: bool,
    enhanced: bool,
    keyboard_active: bool,
    mouse: bool,
    #[cfg(unix)]
    child_settings: Option<TtySettings>,
}

impl TerminalHandoff for TerminalModes {
    fn prepare_child(&mut self) -> io::Result<()> {
        self.suspend()?;
        // crossterm consumes its termios snapshot on disable_raw_mode. Keep
        // the cooked settings across the child as well: an editor that dies
        // in raw mode cannot restore them itself, and a subsequent enable
        // would otherwise remember that broken state as the new original.
        #[cfg(unix)]
        {
            self.child_settings = Some(TtySettings::capture()?);
        }
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(settings) = &mut self.child_settings {
            settings.restore()?;
            self.child_settings = None;
        }
        if self.active {
            self.suspend()?;
        }
        self.active = true;
        enable_raw_mode()?;
        let mut out = io::stdout();
        out.execute(EnterAlternateScreen)?;
        out.execute(EnableBracketedPaste)?;
        if self.enhanced {
            self.keyboard_active = true;
            out.execute(PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            ))?;
        }
        if self.mouse {
            out.execute(EnableMouseCapture)?;
        }
        Ok(())
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut out = io::stdout();
        // Attempt every restoration step even when one fails. The first error
        // is reported, and Drop retries any incomplete restoration.
        let mut result = Ok(());
        result = result.and(out.execute(DisableMouseCapture).map(|_| ()));
        if self.keyboard_active {
            let popped = out.execute(PopKeyboardEnhancementFlags).map(|_| ());
            if popped.is_ok() {
                self.keyboard_active = false;
            }
            result = result.and(popped);
        }
        result = result.and(out.execute(DisableBracketedPaste).map(|_| ()));
        result = result.and(out.execute(LeaveAlternateScreen).map(|_| ()));
        result = result.and(out.execute(Show).map(|_| ()));
        result = result.and(disable_raw_mode());
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        let _ = self.suspend();
    }
}

/// POSIX stty carries opaque termios settings without adding platform FFI.
/// Used only for interactive child handoffs; a missing stty prevents launch
/// with a recoverable error, and ordinary TUI startup does not need it.
#[cfg(unix)]
struct TtySettings {
    tty: std::fs::File,
    encoded: String,
    restored: bool,
}

#[cfg(unix)]
impl TtySettings {
    fn capture() -> io::Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        let output = Command::new("stty")
            .arg("-g")
            .stdin(tty.try_clone()?)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("Cannot save terminal settings with stty"));
        }
        let encoded = String::from_utf8(output.stdout)
            .map_err(io::Error::other)?
            .trim()
            .to_owned();
        if encoded.is_empty() {
            return Err(io::Error::other("stty returned empty terminal settings"));
        }
        Ok(Self {
            tty,
            encoded,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let output = Command::new("stty")
            .arg(&self.encoded)
            .stdin(self.tty.try_clone()?)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "Cannot restore terminal settings with stty",
            ));
        }
        self.restored = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TtySettings {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct RoundTripResult {
    child: io::Result<ExitStatus>,
    restored: io::Result<()>,
}

async fn terminal_round_trip(
    terminal: &mut impl TerminalHandoff,
    command: Command,
    shutdown: &mut watch::Receiver<bool>,
) -> RoundTripResult {
    let suspended = terminal.prepare_child();
    // If the future is cancelled or panics while the child is running, resume
    // before the enclosing modes guard restores the user's terminal on exit.
    let mut handoff = ResumeOnDrop {
        terminal,
        resumed: false,
    };
    let child = match suspended {
        Ok(()) => run_child(command, shutdown).await,
        Err(error) => Err(error),
    };
    let restored = handoff.terminal.resume();
    handoff.resumed = restored.is_ok();
    let child = match &restored {
        Ok(()) => child,
        Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
    };
    RoundTripResult { child, restored }
}

struct ResumeOnDrop<'a, T: TerminalHandoff> {
    terminal: &'a mut T,
    resumed: bool,
}

impl<T: TerminalHandoff> Drop for ResumeOnDrop<'_, T> {
    fn drop(&mut self) {
        if !self.resumed {
            let _ = self.terminal.resume();
        }
    }
}

struct InteractiveChild(Child);

impl Drop for InteractiveChild {
    fn drop(&mut self) {
        // Also covers a runtime future aborted while the child is active.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn run_child(
    mut command: Command,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<ExitStatus> {
    if *shutdown.borrow() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "TUI shutting down",
        ));
    }
    let mut child = InteractiveChild(
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        tokio::select! {
            _ = shutdown.changed() => {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "TUI shutting down"));
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

    #[test]
    fn translates_key_mouse_and_resize() {
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(matches!(
            translate_event(CtEvent::Key(key)),
            Some(TuiEvent::Key(_))
        ));

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            translate_event(CtEvent::Mouse(mouse)),
            Some(TuiEvent::Mouse(_))
        ));

        assert!(matches!(
            translate_event(CtEvent::Resize(80, 24)),
            Some(TuiEvent::Resize { cols: 80, rows: 24 })
        ));
    }

    #[test]
    fn drops_terminal_focus_events() {
        // Terminal focus is the *terminal's* notion; the shell publishes its
        // own on panel switch.
        assert!(translate_event(CtEvent::FocusGained).is_none());
        assert!(translate_event(CtEvent::FocusLost).is_none());
    }

    #[test]
    fn forwards_bracketed_paste_as_one_payload() {
        // The whole point: a two-line paste is one event, not a key burst with
        // an Enter in the middle that would submit the first line.
        let pasted = translate_event(CtEvent::Paste("one\ntwo".into()));
        match pasted {
            Some(TuiEvent::Paste(text)) => assert_eq!(text, "one\ntwo"),
            other => panic!("expected a paste event, got {other:?}"),
        }
    }

    #[test]
    fn drops_key_releases_and_repeats() {
        // One physical keystroke must reach panels once. The keyboard
        // enhancement protocol and Windows both surface non-press kinds.
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let key = KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::NONE,
                kind,
                state: KeyEventState::NONE,
            };
            assert!(
                translate_event(CtEvent::Key(key)).is_none(),
                "{kind:?} must not reach panels"
            );
        }
    }
    #[derive(Default)]
    struct FakeTerminal {
        transitions: Vec<&'static str>,
        fail_suspend: bool,
        fail_resume: bool,
    }

    impl TerminalHandoff for FakeTerminal {
        fn suspend(&mut self) -> io::Result<()> {
            self.transitions.push("suspend");
            if self.fail_suspend {
                Err(io::Error::other("suspend failed"))
            } else {
                Ok(())
            }
        }
        fn resume(&mut self) -> io::Result<()> {
            self.transitions.push("resume");
            if self.fail_resume {
                Err(io::Error::other("resume failed"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn spawn_and_suspend_failures_restore_terminal_before_reply() {
        let (_tx, mut shutdown) = watch::channel(false);
        let mut terminal = FakeTerminal::default();
        let result = terminal_round_trip(
            &mut terminal,
            Command::new("/nonexistent/polaris-editor"),
            &mut shutdown,
        )
        .await;
        assert_eq!(result.child.unwrap_err().kind(), io::ErrorKind::NotFound);
        result.restored.unwrap();
        assert_eq!(terminal.transitions, ["suspend", "resume"]);
        let mut terminal = FakeTerminal {
            fail_suspend: true,
            ..Default::default()
        };
        let result = terminal_round_trip(
            &mut terminal,
            Command::new("/nonexistent/polaris-editor"),
            &mut shutdown,
        )
        .await;
        assert_eq!(result.child.unwrap_err().to_string(), "suspend failed");
        result.restored.unwrap();
        assert_eq!(terminal.transitions, ["suspend", "resume"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_editor_round_trip_uses_runtime_handle_and_restores_on_nonzero_exit() {
        use crate::composer::editor::{EditorCommand, edit_draft};
        for (script, expected) in [
            ("printf '世界\\nline\\n' > \"$1\"", Some("世界\nline\n")),
            ("exit 7", None),
        ] {
            let (handle, mut requests) = crate::plugin::test_handle();
            let editor = EditorCommand::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("fake-editor");
            let draft = tokio::spawn(edit_draft(handle, editor, "original".into()));
            let request = tokio::time::timeout(Duration::from_secs(2), requests.recv())
                .await
                .unwrap()
                .unwrap();
            let mut terminal = FakeTerminal::default();
            let (_tx, mut shutdown) = watch::channel(false);
            let result = terminal_round_trip(&mut terminal, request.command, &mut shutdown).await;
            result.restored.unwrap();
            assert_eq!(terminal.transitions, ["suspend", "resume"]);
            request.reply.send(result.child).unwrap();
            let result = draft.await.unwrap();
            match expected {
                Some(text) => assert_eq!(result.unwrap(), text),
                None => assert!(result.unwrap_err().to_string().contains("draft unchanged")),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_and_future_cancellation_restore_terminal_and_reap_child() {
        let mut terminal = FakeTerminal::default();
        let (tx, mut shutdown) = watch::channel(false);
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let mut round_trip = Box::pin(terminal_round_trip(&mut terminal, command, &mut shutdown));
        assert!(futures::poll!(&mut round_trip).is_pending());
        tx.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), round_trip)
            .await
            .unwrap();
        assert_eq!(result.child.unwrap_err().kind(), io::ErrorKind::Interrupted);
        result.restored.unwrap();
        assert_eq!(terminal.transitions, ["suspend", "resume"]);

        let (_tx, mut shutdown) = watch::channel(false);
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let mut round_trip = Box::pin(terminal_round_trip(&mut terminal, command, &mut shutdown));
        assert!(futures::poll!(&mut round_trip).is_pending());
        drop(round_trip);
        assert_eq!(
            terminal.transitions,
            ["suspend", "resume", "suspend", "resume"]
        );
    }

    #[tokio::test]
    async fn restore_failure_is_reported_to_both_caller_and_runtime() {
        let (_tx, mut shutdown) = watch::channel(false);
        let mut terminal = FakeTerminal {
            fail_resume: true,
            ..Default::default()
        };
        let result = terminal_round_trip(
            &mut terminal,
            Command::new("/nonexistent/editor"),
            &mut shutdown,
        )
        .await;
        assert_eq!(result.restored.unwrap_err().to_string(), "resume failed");
        assert_eq!(result.child.unwrap_err().to_string(), "resume failed");
    }
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires an isolated controlling PTY; see docs/prompt-history-editor.md"]
    async fn unix_handoff_restores_termios_left_raw_by_a_failed_child() {
        let before = TtySettings::capture().unwrap();
        let mut modes = TerminalModes::default();
        modes.resume().unwrap();
        let (_tx, mut shutdown) = watch::channel(false);
        // This fake editor deliberately changes tty settings and exits without
        // restoring them. No real interactive editor or user input is needed.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "stty raw -echo; exit 9"]);
        let result = terminal_round_trip(&mut modes, command, &mut shutdown).await;
        assert!(!result.child.unwrap().success());
        result.restored.unwrap();
        modes.suspend().unwrap();
        let after = TtySettings::capture().unwrap();
        assert_eq!(
            comparable_tty_settings(&after.encoded),
            comparable_tty_settings(&before.encoded),
            "a failed raw-mode editor must not change the user's tty settings"
        );
    }
    #[cfg(unix)]
    fn comparable_tty_settings(encoded: &str) -> String {
        #[cfg(target_os = "macos")]
        {
            // Darwin's termios.h marks PENDIN (0x20000000) as pending-input
            // *state*, not a configured mode. tcsetattr may set it while
            // reprocessing input. Compare every configured bit and control
            // character, ignoring only this transient kernel state.
            encoded
                .split(':')
                .map(|field| {
                    if let Some(flags) = field.strip_prefix("lflag=") {
                        format!(
                            "lflag={:x}",
                            u64::from_str_radix(flags, 16).unwrap() & !0x20000000
                        )
                    } else {
                        field.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(":")
        }
        #[cfg(not(target_os = "macos"))]
        {
            encoded.to_owned()
        }
    }
}
