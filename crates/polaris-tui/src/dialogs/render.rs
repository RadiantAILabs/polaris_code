//! Dialog layout and rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::{Active, DialogChoice, DialogContent, DialogDetail, safe};
use crate::composer::Composer;

const GUTTER: u16 = 3;
const CONTENT_WIDTH: u16 = 104;

pub(super) fn dialog(active: &mut Active, frame: &mut Frame<'_>, area: Rect) {
    active.detail_area = None;
    frame.render_widget(Clear, area);
    if area.is_empty() {
        return;
    }
    let gutter = if area.width >= 44 { GUTTER } else { 0 };
    let width = area.width.saturating_sub(gutter * 2).min(CONTENT_WIDTH);
    let mut content = Rect::new(
        area.x + (area.width - width) / 2,
        area.y,
        width,
        area.height,
    );
    let footer = (content.height >= 6).then(|| {
        content.height -= 1;
        Rect::new(content.x, content.y + content.height, width, 1)
    });
    if content.height >= 20 {
        take(&mut content, 2);
    }
    let controls = Controls::measure(&active.dialog.content, width, content.height);
    let required = controls.minimum() as usize
        + rows(
            &Paragraph::new(safe(&active.dialog.title)).wrap(Wrap { trim: false }),
            width,
        ) as usize
        + if active.dialog.body.is_empty() { 1 } else { 3 }
        + if active.dialog.detail.is_some() { 3 } else { 0 };
    let header = active.dialog.header.iter().find_map(|text| {
        let mut text = text.clone();
        for line in &mut text.lines {
            for span in &mut line.spans {
                span.content = safe(&span.content).into();
            }
        }
        (!text.lines.is_empty()
            && text.width() <= width as usize
            && text.height().saturating_add(2).saturating_add(required) <= content.height as usize)
            .then_some(text)
    });
    if let Some(header) = header {
        let height = header.height() as u16;
        frame.render_widget(Paragraph::new(header), take(&mut content, height));
        take(&mut content, 2);
    }
    let title = Paragraph::new(safe(&active.dialog.title))
        .wrap(Wrap { trim: false })
        .style(Style::default().add_modifier(Modifier::BOLD));
    let title_height = rows(&title, width).min(content.height.saturating_sub(2).max(1));
    frame.render_widget(title, take(&mut content, title_height));
    if content.height > 2 {
        take(&mut content, 1);
    }

    let minimum = controls.minimum().min(content.height);
    let body = Paragraph::new(safe(&active.dialog.body)).wrap(Wrap { trim: false });
    let body_rows = if active.dialog.body.is_empty() {
        0
    } else {
        rows(&body, width)
    };
    let height = body_rows.min(content.height.saturating_sub(minimum.saturating_add(1)));
    active.body_overflows = body_rows > height;
    active.body_scroll = active.body_scroll.min(body_rows.saturating_sub(height));
    frame.render_widget(
        body.scroll((active.body_scroll, 0)),
        take(&mut content, height),
    );
    if height > 0 && content.height > minimum {
        take(&mut content, 1);
    }

    let has_choices = matches!(&controls, Controls::Choices(choices) if !choices.is_empty());
    let has_input = matches!(&controls, Controls::Input { .. });
    match controls {
        Controls::Choices(choices) => {
            let spare = active.dialog.detail.is_none();
            render_choices(frame, &mut content, choices, spare, &mut active.list);
        }
        Controls::Input {
            height,
            secret,
            error,
        } => {
            let field = take(&mut content, height);
            render_input(&active.composer, frame, field, secret, error);
        }
    }
    if let Some(detail) = &active.dialog.detail {
        let hint = if active.body_overflows {
            "Shift+PgUp/PgDn to scroll"
        } else {
            "PgUp/PgDn to scroll"
        };
        active.detail_area = render_detail(
            frame,
            area,
            content,
            detail,
            hint,
            &mut active.detail_scroll,
        );
    }
    if let Some(footer) = footer {
        let hints: &[(&str, &str)] = if active.body_overflows {
            &[("PgUp/PgDn", " to scroll"), ("Esc", " to go back")]
        } else if has_choices {
            &[
                ("↑↓", " to choose"),
                ("Enter", " to select"),
                ("Esc", " to go back"),
            ]
        } else if has_input {
            &[("Enter", " to continue"), ("Esc", " to go back")]
        } else {
            &[("Esc", " to go back")]
        };
        let hint_width = hints
            .iter()
            .map(|(key, action)| {
                unicode_width::UnicodeWidthStr::width(*key)
                    + unicode_width::UnicodeWidthStr::width(*action)
            })
            .sum::<usize>()
            + hints.len().saturating_sub(1) * 3;
        let hints = if hint_width <= width as usize {
            hints
        } else {
            &[("Enter", ""), ("Esc", "")]
        };
        let mut spans = Vec::new();
        for (index, (key, action)) in hints.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" · "));
            }
            spans.push(Span::styled(*key, Style::default().fg(Color::White)));
            spans.push(Span::raw(*action));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray)),
            footer,
        );
    }
}

/// The controls of one frame, wrapped once to the content width.
enum Controls<'a> {
    Choices(Vec<ChoiceRows>),
    Input {
        height: u16,
        secret: bool,
        error: Option<&'a str>,
    },
}

impl<'a> Controls<'a> {
    fn measure(content: &'a DialogContent, width: u16, limit: u16) -> Self {
        match content {
            DialogContent::Choices(choices) => Self::Choices(
                choices
                    .iter()
                    .enumerate()
                    .map(|(index, choice)| ChoiceRows::wrap(index, choice, width, limit))
                    .collect(),
            ),
            DialogContent::Input { secret, error, .. } => Self::Input {
                secret: *secret,
                error: error.as_deref(),
                height: 3 + error.as_ref().map_or(0, |error| {
                    1 + rows(
                        &Paragraph::new(safe(error)).wrap(Wrap { trim: false }),
                        width,
                    )
                    .min(3)
                }),
            },
        }
    }

    /// Rows the controls need with choice descriptions hidden.
    fn minimum(&self) -> u16 {
        match self {
            Self::Choices(choices) => choices
                .iter()
                .map(|choice| choice.label.len())
                .sum::<usize>()
                .min(u16::MAX as usize) as u16,
            Self::Input { height, .. } => *height,
        }
    }
}

struct ChoiceRows {
    label: Vec<Line<'static>>,
    description: Vec<Line<'static>>,
}

impl ChoiceRows {
    fn wrap(index: usize, choice: &DialogChoice, width: u16, limit: u16) -> Self {
        let width = width.saturating_sub(2).max(1); // selection marker
        let prefix = format!("{}. ", index + 1);
        let indent = " ".repeat(prefix.len());
        let text_width = width.saturating_sub(prefix.len() as u16).max(1);
        let label = wrap_line(safe(&choice.label), text_width, limit)
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                let lead = if row == 0 { &prefix } else { &indent };
                let mut spans = vec![lead.clone().into()];
                spans.extend(line.spans);
                Line::from(spans)
            })
            .collect();
        let description = if choice.description.is_empty() {
            Vec::new()
        } else {
            let dim = Style::default().fg(Color::DarkGray);
            wrap_line(safe(&choice.description), text_width, limit)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![indent.clone().into()];
                    spans.extend(line.spans);
                    for span in &mut spans {
                        span.style = span.style.patch(dim);
                    }
                    Line::from(spans).style(dim)
                })
                .collect()
        };
        Self { label, description }
    }

    /// The rows to draw. Only label rows are highlighted when selected, so
    /// descriptions keep their dimmed style.
    fn into_lines(self, details: bool, spaced: bool, selected: bool) -> Vec<Line<'static>> {
        let mut lines = self.label;
        if selected {
            for span in lines.iter_mut().flat_map(|line| line.spans.iter_mut()) {
                span.style = span.style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
            }
        }
        if details {
            lines.extend(self.description);
        }
        if spaced {
            lines.push(Line::default());
        }
        lines
    }
}

/// Draw the choice list into `content`. Descriptions are hidden first when
/// choices do not fit; with `spare` rows and no detail below, choices are spaced.
fn render_choices(
    frame: &mut Frame<'_>,
    content: &mut Rect,
    choices: Vec<ChoiceRows>,
    spare: bool,
    list: &mut ListState,
) {
    let available = content.height as usize;
    let detailed: usize = choices
        .iter()
        .map(|choice| choice.label.len() + choice.description.len())
        .sum();
    let details = detailed <= available;
    let spaced = details && spare && detailed + choices.len() <= available;
    let selected = list.selected();
    let mut height = 0;
    let items = choices
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let lines = choice.into_lines(details, spaced, selected == Some(index));
            height += lines.len();
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    frame.render_stateful_widget(
        List::new(items).highlight_symbol("› "),
        take(content, height.min(available) as u16),
        list,
    );
}

fn take(area: &mut Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    let result = Rect { height, ..*area };
    area.y += height;
    area.height -= height;
    result
}

fn rows(paragraph: &Paragraph<'_>, width: u16) -> u16 {
    paragraph.line_count(width.max(1)).min(u16::MAX as usize) as u16
}

fn wrap_line(text: String, width: u16, limit: u16) -> Vec<Line<'static>> {
    let line = Line::from(text);
    let height = rows(
        &Paragraph::new(line.clone()).wrap(Wrap { trim: false }),
        width,
    )
    .min(limit);
    crate::widgets::wrapped_rows(&line, width, height)
}

fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    mut content: Rect,
    detail: &DialogDetail,
    hint: &str,
    scroll: &mut u16,
) -> Option<Rect> {
    if content.height < 3 {
        return None;
    }
    take(&mut content, 1);
    // Use the full terminal width without indentation for manual copying.
    let mut detail_area = Rect {
        x: area.x,
        width: area.width,
        ..content
    };
    let paragraph = Paragraph::new(safe(&detail.value))
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::DarkGray));
    let count = rows(&paragraph, detail_area.width);
    let label = if count > detail_area.height.saturating_sub(1) {
        format!("{} · {hint}", safe(&detail.label))
    } else {
        safe(&detail.label)
    };
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(Color::DarkGray)),
        take(&mut content, 1),
    );
    take(&mut detail_area, 1);
    *scroll = (*scroll).min(count.saturating_sub(detail_area.height));
    frame.render_widget(paragraph.scroll((*scroll, 0)), detail_area);
    Some(Rect {
        height: detail_area.height.min(count.saturating_sub(*scroll)),
        ..detail_area
    })
}

fn render_input(
    composer: &Composer,
    frame: &mut Frame<'_>,
    mut area: Rect,
    secret: bool,
    error: Option<&str>,
) {
    let text = if secret {
        "•".repeat(composer.text().chars().count())
    } else {
        composer.text().to_owned()
    };
    let field = take(&mut area, 3);
    let input = if field.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(if secret { " Hidden " } else { "" })
            .border_style(Style::default().fg(if error.is_some() {
                Color::Red
            } else {
                Color::DarkGray
            }));
        let input = block.inner(field);
        frame.render_widget(block, field);
        input
    } else {
        field
    };
    let prefix = &composer.text()[..composer.cursor()];
    let caret = if secret {
        prefix.chars().count()
    } else {
        unicode_width::UnicodeWidthStr::width(prefix)
    };
    let scroll = caret.saturating_sub(input.width.saturating_sub(1) as usize);
    frame.render_widget(
        Paragraph::new(text).scroll((0, scroll.min(u16::MAX as usize) as u16)),
        input,
    );
    if !input.is_empty() {
        frame.set_cursor_position((input.x + (caret - scroll) as u16, input.y));
    }
    if let Some(error) = error {
        if area.height > 1 {
            take(&mut area, 1);
        }
        frame.render_widget(
            Paragraph::new(safe(error))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Red)),
            area,
        );
    }
}
