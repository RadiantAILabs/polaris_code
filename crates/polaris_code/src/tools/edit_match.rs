//! Deciding where an edit lands.
//!
//! [`plan_edit`] is the one place that decides what `edit_file` does to a
//! file's text.
//!
//! An edit is an exact replacement: `old_string` must occur in the file, once
//! unless `replace_all` is set, and only that text changes. One mistake is
//! forgiven, because it has one possible meaning. A request that ends its
//! `old_string` with a newline cannot match the last line of a file that has
//! no final newline, yet a line of text can only be asked for with its newline
//! and the model has no way to see whether the file has one. When the text
//! without that newline occurs exactly once in the file, and that one place is
//! the very end of the file, nothing else could have been meant, and the edit
//! is made there; the file still ends the way it did. Every other request that
//! does not match is refused, and the refusal shows the text the file really
//! holds near the place the request was aiming at, so the next attempt can
//! copy it.

use std::fmt::Write as _;

/// How many match positions an ambiguity refusal lists.
const MAX_LISTED_MATCHES: usize = 8;

/// File lines considered when looking for the text nearest a failed request.
const MAX_SCANNED_LINES: usize = 20_000;

/// File lines a refusal shows.
const MAX_SHOWN_LINES: usize = 12;

/// Characters of one line a refusal shows.
const MAX_SHOWN_CHARS: usize = 200;

/// Characters of a line compared when ranking candidates.
const MAX_COMPARED_CHARS: usize = 160;

/// Lowest similarity of a file line to the first line of the request for the
/// file to be said to hold something near it.
const MIN_NEARBY_SIMILARITY: f32 = 0.6;

/// File lines examined closely as the start of the nearest text.
const MAX_CANDIDATES: usize = 20;

/// An edit that can be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditPlan {
    /// The whole file once the edit is made.
    pub updated: String,
    /// How many places changed.
    pub replacements: usize,
    /// Set when the request was matched with the one allowance described in
    /// the module documentation rather than exactly as sent.
    pub adjusted: bool,
}

/// Why an edit is not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditRefusal {
    /// `old_string` is empty and the file is not.
    EmptyOldString,
    /// `old_string` does not occur in the file.
    NotFound(Option<Nearest>),
    /// The file already holds the text the edit would put in its place.
    NoChange,
    /// `old_string` occurs more than once and `replace_all` is not set.
    NotUnique {
        /// How many places match.
        count: usize,
        /// The 1-based line each of the first matches starts on.
        lines: Vec<usize>,
    },
}

/// The text in a file nearest to the one a failed request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Nearest {
    /// 1-based line the text starts on.
    first_line: usize,
    /// The lines, each prefixed with its line number.
    shown: Vec<String>,
    /// Where the request first differs from the file, when it does.
    difference: Option<String>,
}

impl EditRefusal {
    /// The refusal as the model reads it: what is wrong and what to do next.
    pub(crate) fn message(&self, path: &str) -> String {
        match self {
            Self::EmptyOldString => format!(
                "old_string is empty, so it does not say where in {path} to edit; give the \
                 text to replace (use create_file to add a new file)"
            ),
            Self::NoChange => format!(
                "{path} already ends with this text and has no final newline, so this edit \
                 would change nothing. Nothing was changed."
            ),
            Self::NotFound(None) => format!(
                "old_string not found in {path}, and nothing in the file is close to its first \
                 line. Nothing was changed. Find the text with search or read_file, then copy \
                 it exactly."
            ),
            Self::NotFound(Some(nearest)) => {
                let mut text = format!(
                    "old_string not found in {path}. Nothing was changed. The closest text \
                     in the file starts at line {}:\n{}",
                    nearest.first_line,
                    nearest.shown.join("\n")
                );
                if let Some(difference) = &nearest.difference {
                    text.push('\n');
                    text.push_str(difference);
                }
                let _ = write!(
                    text,
                    "\nCopy the text exactly as the file has it (read_file with offset {} shows \
                     it) and send the edit again.",
                    nearest.first_line
                );
                text
            }
            Self::NotUnique { count, lines } => {
                let mut places = lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                if *count > lines.len() {
                    let _ = write!(places, " and {} more", count - lines.len());
                }
                format!(
                    "old_string is not unique in {path} ({count} matches, starting at lines \
                     {places}). Nothing was changed. Add the lines around the one you mean so \
                     that only one place matches, or set replace_all = true to change every \
                     match."
                )
            }
        }
    }
}

/// The note a successful edit carries when it was matched with the allowance
/// in the module documentation.
pub(crate) fn adjustment_note(path: &str) -> String {
    format!(
        "Note: {path} has no newline after this text, so the newline that ended old_string and \
         new_string was left out. The file still ends without a final newline."
    )
}

/// Decide what replacing `old` with `new` in `original` does.
///
/// With `replace_all` unset the text must match exactly once. See the module
/// documentation for the single request that is matched although it is not
/// exact.
pub(crate) fn plan_edit(
    original: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<EditPlan, EditRefusal> {
    if old.is_empty() {
        // An empty file has one place to put text; in any other file an empty
        // `old_string` points at every position and so at none.
        return if original.is_empty() {
            Ok(EditPlan {
                updated: new.to_string(),
                replacements: 1,
                adjusted: false,
            })
        } else {
            Err(EditRefusal::EmptyOldString)
        };
    }
    let matches = original.matches(old).count();
    match matches {
        0 => without_final_newline(original, old, new)
            .unwrap_or_else(|| Err(EditRefusal::NotFound(nearest_text(original, old)))),
        1 => Ok(EditPlan {
            updated: original.replacen(old, new, 1),
            replacements: 1,
            adjusted: false,
        }),
        _ if replace_all => Ok(EditPlan {
            updated: original.replace(old, new),
            replacements: matches,
            adjusted: false,
        }),
        _ => Err(EditRefusal::NotUnique {
            count: matches,
            lines: original
                .match_indices(old)
                .take(MAX_LISTED_MATCHES)
                .map(|(at, _)| line_of(original, at))
                .collect(),
        }),
    }
}

/// The edit for a request whose `old` ends with newlines the file lacks, when
/// the text before them occurs once and is the end of the file.
///
/// The same number of newlines is left off the end of `new`, so the file ends
/// as it did. `None` means the request is not of this kind; a refusal means it
/// is, and would change nothing.
fn without_final_newline(
    original: &str,
    old: &str,
    new: &str,
) -> Option<Result<EditPlan, EditRefusal>> {
    let body = old.trim_end_matches('\n');
    let dropped = old.len() - body.len();
    if dropped == 0 || body.is_empty() || !original.ends_with(body) {
        return None;
    }
    let start = original.len() - body.len();
    // The first occurrence must be the final one: any earlier occurrence, even
    // one that overlaps it, would be another place the request could mean.
    if original.find(body) != Some(start) {
        return None;
    }
    let trailing = new.len() - new.trim_end_matches('\n').len();
    let replacement = &new[..new.len() - trailing.min(dropped)];
    if replacement == body {
        return Some(Err(EditRefusal::NoChange));
    }
    let mut updated = String::with_capacity(start + replacement.len());
    updated.push_str(&original[..start]);
    updated.push_str(replacement);
    Some(Ok(EditPlan {
        updated,
        replacements: 1,
        adjusted: true,
    }))
}

/// The 1-based line the byte offset `at` falls on.
fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// At most `limit` characters of `text`.
fn head(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(end, _)| &text[..end])
}

/// `text` as a quoted literal, so spaces, tabs and escapes at its ends show.
fn quoted(text: &str) -> String {
    let shown = head(text, MAX_SHOWN_CHARS);
    if shown.len() < text.len() {
        format!("{shown:?}…")
    } else {
        format!("{shown:?}")
    }
}

/// How alike two lines are, from 0 to 1.
fn similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    similar::TextDiff::from_chars(head(a, MAX_COMPARED_CHARS), head(b, MAX_COMPARED_CHARS)).ratio()
}

/// How well the lines of `file` from `start` on line up with `wanted`: the sum
/// of how alike each pair of lines is.
fn window_score(file: &[&str], wanted: &[&str], start: usize) -> f32 {
    wanted
        .iter()
        .enumerate()
        .filter_map(|(offset, want)| file.get(start + offset).map(|line| similarity(line, want)))
        .sum()
}

/// The text in `original` nearest to `old`, when the file holds any.
///
/// The first non-blank line of `old` is looked for among the file's lines; of
/// the lines that resemble it, the one whose following lines resemble the rest
/// of `old` best starts the answer.
fn nearest_text(original: &str, old: &str) -> Option<Nearest> {
    let wanted: Vec<&str> = old.lines().collect();
    let anchor = wanted.iter().position(|line| !line.trim().is_empty())?;
    let file: Vec<&str> = original.lines().take(MAX_SCANNED_LINES).collect();

    let mut candidates: Vec<(f32, usize)> = file
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let score = similarity(line.trim(), wanted[anchor].trim());
            (score >= MIN_NEARBY_SIMILARITY).then_some((score, index))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    candidates.truncate(MAX_CANDIDATES);

    let mut best: Option<(f32, usize)> = None;
    for (_, index) in candidates {
        let start = index.saturating_sub(anchor);
        let score = window_score(&file, &wanted, start);
        if best.is_none_or(|(top, _)| score > top) {
            best = Some((score, start));
        }
    }
    let (_, start) = best?;

    let end = (start + wanted.len().min(MAX_SHOWN_LINES)).min(file.len());
    let shown = file[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{}\t{}", start + offset + 1, head(line, MAX_SHOWN_CHARS)))
        .collect();
    Some(Nearest {
        first_line: start + 1,
        shown,
        difference: first_difference(&wanted, &file, start),
    })
}

/// A sentence naming the first line of `wanted` that is not the file's line at
/// the same place, written so that whitespace shows.
fn first_difference(wanted: &[&str], file: &[&str], start: usize) -> Option<String> {
    for (offset, want) in wanted.iter().enumerate().take(MAX_SHOWN_LINES) {
        let Some(have) = file.get(start + offset) else {
            return Some(format!(
                "The file ends at line {}, but old_string has more lines after that.",
                file.len()
            ));
        };
        if have == want {
            continue;
        }
        let spaces = if have.trim() == want.trim() {
            " (only the spaces or tabs at its start or end differ)"
        } else {
            ""
        };
        return Some(format!(
            "The first difference: line {} of old_string is {} but line {} of the file is {}{spaces}.",
            offset + 1,
            quoted(want),
            start + offset + 1,
            quoted(have),
        ));
    }
    if wanted.len() > MAX_SHOWN_LINES {
        return None;
    }
    Some(
        "The lines are the same as old_string's, so the difference is in the line endings or \
         at the end of the file."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(original: &str, old: &str, new: &str, all: bool) -> String {
        plan_edit(original, old, new, all).unwrap().updated
    }

    #[test]
    fn an_exact_unique_match_is_replaced_in_that_place_only() {
        let plan = plan_edit("a\nb\nc\n", "b", "B", false).unwrap();
        assert_eq!(plan.updated, "a\nB\nc\n");
        assert_eq!(plan.replacements, 1);
        assert!(!plan.adjusted);
    }

    #[test]
    fn several_exact_matches_need_replace_all() {
        let refusal = plan_edit("x\ny\nx\n", "x", "z", false).unwrap_err();
        assert_eq!(
            refusal,
            EditRefusal::NotUnique {
                count: 2,
                lines: vec![1, 3]
            }
        );
        let message = refusal.message("f.py");
        assert!(message.contains("lines 1, 3"), "{message}");
        assert!(message.contains("replace_all = true"), "{message}");

        let plan = plan_edit("x\ny\nx\n", "x", "z", true).unwrap();
        assert_eq!(plan.updated, "z\ny\nz\n");
        assert_eq!(plan.replacements, 2);
    }

    #[test]
    fn a_long_list_of_matches_is_cut_and_counted() {
        let original = "x\n".repeat(30);
        let EditRefusal::NotUnique { count, lines } =
            plan_edit(&original, "x", "y", false).unwrap_err()
        else {
            panic!("expected an ambiguity refusal");
        };
        assert_eq!((count, lines.len()), (30, MAX_LISTED_MATCHES));
        let message = EditRefusal::NotUnique { count, lines }.message("f.py");
        assert!(message.contains("and 22 more"), "{message}");
    }

    #[test]
    fn an_empty_old_string_names_no_place() {
        assert_eq!(
            plan_edit("abc", "", "x", true).unwrap_err(),
            EditRefusal::EmptyOldString
        );
        assert_eq!(
            plan_edit("abc", "", "x", false).unwrap_err(),
            EditRefusal::EmptyOldString
        );
        assert_eq!(applied("", "", "x", false), "x");
    }

    #[test]
    fn a_final_newline_the_file_lacks_is_left_out_of_both_strings() {
        let original = "def add(a, b):\n    raise NotImplementedError";
        let plan = plan_edit(
            original,
            "    raise NotImplementedError\n",
            "    return a + b\n",
            false,
        )
        .unwrap();
        assert_eq!(plan.updated, "def add(a, b):\n    return a + b");
        assert!(plan.adjusted);
    }

    #[test]
    fn only_as_many_newlines_are_left_off_new_as_were_left_off_old() {
        let original = "a\nb";
        assert_eq!(applied(original, "b\n", "c\n\n", false), "a\nc\n");
        assert_eq!(applied(original, "b\n\n", "c\n", false), "a\nc");
        assert_eq!(applied(original, "b\n", "c", false), "a\nc");
    }

    #[test]
    fn a_forgiven_request_that_would_change_nothing_is_refused() {
        assert_eq!(
            plan_edit("a\nb", "b\n", "b", false).unwrap_err(),
            EditRefusal::NoChange
        );
    }

    #[test]
    fn a_newline_is_not_forgiven_where_the_file_has_one() {
        // The text is there, but followed by a newline the request did not
        // ask for, and by one fewer than it did: not the end of a file that
        // lacks a final newline.
        assert!(plan_edit("a\nb\n", "b\n\n", "c\n", false).is_err());
    }

    #[test]
    fn a_final_newline_is_not_forgiven_when_the_text_occurs_elsewhere() {
        // "b" at the end is one of two places the request could mean.
        let original = "b = 1\nb";
        assert!(plan_edit(original, "b\n", "c\n", false).is_err());
        // An overlapping earlier occurrence counts too.
        assert!(plan_edit("aaa", "aa\n", "b\n", false).is_err());
    }

    #[test]
    fn a_final_newline_is_not_forgiven_away_from_the_end_of_the_file() {
        // "b" is followed by more text, so no newline was lost.
        assert!(plan_edit("a\nb c\nd", "b\n", "x\n", false).is_err());
    }

    #[test]
    fn text_that_is_only_newlines_never_matches_the_end() {
        assert!(plan_edit("a", "\n", "b", false).is_err());
    }

    #[test]
    fn the_nearest_text_is_shown_with_its_line_and_the_first_difference() {
        let original = "def f(x):\n    if x:\n        return 1\n    return 0\n";
        let refusal = plan_edit(original, "    if x:\n      return 1\n", "", false).unwrap_err();
        let message = refusal.message("f.py");
        assert!(message.contains("starts at line 2"), "{message}");
        assert!(
            message.contains("2\t    if x:\n3\t        return 1"),
            "{message}"
        );
        assert!(
            message.contains(
                r#"line 2 of old_string is "      return 1" but line 3 of the file is "        return 1""#
            ),
            "{message}"
        );
        assert!(message.contains("Nothing was changed"), "{message}");
    }

    #[test]
    fn the_candidate_whose_following_lines_fit_is_chosen() {
        let original = "x = 1\ny = 2\n\nx = 1\nz = 3\n";
        let refusal = plan_edit(original, "x = 1\nz = 4\n", "", false).unwrap_err();
        let message = refusal.message("f.py");
        assert!(message.contains("starts at line 4"), "{message}");
    }

    #[test]
    fn nothing_close_is_said_so_without_inventing_a_place() {
        let refusal = plan_edit("alpha\nbeta\n", "completely unrelated", "", false).unwrap_err();
        assert_eq!(refusal, EditRefusal::NotFound(None));
        assert!(
            refusal
                .message("f.py")
                .contains("nothing in the file is close")
        );
    }

    #[test]
    fn identical_lines_with_different_endings_are_named_as_such() {
        let refusal = plan_edit("a\r\nb\r\n", "a\nb\n", "", false).unwrap_err();
        let message = refusal.message("f.py");
        assert!(message.contains("line endings"), "{message}");
    }

    #[test]
    fn what_a_long_line_shows_is_bounded() {
        let long = "x".repeat(5_000);
        let original = format!("{long}\n");
        let refusal = plan_edit(&original, &format!("{long}y\n"), "", false).unwrap_err();
        assert!(refusal.message("f.py").len() < 2_000);
    }
}
