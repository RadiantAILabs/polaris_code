//! Event loop, interactive child handoff, and terminal lifecycle for the TUI.

use crate::app::{AppShell, ShellOutcome};
use crate::config::TuiConfig;
use crate::event::TuiEvent;
use crate::notify::{Notification, RESTORE_TITLE};
use crate::panel::TuiPanel;
use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, KeyEventKind, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::Print;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use parking_lot::Mutex;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Write};
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

type Term<W> = Terminal<CrosstermBackend<W>>;

/// Input redraws run independently of panel polling. Applying every event in
/// order preserves scroll distance and direction changes, while drawing their
/// combined result avoids one full-screen render per wheel/trackpad report.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

struct FrameSchedule {
    next: tokio::time::Instant,
    dirty: bool,
}

impl FrameSchedule {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            next: now + FRAME_INTERVAL,
            dirty: false,
        }
    }

    fn request(&mut self) {
        self.dirty = true;
    }

    fn rendered(&mut self, now: tokio::time::Instant) {
        self.next = now + FRAME_INTERVAL;
        self.dirty = false;
    }
}

/// What wakes [`event_loop`]: the clock, the tick timer and terminal input.
/// [`Live`] is the real set; a test supplies its own to run the production loop
/// in controlled time. Methods take `&self` so one `select!` can wait on all of
/// them at once.
trait LoopSource {
    /// The current time, against which frames are scheduled.
    fn now(&self) -> tokio::time::Instant;
    /// Complete once [`now`](Self::now) reaches `at`.
    async fn sleep_until(&self, at: tokio::time::Instant);
    /// Complete at each tick of the shell's wall clock.
    async fn tick(&self);
    /// The next terminal event.
    async fn next_event(&self) -> io::Result<CtEvent>;
}

/// The real clock, tick timer and keyboard.
struct Live {
    tick: Mutex<tokio::time::Interval>,
}

impl Live {
    fn new(tick_rate: Duration) -> Self {
        let mut tick = tokio::time::interval(tick_rate);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            tick: Mutex::new(tick),
        }
    }
}

impl LoopSource for Live {
    fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    async fn sleep_until(&self, at: tokio::time::Instant) {
        tokio::time::sleep_until(at).await;
    }

    async fn tick(&self) {
        // The lock is held only inside one poll, never across an await.
        std::future::poll_fn(|cx| self.tick.lock().poll_tick(cx).map(|_| ())).await;
    }

    async fn next_event(&self) -> io::Result<CtEvent> {
        next_terminal_event().await
    }
}

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
        &Live::new(config.tick_rate),
    )
    .await;
    result.and(modes.suspend())
}

async fn event_loop<W: Write>(
    terminal: &mut Term<W>,
    modes: &mut TerminalModes,
    panels: Vec<Box<dyn TuiPanel>>,
    config: &TuiConfig,
    mut shutdown: RuntimeShutdown,
    mut requests: mpsc::Receiver<TerminalRequest>,
    source: &impl LoopSource,
) -> io::Result<()> {
    let mut shell = AppShell::new(panels, config);
    shell.init_panels();
    terminal.draw(|frame| shell.render(frame))?;
    let mut frames = FrameSchedule::new(source.now());
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
                deliver_notifications(modes, &mut shell);
                terminal.draw(|frame| shell.render(frame))?;
                frames.rendered(source.now());
            }
            // A due frame wins over more input, so continuous wheel traffic
            // cannot starve painting. Shutdown and child handoffs stay first.
            _ = source.sleep_until(frames.next), if frames.dirty => {
                terminal.draw(|frame| shell.render(frame))?;
                frames.rendered(source.now());
            }
            _ = source.tick() => {
                shell.tick();
                if matches!(shell.handle_event(TuiEvent::Tick), ShellOutcome::Quit) { break; }
                deliver_notifications(modes, &mut shell);
                frames.request();
            }
            event = source.next_event() => {
                match event {
                    Ok(ct) => {
                        if let Some(event) = translate_event(ct) {
                            // The user is at the keyboard again, so a title
                            // that asked for them has done its job.
                            if is_user_input(&event) {
                                modes.restore_title();
                            }
                            match shell.handle_event(event) {
                                ShellOutcome::Quit => break,
                                ShellOutcome::ToggleMouse => {
                                    modes.mouse = shell.mouse_captured();
                                    apply_mouse_capture(terminal, modes.mouse)?;
                                }
                                ShellOutcome::Continue => {}
                            }
                            frames.request();
                        }
                    }
                    Err(err) => return Err(err),
                }
                deliver_notifications(modes, &mut shell);
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

/// Whether `event` shows a person at the keyboard. Pointer movement alone
/// does not: with mouse tracking on, a terminal reports every movement.
fn is_user_input(event: &TuiEvent) -> bool {
    match event {
        TuiEvent::Key(_) | TuiEvent::Paste(_) => true,
        TuiEvent::Mouse(mouse) => mouse.kind != MouseEventKind::Moved,
        _ => false,
    }
}

/// Write every attention request the panels raised. Best effort: failing to
/// ring a bell is not a reason to stop the shell.
fn deliver_notifications(modes: &mut TerminalModes, shell: &mut AppShell) {
    for notification in shell.take_notifications() {
        if let Err(error) = modes.notify(&notification) {
            tracing::debug!(%error, "could not deliver a terminal notification");
        }
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

fn apply_mouse_capture<W: Write>(terminal: &mut Term<W>, capture: bool) -> io::Result<()> {
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
    /// Whether the terminal was asked to save its window title, so a
    /// notification title can be undone.
    title_saved: bool,
    #[cfg(unix)]
    child_settings: Option<TtySettings>,
}

impl TerminalModes {
    /// Write a notification's bell and title.
    fn notify(&mut self, notification: &Notification) -> io::Result<()> {
        let bytes = notification.encode(self.title_saved);
        if bytes.is_empty() {
            return Ok(());
        }
        // Mark before writing: even a partial write gets a defensive restore.
        self.title_saved |= notification.sets_title();
        io::stdout().execute(Print(bytes)).map(|_| ())
    }

    /// Put back the window title saved before the first notification.
    fn restore_title(&mut self) {
        if !self.title_saved {
            return;
        }
        if io::stdout().execute(Print(RESTORE_TITLE)).is_ok() {
            self.title_saved = false;
        }
    }
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
        if self.title_saved {
            let restored = out.execute(Print(RESTORE_TITLE)).map(|_| ());
            if restored.is_ok() {
                self.title_saved = false;
            }
            result = result.and(restored);
        }
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
    use crate::panel::EventOutcome;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::{Frame, TerminalOptions, Viewport};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn input_bursts_share_one_frame_without_extending_the_deadline() {
        let start = tokio::time::Instant::now();
        let mut frames = FrameSchedule::new(start);
        for _ in 0..100 {
            frames.request();
        }
        assert!(frames.dirty);
        assert_eq!(frames.next, start + FRAME_INTERVAL);
        frames.rendered(frames.next);
        assert!(!frames.dirty);
        assert_eq!(frames.next, start + FRAME_INTERVAL * 2);
        // After an idle period, a request is already due: no tick-sized wait.
        frames.request();
        assert!(frames.next < start + Duration::from_secs(1));
    }

    /// What the probe panel was shown, in the order the loop delivered it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Seen {
        Up(u16),
        Down(u16),
        Tick,
    }

    #[derive(Default)]
    struct ProbeLog {
        seen: Vec<Seen>,
        /// How many events the panel had seen at each paint, the loop's first
        /// paint included.
        painted_after: Vec<usize>,
    }

    /// A panel that records its input and its paints for the test to read.
    struct Probe(Arc<Mutex<ProbeLog>>);

    impl TuiPanel for Probe {
        fn id(&self) -> &str {
            "probe"
        }

        fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
            let seen = match event {
                TuiEvent::Mouse(m) if m.kind == MouseEventKind::ScrollUp => Seen::Up(m.column),
                TuiEvent::Mouse(m) if m.kind == MouseEventKind::ScrollDown => Seen::Down(m.column),
                TuiEvent::Tick => Seen::Tick,
                _ => return EventOutcome::Continue,
            };
            self.0.lock().seen.push(seen);
            EventOutcome::Consumed
        }

        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {
            let mut log = self.0.lock();
            let seen = log.seen.len();
            log.painted_after.push(seen);
        }
    }

    struct FakeTime {
        start: tokio::time::Instant,
        elapsed: Duration,
        /// Time that passes as each queued event is delivered, so that one poll
        /// of the loop can span many frames.
        per_event: Duration,
        events: VecDeque<CtEvent>,
        ticks: usize,
    }

    /// Time and input for the production loop, controlled by the test. Nothing
    /// here waits: each future is ready or pending according to this state, and
    /// the test changes the state between polls of the loop.
    #[derive(Clone)]
    struct Fake(Rc<RefCell<FakeTime>>);

    impl Fake {
        fn new() -> Self {
            Self(Rc::new(RefCell::new(FakeTime {
                start: tokio::time::Instant::now(),
                elapsed: Duration::ZERO,
                per_event: Duration::ZERO,
                events: VecDeque::new(),
                ticks: 0,
            })))
        }

        fn advance(&self, by: Duration) {
            self.0.borrow_mut().elapsed += by;
        }

        fn send(&self, event: CtEvent) {
            self.0.borrow_mut().events.push_back(event);
        }

        fn fire_tick(&self) {
            self.0.borrow_mut().ticks += 1;
        }
    }

    impl LoopSource for Fake {
        fn now(&self) -> tokio::time::Instant {
            let time = self.0.borrow();
            time.start + time.elapsed
        }

        async fn sleep_until(&self, at: tokio::time::Instant) {
            std::future::poll_fn(|_| {
                if self.now() >= at {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }

        async fn tick(&self) {
            std::future::poll_fn(|_| {
                let mut time = self.0.borrow_mut();
                if time.ticks > 0 {
                    time.ticks -= 1;
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }

        async fn next_event(&self) -> io::Result<CtEvent> {
            std::future::poll_fn(|_| {
                let mut time = self.0.borrow_mut();
                match time.events.pop_front() {
                    Some(event) => {
                        let per_event = time.per_event;
                        time.elapsed += per_event;
                        Poll::Ready(Ok(event))
                    }
                    None => Poll::Pending,
                }
            })
            .await
        }
    }

    /// The production [`event_loop`] over a probe panel, with fake time, fake
    /// input and a terminal that writes to memory.
    struct Rig {
        fake: Fake,
        log: Arc<Mutex<ProbeLog>>,
        shutdown: watch::Sender<bool>,
        event_loop: Pin<Box<dyn Future<Output = io::Result<()>>>>,
    }

    impl Rig {
        /// Start the loop and run it until it first waits.
        fn start() -> Self {
            let fake = Fake::new();
            let log = Arc::new(Mutex::new(ProbeLog::default()));
            let (shutdown, shutdown_rx) = watch::channel(false);
            let panels: Vec<Box<dyn TuiPanel>> = vec![Box::new(Probe(log.clone()))];
            let source = fake.clone();
            let event_loop = Box::pin(async move {
                let backend = CrosstermBackend::new(Vec::new());
                let options = TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
                };
                let mut terminal = Terminal::with_options(backend, options)?;
                let mut modes = TerminalModes::default();
                let (_requests_tx, requests) = mpsc::channel(1);
                event_loop(
                    &mut terminal,
                    &mut modes,
                    panels,
                    &TuiConfig::default(),
                    RuntimeShutdown::new(shutdown_rx),
                    requests,
                    &source,
                )
                .await
            });
            let mut rig = Self {
                fake,
                log,
                shutdown,
                event_loop,
            };
            assert!(rig.step().is_pending());
            assert_eq!(rig.paints(), 1, "the loop paints once before any input");
            rig
        }

        /// Poll the loop once. It keeps running while any input or timer is
        /// ready, so this returns when the loop would wait or has finished.
        fn step(&mut self) -> Poll<io::Result<()>> {
            self.event_loop
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        }

        fn paints(&self) -> usize {
            self.log.lock().painted_after.len()
        }

        fn seen(&self) -> Vec<Seen> {
            self.log.lock().seen.clone()
        }
    }

    fn wheel(kind: MouseEventKind, id: u16) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: id,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_wheel_burst_is_applied_in_order_and_painted_once() {
        let mut rig = Rig::start();
        // Keep the direction change: netting these events before delivering
        // them would lose clamping and tail-following at either end.
        let expected: Vec<Seen> = (0..100)
            .map(|id| {
                if id < 80 {
                    Seen::Up(id)
                } else {
                    Seen::Down(id)
                }
            })
            .collect();
        for seen in &expected {
            rig.fake.send(match *seen {
                Seen::Up(id) => wheel(MouseEventKind::ScrollUp, id),
                Seen::Down(id) => wheel(MouseEventKind::ScrollDown, id),
                Seen::Tick => unreachable!("the burst has no ticks"),
            });
        }
        assert!(rig.step().is_pending());
        assert_eq!(rig.seen(), expected, "every report arrives, in order");
        assert_eq!(rig.paints(), 1, "no paint before the frame is due");

        rig.fake.advance(FRAME_INTERVAL);
        assert!(rig.step().is_pending());
        assert_eq!(rig.paints(), 2, "100 reports need one full-screen paint");
        assert_eq!(rig.log.lock().painted_after[1], 100);

        rig.fake.advance(Duration::from_secs(1));
        assert!(rig.step().is_pending());
        assert_eq!(rig.paints(), 2, "an idle loop does not repaint");
    }

    #[test]
    fn continuous_input_is_painted_while_it_keeps_arriving() {
        let mut rig = Rig::start();
        // One report every 5 ms for half a second, all queued before the loop
        // runs. The loop never idles, so only its own priority between a due
        // frame and more input can interleave paints with the reports.
        let per_event = Duration::from_millis(5);
        rig.fake.0.borrow_mut().per_event = per_event;
        for id in 0..100 {
            rig.fake.send(wheel(MouseEventKind::ScrollUp, id));
        }
        assert!(rig.step().is_pending());
        assert_eq!(rig.seen().len(), 100);

        let painted = rig.log.lock().painted_after.clone();
        let mid_stream = painted.iter().filter(|&&n| 0 < n && n < 100).count();
        let at_least = (per_event * 100).as_millis() / (2 * FRAME_INTERVAL.as_millis());
        assert!(
            mid_stream as u128 >= at_least,
            "{mid_stream} paints while input arrived, expected at least {at_least}: {painted:?}"
        );
        assert!(
            painted.len() < 100 / 2,
            "100 reports must not paint per report: {painted:?}"
        );
    }

    #[test]
    fn a_tick_asks_for_a_frame() {
        let mut rig = Rig::start();
        rig.fake.fire_tick();
        assert!(rig.step().is_pending());
        assert_eq!(rig.seen(), [Seen::Tick]);
        assert_eq!(rig.paints(), 1, "the frame is not due yet");

        rig.fake.advance(FRAME_INTERVAL);
        assert!(rig.step().is_pending());
        assert_eq!(rig.paints(), 2);
        assert_eq!(rig.log.lock().painted_after[1], 1);
    }

    #[test]
    fn shutdown_wins_over_a_due_frame() {
        let mut rig = Rig::start();
        rig.fake.send(wheel(MouseEventKind::ScrollUp, 0));
        assert!(rig.step().is_pending());
        // The frame is now due and input is waiting, but shutdown was asked for.
        rig.fake.advance(FRAME_INTERVAL);
        rig.fake.send(wheel(MouseEventKind::ScrollUp, 1));
        rig.shutdown.send(true).unwrap();
        assert!(matches!(rig.step(), Poll::Ready(Ok(()))));
        assert_eq!(rig.paints(), 1, "no paint after shutdown is asked for");
    }

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
    fn typing_and_clicking_count_as_presence_but_moving_the_pointer_does_not() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let mouse = |kind| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(is_user_input(&TuiEvent::Key(key)));
        assert!(is_user_input(&TuiEvent::Paste("text".into())));
        assert!(is_user_input(&mouse(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(is_user_input(&mouse(MouseEventKind::ScrollUp)));
        assert!(!is_user_input(&mouse(MouseEventKind::Moved)));
        assert!(!is_user_input(&TuiEvent::Tick));
        assert!(!is_user_input(&TuiEvent::Resize { cols: 80, rows: 24 }));
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
