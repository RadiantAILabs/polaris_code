//! Dialog helpers shared by connection setup and the session account commands.

use polaris_tui::dialogs::{Dialog, DialogAnswer, DialogChoice, DialogContent, DialogHost};

/// Present a choice list and return the chosen index, or `None` when dismissed.
pub(crate) async fn choose(
    dialogs: &DialogHost,
    title: &str,
    body: &str,
    choices: Vec<DialogChoice>,
) -> Option<usize> {
    match dialogs
        .present(Dialog::new(title, body, DialogContent::Choices(choices)))
        .await
    {
        Some(DialogAnswer::Choice(index)) => Some(index),
        _ => None,
    }
}

/// Open the `ChatGPT` usage settings, showing the address when no browser opens.
pub(crate) async fn open_usage(dialogs: &DialogHost) {
    if super::browser::open(crate::chatgpt::USAGE_URL)
        .await
        .is_err()
    {
        let _ = choose(
            dialogs,
            "Open ChatGPT usage settings",
            &format!(
                "Couldn’t confirm the browser opened.\nOpen this address in your browser:\n{}",
                crate::chatgpt::USAGE_URL
            ),
            vec![],
        )
        .await;
    }
}

/// What to tell the user after signing out of a `ChatGPT` account.
pub(crate) fn sign_out_notice(label: &str, confirmed: bool) -> String {
    if confirmed {
        format!("Signed out of {label}.")
    } else {
        format!(
            "Signed out of {label} on this device. Remote sign-out could not be confirmed; to \
             disconnect Polaris Code, open ChatGPT Settings → Security and login → Sign in with \
             ChatGPT."
        )
    }
}
