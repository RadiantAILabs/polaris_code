//! Open sign-in links and copy them to the clipboard without logging their contents.

use std::io::{self, Write};
use std::process::{Command, Stdio};

pub(super) async fn open(url: &str) -> io::Result<()> {
    let command = browser_command(std::env::var_os("BROWSER").as_deref(), url);
    tokio::task::spawn_blocking(move || launch(command))
        .await
        .map_err(|_| io::Error::other("browser launcher stopped"))?
}

fn browser_command(browser: Option<&std::ffi::OsStr>, url: &str) -> Command {
    // Remote IDEs supply a helper that forwards links to the local browser.
    if let Some(browser) = browser.filter(|value| !value.is_empty()) {
        let mut command = Command::new(browser);
        command.arg(url);
        return command;
    }
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

fn launch(mut command: Command) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("browser launcher failed"))
            };
        }
        if std::time::Instant::now() >= deadline {
            // Some launchers stay open with the browser. Keep it running, but
            // offer the URL because we cannot confirm that it opened.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Err(io::Error::other("browser launch could not be confirmed"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub(super) enum CopyOutcome {
    Copied,
    TerminalRequested,
}

pub(super) async fn copy(url: &str) -> io::Result<CopyOutcome> {
    let text = url.to_string();
    let copied = tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        let command = Command::new("pbcopy");
        #[cfg(target_os = "windows")]
        let command = Command::new("clip");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let command = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Command::new("wl-copy")
        } else {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard"]);
            command
        };
        copy_with_command(&text, command)
    })
    .await
    .map_err(|_| io::Error::other("clipboard operation stopped"))?;
    copied.map(|()| CopyOutcome::Copied)
}

fn copy_with_command(text: &str, mut command: Command) -> io::Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("clipboard unavailable"))
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("clipboard timed out"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn clipboard_helper_receives_the_original_url_without_wrapping() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("clipboard");
        let url = format!(
            "https://example.com/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A44107%2Fauth%2Fcallback&state={}&scope=openid+profile",
            "a".repeat(500)
        );
        let mut command = Command::new("sh");
        command
            .args(["-c", r#"cat > "$1""#, "polaris-copy"])
            .arg(&destination);
        copy_with_command(&url, command).unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), url.as_bytes());
        assert!(copy_with_command(&url, Command::new("false")).is_err());
        assert!(copy_with_command(&url, Command::new("/nonexistent/polaris-copy")).is_err());
    }

    #[test]
    fn configured_browser_helper_receives_the_original_url_as_one_argument() {
        let url = "https://example.com/authorize?state=a&redirect_uri=http%3A%2F%2F127.0.0.1%3A39303%2Fauth%2Fcallback";
        let command = browser_command(Some(std::ffi::OsStr::new("/remote IDE/browser.sh")), url);
        assert_eq!(command.get_program(), "/remote IDE/browser.sh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new(url)]
        );
    }

    #[test]
    fn launcher_must_exit_successfully() {
        assert!(launch(Command::new("true")).is_ok());
        assert!(launch(Command::new("false")).is_err());
        assert!(launch(Command::new("/nonexistent/polaris-browser")).is_err());
    }
}
