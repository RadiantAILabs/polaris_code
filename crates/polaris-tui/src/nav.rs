//! Navigation key maps — translate raw key events into movement [`NavIntent`]s.
//!
//! Panels that scroll a viewport (or move a cursor) should not hard-code a key table. Instead they pick one or more stock
//! [`KeyMap`]s and call [`KeyMap::resolve`] to turn a [`KeyEvent`] into a
//! [`NavIntent`], leaving the panel to act on the intent rather than the
//! physical key.
//!
//! # Composition
//!
//! Maps layer with [`KeyMap::then`]. Resolution is first-match-wins, so the
//! left-hand map takes priority on any key both bind. This is what lets a
//! typing-heavy panel stay safe: it can layer the chord-only [`KeyMap::vim_paging`]
//! or arrow keys *without* pulling in the letter-based [`KeyMap::vim_motion`]
//! that would otherwise eat `j`/`k` keystrokes meant for text.
//!
//! ```
//! use polaris_tui::nav::{KeyMap, NavIntent};
//! use polaris_tui::ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
//!
//! // Arrow keys, plus vim's Ctrl-D / Ctrl-U paging chords — but not h/j/k/l.
//! let map = KeyMap::arrows().then(KeyMap::vim_paging());
//!
//! let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
//! assert_eq!(map.resolve(down), Some(NavIntent::LineDown));
//!
//! let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
//! assert_eq!(map.resolve(ctrl_d), Some(NavIntent::PageDown));
//!
//! // A plain `j` is left for the panel to treat as text.
//! let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
//! assert_eq!(map.resolve(j), None);
//! ```
//!
//! Before composing two maps a panel can call [`KeyMap::conflicts`] to see
//! which keys overlap (and would therefore resolve to the left map's intent).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A movement request, decoded from a key press by a [`KeyMap`].
///
/// Intentionally a small, presentation-agnostic vocabulary: a panel decides
/// what "a page" or "the top" means for its own content. Marked
/// `#[non_exhaustive]` so cursor intents can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NavIntent {
    /// Move one line / row towards the start.
    LineUp,
    /// Move one line / row towards the end.
    LineDown,
    /// Move one viewport-height towards the start.
    PageUp,
    /// Move one viewport-height towards the end.
    PageDown,
    /// Jump to the very start.
    Top,
    /// Jump to the very end.
    Bottom,
}

/// A physical key: a [`KeyCode`] plus the modifiers held with it.
///
/// `SHIFT` is ignored when the code is a [`KeyCode::Char`], because the char
/// itself already encodes case (`G` vs `g`) and terminals are inconsistent
/// about whether they additionally report `SHIFT`. The same holds for
/// [`KeyCode::BackTab`], which already means Shift-Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The key code.
    pub code: KeyCode,
    /// Modifiers held with the key.
    pub mods: KeyModifiers,
}

impl Key {
    /// Construct a key binding.
    #[must_use]
    pub const fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    fn normalized_mods(code: KeyCode, mods: KeyModifiers) -> KeyModifiers {
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.difference(KeyModifiers::SHIFT)
        } else {
            mods
        }
    }

    /// Whether `event` is this key, under the `SHIFT` rule above.
    pub(crate) fn matches(self, event: KeyEvent) -> bool {
        self.code == event.code
            && Self::normalized_mods(self.code, self.mods)
                == Self::normalized_mods(event.code, event.modifiers)
    }
}

/// An ordered table of [`Key`] → [`NavIntent`] bindings.
///
/// Resolution is first-match-wins, which is also how [`KeyMap::then`]
/// composes: bindings from the left map are tried before the right's.
#[derive(Debug, Clone, Default)]
pub struct KeyMap {
    bindings: Vec<(Key, NavIntent)>,
}

impl KeyMap {
    /// An empty map — resolves nothing until [`bind`](Self::bind) is called.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a binding. Builder-style; later binds for the same key are dead
    /// weight (the first match wins) but harmless.
    #[must_use]
    pub fn bind(mut self, code: KeyCode, mods: KeyModifiers, intent: NavIntent) -> Self {
        self.bindings.push((Key::new(code, mods), intent));
        self
    }

    /// Resolve a key press to an intent, or `None` if this map does not bind
    /// it. The panel should treat `None` as "not a navigation key" and fall
    /// through to its own handling.
    #[must_use]
    pub fn resolve(&self, event: KeyEvent) -> Option<NavIntent> {
        self.bindings
            .iter()
            .find(|(key, _)| key.matches(event))
            .map(|(_, intent)| *intent)
    }

    /// Layer `other` underneath `self`: keys bound by both resolve to
    /// `self`'s intent. Use [`conflicts`](Self::conflicts) first if you want
    /// to know which keys those are.
    #[must_use]
    pub fn then(mut self, other: KeyMap) -> Self {
        self.bindings.extend(other.bindings);
        self
    }

    /// Keys bound by *both* maps. Composing via [`then`](Self::then) would
    /// resolve each of these to the left map's intent; an empty result means
    /// the two maps layer cleanly.
    #[must_use]
    pub fn conflicts(&self, other: &KeyMap) -> Vec<Key> {
        self.bindings
            .iter()
            .filter(|(key, _)| other.bindings.iter().any(|(o, _)| o == key))
            .map(|(key, _)| *key)
            .collect()
    }

    /// Arrow keys, `PageUp`/`PageDown`, and `Home`/`End`. The universally
    /// understood default, and safe to layer on any panel — none of these
    /// keys carry text.
    #[must_use]
    pub fn arrows() -> Self {
        let n = KeyModifiers::NONE;
        Self::new()
            .bind(KeyCode::Up, n, NavIntent::LineUp)
            .bind(KeyCode::Down, n, NavIntent::LineDown)
            .bind(KeyCode::PageUp, n, NavIntent::PageUp)
            .bind(KeyCode::PageDown, n, NavIntent::PageDown)
            .bind(KeyCode::Home, n, NavIntent::Top)
            .bind(KeyCode::End, n, NavIntent::Bottom)
    }

    /// Vim's letter motions: `j`/`k` to step, `g`/`G` for top/bottom. These
    /// collide with text entry, so only layer this onto panels with no input
    /// surface (or behind a mode switch).
    #[must_use]
    pub fn vim_motion() -> Self {
        let n = KeyModifiers::NONE;
        Self::new()
            .bind(KeyCode::Char('k'), n, NavIntent::LineUp)
            .bind(KeyCode::Char('j'), n, NavIntent::LineDown)
            .bind(KeyCode::Char('g'), n, NavIntent::Top)
            .bind(KeyCode::Char('G'), n, NavIntent::Bottom)
    }

    /// Vim's paging chords: `Ctrl-U`/`Ctrl-D` and `Ctrl-B`/`Ctrl-F`. All
    /// Ctrl-modified, so these are safe to layer onto a typing panel —
    /// provided it does not itself bind those chords (e.g. `Ctrl-U` as
    /// clear-line; check with [`conflicts`](Self::conflicts)).
    #[must_use]
    pub fn vim_paging() -> Self {
        let c = KeyModifiers::CONTROL;
        Self::new()
            .bind(KeyCode::Char('u'), c, NavIntent::PageUp)
            .bind(KeyCode::Char('d'), c, NavIntent::PageDown)
            .bind(KeyCode::Char('b'), c, NavIntent::PageUp)
            .bind(KeyCode::Char('f'), c, NavIntent::PageDown)
    }

    /// The full vim style — [`vim_motion`](Self::vim_motion) layered over
    /// [`vim_paging`](Self::vim_paging).
    #[must_use]
    pub fn vim() -> Self {
        Self::vim_motion().then(Self::vim_paging())
    }

    /// Emacs motion chords: `Ctrl-P`/`Ctrl-N` to step, `Ctrl-V`/`Alt-V` to
    /// page, `Alt-<`/`Alt->` for top/bottom. All modified, so typing-safe.
    #[must_use]
    pub fn emacs() -> Self {
        let c = KeyModifiers::CONTROL;
        let a = KeyModifiers::ALT;
        Self::new()
            .bind(KeyCode::Char('p'), c, NavIntent::LineUp)
            .bind(KeyCode::Char('n'), c, NavIntent::LineDown)
            .bind(KeyCode::Char('v'), c, NavIntent::PageDown)
            .bind(KeyCode::Char('v'), a, NavIntent::PageUp)
            .bind(KeyCode::Char('<'), a, NavIntent::Top)
            .bind(KeyCode::Char('>'), a, NavIntent::Bottom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn arrows_resolve_expected_intents() {
        let map = KeyMap::arrows();
        assert_eq!(
            map.resolve(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(NavIntent::LineUp)
        );
        assert_eq!(
            map.resolve(key(KeyCode::End, KeyModifiers::NONE)),
            Some(NavIntent::Bottom)
        );
        assert_eq!(
            map.resolve(key(KeyCode::Char('j'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn shift_is_ignored_for_char_keys() {
        // `G` arrives as Char('G') with or without SHIFT depending on the
        // terminal; both must resolve.
        let map = KeyMap::vim_motion();
        assert_eq!(
            map.resolve(key(KeyCode::Char('G'), KeyModifiers::NONE)),
            Some(NavIntent::Bottom)
        );
        assert_eq!(
            map.resolve(key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(NavIntent::Bottom)
        );
    }

    #[test]
    fn shift_is_ignored_for_back_tab() {
        // Terminals report Shift-Tab as `BackTab` with or without SHIFT.
        let binding = Key::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(binding.matches(key(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(binding.matches(key(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert!(!binding.matches(key(KeyCode::BackTab, KeyModifiers::ALT)));
        assert!(!binding.matches(key(KeyCode::Tab, KeyModifiers::NONE)));
    }

    #[test]
    fn ctrl_is_not_ignored_for_char_keys() {
        // A plain `d` is text; only Ctrl-D is paging.
        let map = KeyMap::vim_paging();
        assert_eq!(
            map.resolve(key(KeyCode::Char('d'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            map.resolve(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(NavIntent::PageDown)
        );
    }

    #[test]
    fn then_is_first_match_wins() {
        // Left map binds Down → Top; layering arrows underneath must not
        // override it.
        let custom = KeyMap::new().bind(KeyCode::Down, KeyModifiers::NONE, NavIntent::Top);
        let composed = custom.then(KeyMap::arrows());
        assert_eq!(
            composed.resolve(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(NavIntent::Top)
        );
        // A key only the right map binds still resolves.
        assert_eq!(
            composed.resolve(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(NavIntent::LineUp)
        );
    }

    #[test]
    fn conflicts_reports_overlapping_keys() {
        // Arrows and vim paging layer cleanly — no shared keys.
        assert!(KeyMap::arrows().conflicts(&KeyMap::vim_paging()).is_empty());

        // A panel binding Ctrl-U for itself would clash with vim paging.
        let panel =
            KeyMap::new().bind(KeyCode::Char('u'), KeyModifiers::CONTROL, NavIntent::PageUp);
        let clash = panel.conflicts(&KeyMap::vim_paging());
        assert_eq!(
            clash,
            vec![Key::new(KeyCode::Char('u'), KeyModifiers::CONTROL)]
        );
    }

    #[test]
    fn emacs_alt_and_ctrl_v_are_distinct() {
        let map = KeyMap::emacs();
        assert_eq!(
            map.resolve(key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Some(NavIntent::PageDown)
        );
        assert_eq!(
            map.resolve(key(KeyCode::Char('v'), KeyModifiers::ALT)),
            Some(NavIntent::PageUp)
        );
    }
}
