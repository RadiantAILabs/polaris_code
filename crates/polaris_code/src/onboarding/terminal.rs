use polaris_tui::TuiEvent;
use polaris_tui::dialogs::DialogHost;
use polaris_tui::ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{ExecutableCommand, clipboard::CopyToClipboard, event, execute, terminal},
};
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

/// Run setup dialogs in a temporary terminal until the workflow completes.
pub(super) async fn run<T>(host: &DialogHost, work: impl Future<Output = T>) -> io::Result<T> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(io::Error::other(
            "interactive setup needs a terminal; use `polaris login <provider> --with-api-key` for scripted API-key setup",
        ));
    }
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = execute!(
                io::stderr(),
                event::DisableBracketedPaste,
                event::DisableMouseCapture,
                terminal::LeaveAlternateScreen
            );
            let _ = terminal::disable_raw_mode();
        }
    }
    terminal::enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        io::stderr(),
        terminal::EnterAlternateScreen,
        event::EnableBracketedPaste,
        event::DisableMouseCapture
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
    let mut work = std::pin::pin!(work);
    let mut tick = tokio::time::interval(Duration::from_millis(30));
    loop {
        tokio::select! {
            result = &mut work => return Ok(result),
            _ = tick.tick() => {
                terminal.backend_mut().execute(terminal::BeginSynchronizedUpdate)?;
                let drawn = terminal.draw(|frame| { host.render(frame,frame.area()); }).map(|_| ());
                let ended = terminal.backend_mut().execute(terminal::EndSynchronizedUpdate).map(|_| ());
                drawn.and(ended)?;
                while event::poll(Duration::ZERO)? {
                    let input = match event::read()? {
                        event::Event::Key(key) => TuiEvent::Key(key),
                        event::Event::Paste(text) => TuiEvent::Paste(text),
                        event::Event::Mouse(mouse) => TuiEvent::Mouse(mouse),
                        event::Event::Resize(cols,rows) => TuiEvent::Resize { cols,rows },
                        _ => continue,
                    };
                    host.handle_event(&input);
                }
            }
        }
    }
}

/// Ask the user's terminal to copy the original text, including over SSH.
/// OSC 52 has no success acknowledgement; callers must describe it as a request.
pub(super) fn copy_to_clipboard(text: &str) -> io::Result<()> {
    if !io::stderr().is_terminal() {
        return Err(io::Error::other("clipboard copy needs a terminal"));
    }
    write_clipboard_request(&mut io::stderr().lock(), text)
}

fn write_clipboard_request(output: &mut impl Write, text: &str) -> io::Result<()> {
    output
        .execute(CopyToClipboard::to_clipboard_from(text))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn terminal_clipboard_preserves_a_long_authorization_url_exactly() {
        let url = format!(
            "https://example.com/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A44107%2Fauth%2Fcallback&state={}&scope=openid+profile",
            "a".repeat(500)
        );
        let mut output = Vec::new();
        write_clipboard_request(&mut output, &url).unwrap();
        let sequence = String::from_utf8(output).unwrap();
        let payload = sequence
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(decoded, url.as_bytes());
        assert!(!payload.chars().any(char::is_whitespace));
    }
}
