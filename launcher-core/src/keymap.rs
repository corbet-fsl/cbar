// keymap.rs — what a key means, decided by configuration rather than by a match arm.
//
// The bindings were spelled directly into the key handler: `Key::Left` did one thing, `Key::Tab`
// another, and changing any of it meant editing Rust. That is fine for one person who wrote it and
// wrong for everyone else -- vi hands want `hjkl`, emacs hands want `ctrl+n`, and neither is more
// correct than the arrow keys. A launcher is muscle memory or it is nothing.
//
// ── WHY THE CORE OWNS THIS WHEN THE CORE CANNOT SEE A KEYBOARD ───────────────────────────────
//
// Because "which action" is a decision and "which physical key" is a detail. The shell turns a
// toolkit's key event into a CHORD STRING -- `ctrl+shift+tab` -- and asks here what it means. That
// keeps every binding testable without a display, and it means a second shell inherits the
// bindings rather than reimplementing them.
//
// The chord format is deliberately the one a person would type into a config file: modifiers in a
// fixed order, lowercase, joined by `+`. Anything else would make the file a puzzle.
use serde::Deserialize;
use std::collections::HashMap;

/// What the launcher can be asked to do. Adding one here is the only place a new binding needs a
/// decision; the shell just has to know how to perform it.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Move within the current level -- machine when outside a box, app when inside one.
    MoveLeft,
    MoveRight,
    /// Move rows when outside a box, lines when inside one.
    MoveUp,
    MoveDown,
    /// Go one level in: from the grid into a box. Launches when already inside.
    Enter,
    /// Start everything on the current line, in one keystroke. The appset, as a verb.
    LaunchLine,
    /// Start everything in the current box.
    LaunchCell,
    /// Preserve the default contextual gesture: start the cell while outside it, or the selected
    /// line while inside it. Explicit `launch-line` and `launch-cell` bindings never change meaning
    /// with focus; this action exists so the default Shift+Return behavior can remain contextual
    /// without consulting the physical Shift modifier after the keymap has decoded it.
    LaunchSelection,
    /// In when outside, out when inside.
    ToggleInside,
    /// Straight out to the grid, wherever you are.
    GoOutside,
    /// Unwind one layer: clear the query, then leave the box, then close.
    Cancel,
    /// Delete the last character of the query.
    Backspace,
    /// Reveal every application hidden through the launcher.
    ResetVisibility,
}

/// Chord string -> action.
///
/// Everything not bound is TEXT. That is the rule that keeps a launcher usable: an unrecognised
/// key types into the search box rather than doing nothing, so the common case -- start typing the
/// name of the thing you want -- needs no binding at all and cannot be broken by one.
#[derive(Debug, Clone)]
pub struct Keymap {
    map: HashMap<String, Action>,
}

impl Default for Keymap {
    /// The bindings that were hardcoded, now merely the default. Arrow keys, Tab, Enter, Escape:
    /// what someone who has never read the configuration would try first.
    fn default() -> Self {
        let mut map = HashMap::new();
        for (chord, action) in [
            ("left", Action::MoveLeft),
            ("right", Action::MoveRight),
            ("up", Action::MoveUp),
            ("down", Action::MoveDown),
            ("return", Action::Enter),
            ("shift+return", Action::LaunchSelection),
            ("tab", Action::ToggleInside),
            ("shift+tab", Action::GoOutside),
            ("escape", Action::Cancel),
            ("backspace", Action::Backspace),
            ("ctrl+shift+h", Action::ResetVisibility),
        ] {
            map.insert(chord.to_string(), action);
        }
        Keymap { map }
    }
}

impl Keymap {
    /// Start from the defaults and let configuration override, add, or REMOVE.
    ///
    /// Overriding rather than replacing wholesale, because a user who wants `ctrl+n` for down
    /// almost never wants to lose Escape as well -- and a configuration format where adding one
    /// binding costs you the other nine is one people get wrong once and then distrust.
    ///
    /// An empty action name unbinds, which is the only way to say "I want this key to type" about
    /// a key that is bound by default.
    pub fn from_overrides(overrides: &HashMap<String, Option<Action>>) -> Self {
        let mut me = Keymap::default();
        for (chord, action) in overrides {
            let key = normalise(chord);
            match action {
                Some(a) => {
                    me.map.insert(key, *a);
                }
                None => {
                    me.map.remove(&key);
                }
            }
        }
        me
    }

    /// What this chord means, or None -- which means "this is text".
    pub fn action(&self, chord: &str) -> Option<Action> {
        self.map.get(&normalise(chord)).copied()
    }

    /// Build the canonical chord for a key plus the modifiers that were held.
    ///
    /// The shell calls this with whatever its toolkit reports, so the ORDER modifiers arrive in
    /// cannot matter -- a binding written `shift+ctrl+x` has to find a key pressed as ctrl+shift+x,
    /// or the configuration file becomes a place where two spellings of the same chord behave
    /// differently and nobody can see why.
    pub fn chord(key: &str, ctrl: bool, alt: bool, shift: bool, logo: bool) -> String {
        let mut out = String::new();
        for (held, name) in [
            (ctrl, "ctrl"),
            (alt, "alt"),
            (shift, "shift"),
            (logo, "super"),
        ] {
            if held {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&key.to_lowercase());
        out
    }
}

/// One spelling for one chord: lowercase, modifiers in a fixed order, common aliases folded in.
fn normalise(chord: &str) -> String {
    let mut mods = [false; 4];
    let mut key = String::new();
    for part in chord.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => mods[0] = true,
            "alt" | "meta" => mods[1] = true,
            "shift" => mods[2] = true,
            "super" | "logo" | "mod4" | "cmd" => mods[3] = true,
            other => {
                // Aliases people actually write, folded to one name each. Someone binding `esc`
                // means Escape, and discovering otherwise costs an evening.
                key = match other {
                    "esc" => "escape",
                    "enter" | "ret" => "return",
                    "bs" | "backspace" => "backspace",
                    "spc" | "space" => "space",
                    o => o,
                }
                .to_string();
            }
        }
    }
    Keymap::chord(&key, mods[0], mods[1], mods[2], mods[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_what_was_hardcoded() {
        let k = Keymap::default();
        assert_eq!(k.action("left"), Some(Action::MoveLeft));
        assert_eq!(k.action("shift+return"), Some(Action::LaunchSelection));
        assert_eq!(k.action("escape"), Some(Action::Cancel));
        assert_eq!(
            k.action("ctrl+shift+h"),
            Some(Action::ResetVisibility),
            "hidden applications always have a discoverable way back"
        );
    }

    /// THE RULE THAT KEEPS IT USABLE: anything unbound is text, so typing a name always works and
    /// no configuration can take that away.
    #[test]
    fn an_unbound_key_is_text() {
        assert_eq!(Keymap::default().action("f"), None);
        assert_eq!(Keymap::default().action("ctrl+w"), None);
    }

    /// Modifier order is not a spelling test.
    #[test]
    fn modifier_order_does_not_matter() {
        let k = Keymap::default();
        assert_eq!(k.action("shift+tab"), k.action("tab+shift"));
        assert_eq!(k.action("shift+return"), Some(Action::LaunchSelection));
    }

    #[test]
    fn the_aliases_people_write_are_understood() {
        let k = Keymap::default();
        assert_eq!(k.action("esc"), Some(Action::Cancel));
        assert_eq!(k.action("enter"), Some(Action::Enter));
        assert_eq!(k.action("ESCAPE"), Some(Action::Cancel));
    }

    /// Overriding must not cost you the bindings you did not mention.
    #[test]
    fn an_override_keeps_the_rest() {
        let mut o = HashMap::new();
        o.insert("ctrl+n".to_string(), Some(Action::MoveDown));
        let k = Keymap::from_overrides(&o);
        assert_eq!(k.action("ctrl+n"), Some(Action::MoveDown));
        assert_eq!(
            k.action("escape"),
            Some(Action::Cancel),
            "the rest survived"
        );
    }

    /// And unbinding is possible, or a key bound by default could never be made to type.
    #[test]
    fn an_empty_binding_unbinds() {
        let mut o = HashMap::new();
        o.insert("tab".to_string(), None);
        let k = Keymap::from_overrides(&o);
        assert_eq!(k.action("tab"), None, "tab now types");
        assert_eq!(k.action("left"), Some(Action::MoveLeft));
    }

    /// vi hands, as the motivating example: hjkl bound, arrows still working.
    #[test]
    fn vi_style_bindings_coexist_with_the_defaults() {
        let mut o = HashMap::new();
        for (c, a) in [
            ("ctrl+h", Action::MoveLeft),
            ("ctrl+j", Action::MoveDown),
            ("ctrl+k", Action::MoveUp),
            ("ctrl+l", Action::MoveRight),
        ] {
            o.insert(c.to_string(), Some(a));
        }
        let k = Keymap::from_overrides(&o);
        assert_eq!(k.action("ctrl+j"), Some(Action::MoveDown));
        assert_eq!(
            k.action("down"),
            Some(Action::MoveDown),
            "arrows still work"
        );
        // And plain h still types, so searching for "helix" is unaffected.
        assert_eq!(k.action("h"), None);
    }
}
