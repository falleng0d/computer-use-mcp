//! The `computer_act` batch: what an agent sends, and the typed actions `computerd` runs.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Observation;

/// Most actions one batch may hold, counting each double click as two clicks.
pub const MAX_ACTIONS: usize = 24;

/// Wait length when an action gives none, in milliseconds.
pub const DEFAULT_WAIT_MS: u16 = 350;

/// Longest single wait, and longest settle time, in milliseconds.
pub const MAX_WAIT_MS: u16 = 5000;

/// Settle time before the closing screenshot when the batch gives none, in milliseconds.
pub const DEFAULT_SETTLE_MS: u16 = 300;

/// Wheel steps per scroll action when it gives none.
pub const DEFAULT_SCROLL_AMOUNT: u8 = 3;

/// Most wheel steps one scroll action may take.
pub const MAX_SCROLL_AMOUNT: u8 = 20;

/// Most characters all `type` actions of one batch may hold together.
pub const MAX_TYPE_CHARS: usize = 1000;

/// Time `computerd` may spend on top of the waits and typing of a batch before giving up.
const WORK_ALLOWANCE: Duration = Duration::from_secs(30);

/// Time a `focus` action may take when it has to start an application.
const LAUNCH_ALLOWANCE: Duration = Duration::from_secs(40);

/// Time applications get to reread the keyboard after a temporary key binding changes.
pub const KEYMAP_SETTLE: Duration = Duration::from_millis(20);

/// Allowance for the X round trips behind one typed character.
const ROUND_TRIP_ALLOWANCE: Duration = Duration::from_millis(10);

/// Time one typed character may take: it can need its own binding, which costs two settle pauses.
const TYPE_CHAR_ALLOWANCE: Duration = KEYMAP_SETTLE
    .saturating_mul(2)
    .saturating_add(ROUND_TRIP_ALLOWANCE);

/// Mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// A position on the screen, in pixels from the top left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

/// One validated action. Double clicks are already split into two clicks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Click {
        at: Point,
        button: Button,
    },
    Move {
        at: Point,
    },
    Down {
        at: Option<Point>,
        button: Button,
    },
    Up {
        at: Option<Point>,
        button: Button,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
        modifiers: Vec<String>,
    },
    Scroll {
        at: Option<Point>,
        direction: Direction,
        amount: u8,
    },
    Wait {
        ms: u16,
    },
    Focus {
        application: String,
        uri: Option<String>,
    },
}

/// Body of `POST /sessions/{id}/act`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActRequest {
    pub actions: Vec<Action>,
    /// Whether to end with a screenshot.
    pub observe: bool,
    /// Pause before the closing screenshot, in milliseconds.
    pub settle_ms: u16,
}

impl ActRequest {
    /// Longest the batch can take: its waits, the settle time, typing, launches, and a fixed allowance for the rest.
    #[must_use]
    pub fn time_budget(&self) -> Duration {
        let mut budget = WORK_ALLOWANCE + Duration::from_millis(u64::from(self.settle_ms));
        for action in &self.actions {
            match action {
                Action::Wait { ms } => budget += Duration::from_millis(u64::from(*ms)),
                Action::Type { text } => {
                    budget += TYPE_CHAR_ALLOWANCE
                        * u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
                }
                Action::Focus { .. } => budget += LAUNCH_ALLOWANCE,
                _ => {}
            }
        }
        budget
    }
}

/// Reply to `POST /sessions/{id}/act`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActReply {
    pub actions_run: usize,
    /// The closing screenshot, when the batch asked for one.
    pub observation: Option<Observation>,
    /// Screen number this batch opened for the session, when it was the session's first call.
    #[serde(default)]
    pub opened_screen: Option<u8>,
}

/// Kind of a [`RawAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Click,
    Move,
    Down,
    Up,
    Type,
    Key,
    Scroll,
    Wait,
    Focus,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Move => "move",
            Self::Down => "down",
            Self::Up => "up",
            Self::Type => "type",
            Self::Key => "key",
            Self::Scroll => "scroll",
            Self::Wait => "wait",
            Self::Focus => "focus",
        }
    }
}

/// An action as the agent writes it. Which fields apply depends on `kind`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RawAction {
    /// What to do.
    #[serde(default)]
    pub kind: Option<Kind>,
    /// Pixel column on the screen. Needed by click and move, optional for down, up, and scroll.
    pub x: Option<f64>,
    /// Pixel row on the screen. Needed by click and move, optional for down, up, and scroll.
    pub y: Option<f64>,
    /// Mouse button for click, down, and up. Default left.
    pub button: Option<Button>,
    /// For click: click twice.
    pub double: Option<bool>,
    /// For type: the text to type. Any Unicode works. A newline presses Enter.
    pub text: Option<String>,
    /// For key: a key name such as enter, esc, tab, backspace, delete, space, left, right, up, down, home, end, pageup, pagedown, f1 to f12, or a single character.
    pub key: Option<String>,
    /// For key: modifiers held while the key is pressed, from ctrl, alt, shift, and super (also cmd, meta, win, option).
    pub modifiers: Option<Vec<String>>,
    /// For scroll: the direction.
    pub direction: Option<Direction>,
    /// For scroll: wheel steps, 1 to 20. Default 3.
    pub amount: Option<f64>,
    /// For wait: milliseconds, up to 5000. Default 350.
    pub ms: Option<f64>,
    /// For focus: name or title of an open window to raise, matched without regard to case. Also names an application to start when no window matches.
    pub application: Option<String>,
    /// For focus: a page or file to open in the application. The application is started or reused with it.
    pub uri: Option<String>,
}

/// Why a batch was refused. The text tells the agent what to fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ActionError(String);

impl ActionError {
    /// A refusal with the given message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn to_u16(value: f64) -> Option<u16> {
    let rounded = value.round();
    if (0.0..=f64::from(u16::MAX)).contains(&rounded) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the range check above keeps the value inside u16"
        )]
        Some(rounded as u16)
    } else {
        None
    }
}

fn coordinate(number: usize, kind: Kind, axis: &str, value: f64) -> Result<u16, ActionError> {
    to_u16(value).ok_or_else(|| {
        ActionError::new(format!(
            "action {number} ({}): {axis} must be a screen pixel from 0 to {}, got {value}",
            kind.name(),
            u16::MAX
        ))
    })
}

fn point(number: usize, kind: Kind, raw: &RawAction) -> Result<Option<Point>, ActionError> {
    match (raw.x, raw.y) {
        (Some(x), Some(y)) => Ok(Some(Point {
            x: coordinate(number, kind, "x", x)?,
            y: coordinate(number, kind, "y", y)?,
        })),
        (None, None) => Ok(None),
        _ => Err(ActionError::new(format!(
            "action {number} ({}): give both x and y, or neither",
            kind.name()
        ))),
    }
}

fn required_point(number: usize, kind: Kind, raw: &RawAction) -> Result<Point, ActionError> {
    point(number, kind, raw)?.ok_or_else(|| {
        ActionError::new(format!(
            "action {number} ({}) needs x and y, the pixel position on the screen",
            kind.name()
        ))
    })
}

/// Rounds `value` into `min..=max`. `None` when it is not a number.
fn clamped(value: f64, min: u16, max: u16) -> Option<u16> {
    if value.is_nan() {
        return None;
    }
    to_u16(value.clamp(f64::from(min), f64::from(max)))
}

fn parse_one(number: usize, raw: &RawAction, out: &mut Vec<Action>) -> Result<(), ActionError> {
    let Some(kind) = raw.kind else {
        return Err(ActionError::new(format!(
            "action {number} has no kind, use one of click, move, down, up, type, key, scroll, wait, focus"
        )));
    };
    let button = raw.button.unwrap_or(Button::Left);
    match kind {
        Kind::Click => {
            let at = required_point(number, kind, raw)?;
            let click = Action::Click { at, button };
            out.push(click.clone());
            if raw.double == Some(true) {
                out.push(click);
            }
        }
        Kind::Move => out.push(Action::Move {
            at: required_point(number, kind, raw)?,
        }),
        Kind::Down => out.push(Action::Down {
            at: point(number, kind, raw)?,
            button,
        }),
        Kind::Up => out.push(Action::Up {
            at: point(number, kind, raw)?,
            button,
        }),
        Kind::Type => {
            let text = raw.text.clone().ok_or_else(|| {
                ActionError::new(format!("action {number} (type) needs text to type"))
            })?;
            out.push(Action::Type { text });
        }
        Kind::Key => {
            let key = raw
                .key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    ActionError::new(format!(
                        "action {number} (key) needs a key such as enter, esc, tab, or a letter"
                    ))
                })?;
            out.push(Action::Key {
                key: key.to_owned(),
                modifiers: raw.modifiers.clone().unwrap_or_default(),
            });
        }
        Kind::Scroll => {
            let direction = raw.direction.ok_or_else(|| {
                ActionError::new(format!(
                    "action {number} (scroll) needs a direction: up, down, left, or right"
                ))
            })?;
            let amount = match raw.amount {
                None => DEFAULT_SCROLL_AMOUNT,
                Some(value) => clamped(value, 1, u16::from(MAX_SCROLL_AMOUNT))
                    .and_then(|amount| u8::try_from(amount).ok())
                    .ok_or_else(|| {
                        ActionError::new(format!(
                            "action {number} (scroll): amount is not a number"
                        ))
                    })?,
            };
            out.push(Action::Scroll {
                at: point(number, kind, raw)?,
                direction,
                amount,
            });
        }
        Kind::Wait => {
            let ms = match raw.ms {
                None => DEFAULT_WAIT_MS,
                Some(value) => clamped(value, 0, MAX_WAIT_MS).ok_or_else(|| {
                    ActionError::new(format!("action {number} (wait): ms is not a number"))
                })?,
            };
            out.push(Action::Wait { ms });
        }
        Kind::Focus => {
            let application = raw
                .application
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ActionError::new(format!(
                        "action {number} (focus) needs the application name or window title to raise"
                    ))
                })?;
            out.push(Action::Focus {
                application: application.to_owned(),
                uri: raw.uri.clone(),
            });
        }
    }
    Ok(())
}

impl ActRequest {
    /// Turns the agent's batch into typed actions and applies every limit.
    ///
    /// `observe` defaults to true and `settle_ms` to [`DEFAULT_SETTLE_MS`]. Waits and
    /// the settle time are capped at [`MAX_WAIT_MS`].
    ///
    /// # Errors
    ///
    /// Fails when the batch is empty or too long, an action lacks a field it needs,
    /// or a number is not usable. The message names the action.
    pub fn parse(
        raw: &[RawAction],
        observe: Option<bool>,
        settle_ms: Option<f64>,
    ) -> Result<Self, ActionError> {
        if raw.is_empty() {
            return Err(ActionError::new("actions must hold at least one action"));
        }
        if raw.len() > MAX_ACTIONS {
            return Err(ActionError::new(format!(
                "{} actions given, the limit is {MAX_ACTIONS}. Split the batch",
                raw.len()
            )));
        }
        let mut actions = Vec::with_capacity(raw.len());
        for (index, action) in raw.iter().enumerate() {
            parse_one(index + 1, action, &mut actions)?;
        }
        if actions.len() > MAX_ACTIONS {
            return Err(ActionError::new(format!(
                "the batch runs {} actions because each double click counts as two, the limit is {MAX_ACTIONS}. Split the batch",
                actions.len()
            )));
        }
        let typed: usize = actions
            .iter()
            .map(|action| match action {
                Action::Type { text } => text.chars().count(),
                _ => 0,
            })
            .sum();
        if typed > MAX_TYPE_CHARS {
            return Err(ActionError::new(format!(
                "the batch types {typed} characters, the limit is {MAX_TYPE_CHARS}. Split the text across batches"
            )));
        }
        let settle_ms = match settle_ms {
            None => DEFAULT_SETTLE_MS,
            Some(value) => clamped(value, 0, MAX_WAIT_MS)
                .ok_or_else(|| ActionError::new("settle_ms is not a number"))?,
        };
        Ok(Self {
            actions,
            observe: observe.unwrap_or(true),
            settle_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: serde_json::Value) -> Vec<RawAction> {
        serde_json::from_value(json).unwrap()
    }

    fn parse(json: serde_json::Value) -> Result<ActRequest, ActionError> {
        ActRequest::parse(&raw(json), None, None)
    }

    fn repeated(action: serde_json::Value, times: usize) -> serde_json::Value {
        serde_json::Value::Array(vec![action; times])
    }

    #[test]
    fn double_click_becomes_two_clicks_and_counts_toward_the_limit() {
        let request = parse(serde_json::json!([
            { "kind": "click", "x": 10.4, "y": 20.6, "button": "right", "double": true }
        ]))
        .unwrap();
        let click = Action::Click {
            at: Point { x: 10, y: 21 },
            button: Button::Right,
        };
        assert_eq!(request.actions, vec![click.clone(), click]);

        let double = serde_json::json!({ "kind": "click", "x": 1, "y": 1, "double": true });
        assert_eq!(
            parse(repeated(double.clone(), 12)).unwrap().actions.len(),
            MAX_ACTIONS
        );
        let error = parse(repeated(double, 13)).unwrap_err();
        assert!(error.to_string().contains("runs 26 actions"), "{error}");

        let wait = serde_json::json!({ "kind": "wait" });
        assert_eq!(parse(repeated(wait.clone(), 24)).unwrap().actions.len(), 24);
        assert!(parse(repeated(wait, 25)).is_err());
    }

    #[test]
    fn waits_scrolls_and_settle_are_clamped_and_defaulted() {
        let request = ActRequest::parse(
            &raw(serde_json::json!([
                { "kind": "wait" },
                { "kind": "wait", "ms": 99999 },
                { "kind": "wait", "ms": -5 },
                { "kind": "scroll", "direction": "down" },
                { "kind": "scroll", "direction": "left", "amount": 500 },
                { "kind": "scroll", "direction": "up", "amount": 0, "x": 5, "y": 6 },
            ])),
            Some(false),
            Some(60000.0),
        )
        .unwrap();
        let scroll = |direction, amount, at| Action::Scroll {
            at,
            direction,
            amount,
        };
        assert_eq!(
            request.actions,
            vec![
                Action::Wait { ms: 350 },
                Action::Wait { ms: 5000 },
                Action::Wait { ms: 0 },
                scroll(Direction::Down, 3, None),
                scroll(Direction::Left, 20, None),
                scroll(Direction::Up, 1, Some(Point { x: 5, y: 6 })),
            ]
        );
        assert!(!request.observe);
        assert_eq!(request.settle_ms, 5000);

        let defaults =
            ActRequest::parse(&raw(serde_json::json!([{ "kind": "wait" }])), None, None).unwrap();
        assert!(defaults.observe);
        assert_eq!(defaults.settle_ms, DEFAULT_SETTLE_MS);
    }

    #[test]
    fn missing_fields_and_bad_numbers_name_the_action() {
        for (action, expected) in [
            (serde_json::json!({}), "action 2 has no kind"),
            (
                serde_json::json!({ "kind": "click" }),
                "action 2 (click) needs x and y",
            ),
            (
                serde_json::json!({ "kind": "click", "x": 1 }),
                "action 2 (click): give both x and y",
            ),
            (
                serde_json::json!({ "kind": "down", "x": 1 }),
                "action 2 (down): give both x and y",
            ),
            (
                serde_json::json!({ "kind": "move", "x": -1, "y": 2 }),
                "action 2 (move): x must be",
            ),
            (
                serde_json::json!({ "kind": "type" }),
                "action 2 (type) needs text",
            ),
            (
                serde_json::json!({ "kind": "key", "key": " " }),
                "action 2 (key) needs a key",
            ),
            (
                serde_json::json!({ "kind": "scroll" }),
                "action 2 (scroll) needs a direction",
            ),
            (
                serde_json::json!({ "kind": "focus", "application": " " }),
                "action 2 (focus) needs",
            ),
        ] {
            let error = parse(serde_json::json!([{ "kind": "wait" }, action])).unwrap_err();
            assert!(error.to_string().starts_with(expected), "{error}");
        }
        assert!(parse(serde_json::json!([])).is_err());
    }

    #[test]
    fn unknown_kinds_do_not_deserialize() {
        let error = serde_json::from_value::<RawAction>(serde_json::json!({ "kind": "tap" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected one of"), "{error}");
    }

    #[test]
    fn typed_text_is_capped_across_the_batch() {
        let half = "é".repeat(MAX_TYPE_CHARS / 2 + 1);
        let error = parse(serde_json::json!([
            { "kind": "type", "text": half },
            { "kind": "type", "text": half },
        ]))
        .unwrap_err();
        assert!(error.to_string().contains("types"), "{error}");
    }

    #[test]
    fn time_budget_covers_waits_settle_and_typing() {
        let request = ActRequest::parse(
            &raw(serde_json::json!([
                { "kind": "wait", "ms": 1000 },
                { "kind": "type", "text": "abcde" },
            ])),
            None,
            Some(500.0),
        )
        .unwrap();
        assert_eq!(
            request.time_budget(),
            Duration::from_millis(30_000 + 500 + 1000 + 5 * 50)
        );
    }

    #[test]
    fn actions_survive_the_wire_round_trip() {
        let request = parse(serde_json::json!([
            { "kind": "down", "button": "middle" },
            { "kind": "key", "key": "c", "modifiers": ["ctrl"] },
            { "kind": "focus", "application": "xterm" },
        ]))
        .unwrap();
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<ActRequest>(&json).unwrap(), request);
    }

    #[test]
    fn typing_budget_covers_a_binding_pause_for_every_character() {
        let chars = MAX_TYPE_CHARS;
        let request = ActRequest {
            actions: vec![Action::Type {
                text: "あ".repeat(chars),
            }],
            observe: false,
            settle_ms: 0,
        };
        let per_char = KEYMAP_SETTLE * 2 + Duration::from_millis(10);
        assert!(request.time_budget() >= WORK_ALLOWANCE + per_char * u32::try_from(chars).unwrap());
    }
}
