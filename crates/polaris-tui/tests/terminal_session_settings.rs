//! Host-advertised setting picker behavior.

use polaris_tui::commands::{
    CommandSpec, CompletionPopup, active_command_argument_token, advertise_commands,
    parse_advertisement,
};

fn settings_commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("model", "Pick model")
            .with_usage("[provider/model]")
            .with_choices(["fake/one", "fake/two"]),
        CommandSpec::new("permission", "Pick mode")
            .with_usage("[mode]")
            .with_choices(["confirm", "auto-edit", "plan"]),
    ]
}

#[test]
fn advertised_choices_survive_the_wire_protocol() {
    let specs = settings_commands();
    assert_eq!(
        parse_advertisement(&advertise_commands(&specs)),
        Some(specs)
    );
}

#[test]
fn a_non_empty_setting_draft_opens_and_accepts_its_picker() {
    let draft = "/permission au";
    let (token, choices) = active_command_argument_token(draft, draft.len(), &settings_commands())
        .expect("permission arguments have a picker");
    let mut popup = CompletionPopup::new(token);
    popup.filter_choices(&choices);
    assert_eq!(popup.selected_item().unwrap().insert, "auto-edit");
    let edit = popup.acceptance().unwrap();
    let mut changed = draft.to_string();
    changed.replace_range(edit.from..edit.to, &edit.insert);
    assert_eq!(changed, "/permission auto-edit");
}

#[test]
fn free_form_commands_do_not_gain_a_picker() {
    let commands = vec![CommandSpec::new("help", "Help")];
    assert!(active_command_argument_token("/help topic", 11, &commands).is_none());
}
