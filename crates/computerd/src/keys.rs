//! Key names, keysyms, and the keyboard mapping lookups used to type.

pub(crate) const SHIFT_L: u32 = 0xffe1;
pub(crate) const RETURN: u32 = 0xff0d;
const TAB: u32 = 0xff09;

/// Offset that turns a Unicode code point into a keysym outside Latin-1.
const UNICODE_KEYSYM_BASE: u32 = 0x0100_0000;

const NAMED_KEYS: &[(&str, u32)] = &[
    ("enter", RETURN),
    ("return", RETURN),
    ("esc", 0xff1b),
    ("escape", 0xff1b),
    ("tab", TAB),
    ("backspace", 0xff08),
    ("delete", 0xffff),
    ("del", 0xffff),
    ("insert", 0xff63),
    ("space", 0x20),
    ("left", 0xff51),
    ("up", 0xff52),
    ("right", 0xff53),
    ("down", 0xff54),
    ("home", 0xff50),
    ("end", 0xff57),
    ("pageup", 0xff55),
    ("page_up", 0xff55),
    ("pagedown", 0xff56),
    ("page_down", 0xff56),
];

const MODIFIERS: &[(&str, u32)] = &[
    ("ctrl", 0xffe3),
    ("control", 0xffe3),
    ("shift", SHIFT_L),
    ("alt", 0xffe9),
    ("option", 0xffe9),
    ("super", 0xffeb),
    ("cmd", 0xffeb),
    ("meta", 0xffeb),
    ("win", 0xffeb),
];

const F1: u32 = 0xffbe;
const LAST_FUNCTION_KEY: u32 = 12;

/// Keysym for typing `c`, or `None` for control characters that have no key.
///
/// A newline is Enter and a tab is Tab. A carriage return has no key.
pub(crate) fn char_keysym(c: char) -> Option<u32> {
    match c {
        '\n' => Some(RETURN),
        '\t' => Some(TAB),
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => Some(u32::from(c)),
        c if c.is_control() => None,
        c => Some(UNICODE_KEYSYM_BASE + u32::from(c)),
    }
}

/// Keysym for a key name such as `enter`, `PageDown`, `f5`, or a single character.
pub(crate) fn key_keysym(name: &str) -> Result<u32, String> {
    let mut chars = name.chars();
    if let (Some(only), None) = (chars.next(), chars.next()) {
        return char_keysym(only).ok_or_else(|| format!("{name:?} is not a key"));
    }
    let lower = name.to_lowercase();
    if let Some((_, keysym)) = NAMED_KEYS.iter().find(|(known, _)| *known == lower) {
        return Ok(*keysym);
    }
    if let Some((_, keysym)) = MODIFIERS.iter().find(|(known, _)| *known == lower) {
        return Ok(*keysym);
    }
    if let Some(number) = lower
        .strip_prefix('f')
        .and_then(|digits| digits.parse::<u32>().ok())
        .filter(|number| (1..=LAST_FUNCTION_KEY).contains(number))
    {
        return Ok(F1 + number - 1);
    }
    Err(format!(
        "unknown key {name:?}. Use a single character, enter, esc, tab, backspace, delete, space, left, right, up, down, home, end, pageup, pagedown, f1 to f12, or a modifier name"
    ))
}

/// Keysym for a modifier name such as `ctrl` or `cmd`.
pub(crate) fn modifier_keysym(name: &str) -> Result<u32, String> {
    let lower = name.trim().to_lowercase();
    MODIFIERS
        .iter()
        .find(|(known, _)| *known == lower)
        .map(|(_, keysym)| *keysym)
        .ok_or_else(|| {
            format!("unknown modifier {name:?}. Use ctrl, alt, shift, or super (also cmd, meta, win, option)")
        })
}

/// A key to press, and whether Shift must be held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyPress {
    pub(crate) keycode: u8,
    pub(crate) shift: bool,
}

/// The server's keyboard mapping: the keysyms of each keycode, level by level.
#[derive(Debug, Clone)]
pub(crate) struct Keymap {
    first_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
}

impl Keymap {
    /// `keysyms` holds `per_keycode` entries for each keycode from `first_keycode` on.
    pub(crate) fn new(first_keycode: u8, per_keycode: u8, keysyms: Vec<u32>) -> Self {
        Self {
            first_keycode,
            per_keycode: usize::from(per_keycode.max(1)),
            keysyms,
        }
    }

    fn rows(&self) -> impl Iterator<Item = (u8, &[u32])> {
        self.keysyms
            .chunks(self.per_keycode)
            .enumerate()
            .filter_map(|(index, row)| {
                let keycode = u8::try_from(usize::from(self.first_keycode) + index).ok()?;
                Some((keycode, row))
            })
    }

    /// The key that produces `keysym` unshifted, or else with Shift. Keys that need
    /// `AltGr` or another group are not used.
    pub(crate) fn find(&self, keysym: u32) -> Option<KeyPress> {
        for (level, shift) in [(0, false), (1, true)] {
            if let Some((keycode, _)) = self.rows().find(|(_, row)| row.get(level) == Some(&keysym))
            {
                return Some(KeyPress { keycode, shift });
            }
        }
        None
    }

    /// Keycodes with no keysyms, highest first. They are free to bind temporarily.
    fn spares(&self) -> Vec<u8> {
        let mut spares: Vec<u8> = self
            .rows()
            .filter(|(_, row)| row.iter().all(|keysym| *keysym == 0))
            .map(|(keycode, _)| keycode)
            .collect();
        spares.reverse();
        spares
    }
}

/// Most keysyms bound at once, so one pause covers many characters.
const MAX_BINDINGS: usize = 32;

/// How to produce one keysym.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tap {
    /// A key the keyboard already has.
    Key(KeyPress),
    /// A spare keycode bound to the keysym for the length of its segment.
    Bound(u8),
}

/// Keysyms typed under one set of temporary bindings.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) bindings: Vec<(u8, u32)>,
    pub(crate) taps: Vec<Tap>,
}

/// The keyboard has no spare keycode to type a character with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the keyboard has no free key to type characters it lacks")]
pub(crate) struct NoSpareKey;

/// Splits `keysyms` into segments, each needing at most [`MAX_BINDINGS`] temporary bindings.
pub(crate) fn segments(keymap: &Keymap, keysyms: &[u32]) -> Result<Vec<Segment>, NoSpareKey> {
    let mut spares = keymap.spares();
    spares.truncate(MAX_BINDINGS);
    let mut done = Vec::new();
    let mut current = Segment::default();
    for &keysym in keysyms {
        if let Some(press) = keymap.find(keysym) {
            current.taps.push(Tap::Key(press));
            continue;
        }
        let existing = current.bindings.iter().find(|(_, bound)| *bound == keysym);
        let keycode = if let Some((keycode, _)) = existing {
            *keycode
        } else {
            if current.bindings.len() == spares.len() {
                if spares.is_empty() {
                    return Err(NoSpareKey);
                }
                done.push(std::mem::take(&mut current));
            }
            let keycode = spares[current.bindings.len()];
            current.bindings.push((keycode, keysym));
            keycode
        };
        current.taps.push(Tap::Bound(keycode));
    }
    if !current.taps.is_empty() {
        done.push(current);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_map_to_latin1_or_unicode_keysyms() {
        assert_eq!(char_keysym('a'), Some(0x61));
        assert_eq!(char_keysym('é'), Some(0xe9));
        assert_eq!(char_keysym('→'), Some(0x0100_2192));
        assert_eq!(char_keysym('😀'), Some(0x0101_f600));
        assert_eq!(char_keysym('\n'), Some(RETURN));
        assert_eq!(char_keysym('\u{7f}'), None);
        assert_eq!(char_keysym('\r'), None);
    }

    #[test]
    fn key_names_accept_aliases_and_ignore_case() {
        for (name, keysym) in [
            ("Enter", RETURN),
            ("return", RETURN),
            ("ESC", 0xff1b),
            ("PageDown", 0xff56),
            ("f1", 0xffbe),
            ("F12", 0xffc9),
            ("cmd", 0xffeb),
            ("Option", 0xffe9),
            ("A", 0x41),
            ("+", 0x2b),
        ] {
            assert_eq!(key_keysym(name), Ok(keysym), "{name}");
        }
        assert!(key_keysym("f13").is_err());
        assert!(key_keysym("enterr").is_err());
    }

    #[test]
    fn modifiers_reject_ordinary_keys() {
        assert_eq!(modifier_keysym("Ctrl"), Ok(0xffe3));
        assert_eq!(modifier_keysym("win"), modifier_keysym("super"));
        assert!(modifier_keysym("enter").is_err());
    }

    fn keymap() -> Keymap {
        // keycodes 8..=11, two keysyms each: a/A, 1/!, only AltGr at level 3, empty
        Keymap::new(
            8,
            4,
            vec![
                0x61, 0x41, 0, 0, //
                0x31, 0x21, 0, 0, //
                0, 0, 0x40, 0, //
                0, 0, 0, 0,
            ],
        )
    }

    #[test]
    fn find_prefers_unshifted_and_skips_altgr_levels() {
        let keymap = keymap();
        assert_eq!(
            keymap.find(0x61),
            Some(KeyPress {
                keycode: 8,
                shift: false
            })
        );
        assert_eq!(
            keymap.find(0x41),
            Some(KeyPress {
                keycode: 8,
                shift: true
            })
        );
        assert_eq!(
            keymap.find(0x21),
            Some(KeyPress {
                keycode: 9,
                shift: true
            })
        );
        assert_eq!(keymap.find(0x40), None);
        assert_eq!(keymap.find(0xe9), None);
    }

    #[test]
    fn spares_list_empty_keycodes_highest_first() {
        assert_eq!(keymap().spares(), vec![11]);
        assert_eq!(
            Keymap::new(8, 2, vec![1, 0, 2, 0]).spares(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn unbound_characters_share_bindings_and_overflow_into_new_segments() {
        // two spare keycodes: 10 and 9
        let keymap = Keymap::new(8, 2, vec![0x61, 0x41, 0, 0, 0, 0]);
        let kana = [0x0100_3042, 0x0100_3044, 0x0100_3042, 0x61, 0x0100_3046];
        let planned = segments(&keymap, &kana).unwrap();
        assert_eq!(
            planned,
            vec![
                Segment {
                    bindings: vec![(10, 0x0100_3042), (9, 0x0100_3044)],
                    taps: vec![
                        Tap::Bound(10),
                        Tap::Bound(9),
                        Tap::Bound(10),
                        Tap::Key(KeyPress {
                            keycode: 8,
                            shift: false
                        }),
                    ],
                },
                Segment {
                    bindings: vec![(10, 0x0100_3046)],
                    taps: vec![Tap::Bound(10)],
                },
            ]
        );
        assert_eq!(segments(&keymap, &[0x61]).unwrap().len(), 1);
        assert_eq!(segments(&keymap, &[]), Ok(vec![]));
        let full = Keymap::new(8, 2, vec![1, 0]);
        assert_eq!(segments(&full, &[0x0100_3042]), Err(NoSpareKey));
        assert!(segments(&full, &[1]).is_ok());
    }
}
