//! Turns a validated batch into steps the X server can run, checking what only the screen size and key tables can.

use std::time::Duration;

use computer_protocol::{Action, Button, Direction, Point, ScreenSize};

use crate::keys;

/// Press or release of a mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Press,
    Release,
}

/// An input the X server can take directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Move(Point),
    Click {
        at: Point,
        button: Button,
    },
    Button {
        at: Option<Point>,
        button: Button,
        edge: Edge,
    },
    Type(Vec<u32>),
    Key {
        keysym: u32,
        modifiers: Vec<u32>,
    },
    Scroll {
        at: Option<Point>,
        direction: Direction,
        amount: u8,
    },
}

/// One unit of work on a screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Input(Input),
    Wait(Duration),
    Focus {
        application: String,
        uri: Option<String>,
    },
}

fn check_point(point: Point, size: ScreenSize, number: usize) -> Result<(), String> {
    if point.x < size.width() && point.y < size.height() {
        Ok(())
    } else {
        Err(format!(
            "action {number}: ({}, {}) is outside the {size} screen, x runs 0 to {} and y runs 0 to {}",
            point.x,
            point.y,
            size.width() - 1,
            size.height() - 1,
        ))
    }
}

/// Converts actions to steps. Fails before anything runs when a position is off the
/// screen, a key or modifier name is unknown, or text holds a character with no key.
pub fn plan(actions: &[Action], size: ScreenSize) -> Result<Vec<Step>, String> {
    actions
        .iter()
        .enumerate()
        .map(|(index, action)| plan_one(index + 1, action, size))
        .collect()
}

fn plan_one(number: usize, action: &Action, size: ScreenSize) -> Result<Step, String> {
    let checked = |at: Option<Point>| -> Result<Option<Point>, String> {
        if let Some(at) = at {
            check_point(at, size, number)?;
        }
        Ok(at)
    };
    let input = match action {
        Action::Click { at, button } => {
            check_point(*at, size, number)?;
            Input::Click {
                at: *at,
                button: *button,
            }
        }
        Action::Move { at } => {
            check_point(*at, size, number)?;
            Input::Move(*at)
        }
        Action::Down { at, button } => Input::Button {
            at: checked(*at)?,
            button: *button,
            edge: Edge::Press,
        },
        Action::Up { at, button } => Input::Button {
            at: checked(*at)?,
            button: *button,
            edge: Edge::Release,
        },
        Action::Type { text } => {
            let mut keysyms = Vec::with_capacity(text.len());
            for c in text.chars() {
                match keys::char_keysym(c) {
                    Some(keysym) => keysyms.push(keysym),
                    None if c == '\r' => {}
                    None => {
                        return Err(format!(
                            "action {number} (type): U+{:04X} is a control character with no key, remove it from the text",
                            u32::from(c)
                        ));
                    }
                }
            }
            Input::Type(keysyms)
        }
        Action::Key { key, modifiers } => Input::Key {
            keysym: keys::key_keysym(key).map_err(|error| format!("action {number}: {error}"))?,
            modifiers: modifiers
                .iter()
                .map(|name| {
                    keys::modifier_keysym(name).map_err(|error| format!("action {number}: {error}"))
                })
                .collect::<Result<_, _>>()?,
        },
        Action::Scroll {
            at,
            direction,
            amount,
        } => Input::Scroll {
            at: checked(*at)?,
            direction: *direction,
            amount: *amount,
        },
        Action::Wait { ms } => return Ok(Step::Wait(Duration::from_millis(u64::from(*ms)))),
        Action::Focus { application, uri } => {
            return Ok(Step::Focus {
                application: application.clone(),
                uri: uri.clone(),
            });
        }
    };
    Ok(Step::Input(input))
}

/// A top-level window as the window manager lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    /// Instance and class from `WM_CLASS`.
    pub class: Vec<String>,
    pub title: String,
}

impl WindowInfo {
    /// A short description for messages, such as `xterm: ~ (XTerm)`.
    pub fn describe(&self) -> String {
        match self.class.as_slice() {
            [] => format!("{:?}", self.title),
            [.., class] => format!("{:?} ({class})", self.title),
        }
    }
}

/// Picks the window that `application` names. A class equal to the name wins over a
/// class or title that merely contains it. Ties go to the earliest window.
pub fn pick_window<'a>(windows: &'a [WindowInfo], application: &str) -> Option<&'a WindowInfo> {
    let wanted = application.trim().to_lowercase();
    let exact = windows.iter().find(|window| {
        window
            .class
            .iter()
            .any(|class| class.to_lowercase() == wanted)
    });
    exact.or_else(|| {
        windows.iter().find(|window| {
            window
                .class
                .iter()
                .any(|class| class.to_lowercase().contains(&wanted))
                || window.title.to_lowercase().contains(&wanted)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> ScreenSize {
        ScreenSize::parse("800x600").unwrap()
    }

    #[test]
    fn positions_must_lie_on_the_screen_and_the_error_names_its_size() {
        let at = |x, y| Point { x, y };
        let click = |at| Action::Click {
            at,
            button: Button::Left,
        };
        assert!(plan(&[click(at(799, 599))], size()).is_ok());
        for outside in [at(800, 0), at(0, 600)] {
            let error = plan(&[click(outside)], size()).unwrap_err();
            assert!(error.contains("800x600"), "{error}");
        }
        let scroll = Action::Scroll {
            at: Some(at(900, 1)),
            direction: Direction::Down,
            amount: 3,
        };
        assert!(plan(&[scroll], size()).is_err());
    }

    #[test]
    fn keys_modifiers_and_text_become_keysyms() {
        let steps = plan(
            &[
                Action::Key {
                    key: "Enter".to_owned(),
                    modifiers: vec!["Cmd".to_owned(), "shift".to_owned()],
                },
                Action::Type {
                    text: "a→\r\n".to_owned(),
                },
            ],
            size(),
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Input(Input::Key {
                    keysym: keys::RETURN,
                    modifiers: vec![0xffeb, keys::SHIFT_L],
                }),
                Step::Input(Input::Type(vec![0x61, 0x0100_2192, keys::RETURN])),
            ]
        );
    }

    #[test]
    fn unknown_keys_and_control_characters_are_refused_before_anything_runs() {
        let key = |key: &str, modifier: &str| Action::Key {
            key: key.to_owned(),
            modifiers: vec![modifier.to_owned()],
        };
        assert!(plan(&[key("nope", "ctrl")], size()).is_err());
        assert!(plan(&[key("a", "enter")], size()).is_err());
        let error = plan(
            &[Action::Type {
                text: "a\u{1b}".to_owned(),
            }],
            size(),
        )
        .unwrap_err();
        assert!(error.contains("U+001B"), "{error}");
    }

    fn window(id: u32, class: &[&str], title: &str) -> WindowInfo {
        WindowInfo {
            id,
            class: class.iter().map(|class| (*class).to_owned()).collect(),
            title: title.to_owned(),
        }
    }

    #[test]
    fn focus_prefers_an_exact_class_over_a_title_that_mentions_it() {
        let windows = [
            window(1, &["navigator", "Firefox"], "How to use xterm - Firefox"),
            window(2, &["xterm", "XTerm"], "~"),
        ];
        assert_eq!(pick_window(&windows, "XTERM").map(|w| w.id), Some(2));
        assert_eq!(pick_window(&windows, "fire").map(|w| w.id), Some(1));
        assert_eq!(pick_window(&windows, "gimp"), None);
    }
}
