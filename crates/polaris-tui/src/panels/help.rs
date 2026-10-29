//! Static help / keymap panel. Smallest possible `TuiPanel` example.

use crate::panel::TuiPanel;
use crate::registry::TuiRegistry;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// Renders the global keymap and a short blurb. No state, no events.
#[derive(Debug, Default)]
pub struct HelpPanel;

impl TuiPanel for HelpPanel {
    fn id(&self) -> &str {
        "help"
    }

    fn title(&self) -> &str {
        "Help"
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let lines = vec![
            Line::from("polaris-tui — composable terminal UI for Polaris"),
            Line::from(""),
            Line::from("Global keys"),
            Line::from("  Tab / Shift-Tab     cycle panel focus"),
            Line::from("  Alt-1 … Alt-9       jump to panel N"),
            Line::from("  Ctrl-L              toggle Single ↔ Tiled layout"),
            Line::from("  Ctrl-C / Ctrl-Q     quit"),
            Line::from(""),
            Line::from("Built-in panels"),
            Line::from("  Conversation        Chat transcript wired to UserIO."),
            Line::from("    Ctrl-O            expand the per-turn node timeline"),
            Line::from("    Opt+←/→, Alt-b/f  jump one word"),
            Line::from("    Home/End, ^A/^E   jump to line start / end"),
            Line::from("    Opt+Bksp, ^W      delete word back  (Opt+Del / Alt-d: forward)"),
            Line::from("    Ctrl-U / Ctrl-K   kill to line start / end"),
            Line::from("  Logs                Tail of tracing events."),
            Line::from("  Sessions            Live list of active sessions."),
            Line::from(""),
            Line::from(Span::styled(
                "Contribute your own panel by implementing TuiPanel and",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "calling server.api::<TuiRegistry>().add_panel(…) in build().",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::NONE))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }
}

/// Plugin that contributes a [`HelpPanel`] to the [`TuiRegistry`].
#[derive(Default, Debug)]
pub struct HelpPanelPlugin;

impl Plugin for HelpPanelPlugin {
    const ID: &'static str = "polaris::tui::help_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added before HelpPanelPlugin")
            .add_panel(HelpPanel);
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}
