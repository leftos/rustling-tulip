//! The driver's wire format: one JSON request in, one JSON reply out.
//!
//! A request is `{ "id": u64, "method": string, "params": {...} }`; its reply
//! carries the same id and either `"result"` or `"error"`, an error naming a
//! [`ErrorCode`], a message and, where one helps, a hint.

use gpui::Keystroke;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Every method a request may name, sorted.
pub const METHODS: [&str; 15] = [
    "action",
    "click",
    "drag",
    "find",
    "key",
    "move",
    "pane_text",
    "panic",
    "paste",
    "ping",
    "screenshot",
    "state",
    "type_text",
    "wait_for",
    "wheel",
];

/// One call from a driver client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The client's tag for the call, echoed in its reply. Id 0 is reserved
    /// for replies to a request with no usable id; a client must not use it.
    pub id: u64,
    pub method: String,
    /// The method's arguments; absent or `null` reads as `{}`.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Decodes one request. Text that is not a request is answered with
    /// `bad_params` under its `id` when that is a u64, else under the
    /// reserved id 0.
    ///
    /// # Errors
    ///
    /// The `bad_params` reply when `text` is not JSON, or lacks a u64 `id`
    /// or a string `method`.
    pub fn decode(text: &str) -> Result<Self, Reply> {
        let bad = |id: u64, err: serde_json::Error| {
            Reply::error(
                id,
                ErrorCode::BadParams,
                format!("not a driver request: {err}"),
                Some(r#"send {"id": <u64>, "method": <string>, "params": {...}}"#.to_owned()),
            )
        };
        let value: Value = serde_json::from_str(text).map_err(|err| bad(0, err))?;
        let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
        serde_json::from_value(value).map_err(|err| bad(id, err))
    }

    /// The request's params decoded as `T`, absent or `null` params as `{}`.
    ///
    /// # Errors
    ///
    /// The `bad_params` reply naming what serde could not decode.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, Reply> {
        let params = if self.params.is_null() {
            Value::Object(Map::new())
        } else {
            self.params.clone()
        };
        serde_json::from_value(params).map_err(|err| {
            self.fail(
                ErrorCode::BadParams,
                format!("bad params for {}: {err}", self.method),
            )
        })
    }

    /// The error reply to this request, without a hint.
    #[must_use]
    pub fn fail(&self, code: ErrorCode, message: String) -> Reply {
        Reply::error(self.id, code, message, None)
    }
}

/// The answer to one [`Request`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// What a request came to: its result, or why it failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Result(Value),
    Error(Failure),
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
    /// What to try instead, when there is something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The kinds of failure a reply names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The method is not one of [`METHODS`].
    UnknownMethod,
    /// The request, or its params, could not be read.
    BadParams,
    /// No element by that name in the window's latest frame.
    NotFound,
    /// The element was painted, but a clipping ancestor hides all of it.
    NotVisible,
    /// The driver will not do what was asked.
    Refused,
}

impl Reply {
    /// The successful reply to request `id`.
    #[must_use]
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            outcome: Outcome::Result(result),
        }
    }

    /// The failed reply to request `id`.
    #[must_use]
    pub fn error(id: u64, code: ErrorCode, message: String, hint: Option<String>) -> Self {
        Self {
            id,
            outcome: Outcome::Error(Failure {
                code,
                message,
                hint,
            }),
        }
    }

    /// The reply to request `id` naming a method there is none of, with the
    /// methods there are as its hint.
    #[must_use]
    pub fn unknown_method(id: u64, method: &str) -> Self {
        Self::error(
            id,
            ErrorCode::UnknownMethod,
            format!("unknown method: {method}"),
            Some(format!("methods: {}", METHODS.join(", "))),
        )
    }
}

/// `find`'s params: the names to list start with `prefix`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    #[serde(default)]
    pub prefix: Option<String>,
}

/// `type_text`'s params: the printable text to type into what holds focus.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeTextParams {
    pub text: String,
}

/// `paste`'s params: the text to paste into what holds focus, newlines
/// included.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasteParams {
    pub text: String,
}

/// `pane_text`'s params: the pane to read, else the active tab's focused
/// pane.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneTextParams {
    #[serde(default)]
    pub pane: Option<String>,
}

/// `action`'s params: the named action and its arguments, when it takes
/// any.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionParams {
    pub name: String,
    #[serde(default)]
    pub args: Option<Value>,
}

/// `wait_for`'s params: the predicate to wait on, how long to wait at
/// most and how often to read it, in milliseconds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitParams {
    pub until: Value,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

/// Which mouse button a `click` presses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
}

/// Where a pointer method aims: a point in logical window pixels, or the
/// centre of a registry element's visible part.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Point { x: f32, y: f32 },
    Element(String),
}

/// The target a request writes as `x` and `y`, or as `element`: exactly
/// one of the two, and a point's coordinates finite.
fn target(x: Option<f32>, y: Option<f32>, element: Option<&str>) -> Result<Target, ParamError> {
    let refused = |message: &str| ParamError {
        message: message.to_owned(),
        hint: Some(r#"aim with {"x": <px>, "y": <px>} or {"element": "<name>"}"#.to_owned()),
    };
    match (x, y, element) {
        (None, None, Some(name)) => Ok(Target::Element(name.to_owned())),
        (Some(x), Some(y), None) if x.is_finite() && y.is_finite() => Ok(Target::Point { x, y }),
        (Some(_), Some(_), None) => Err(refused("x and y must be finite numbers")),
        (None, None, None) => Err(refused("give a point or an element to aim at")),
        (_, _, Some(_)) => Err(refused("give a point or an element, not both")),
        _ => Err(refused("a point needs both x and y")),
    }
}

/// A target object: `drag`'s `from` and `to`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetParams {
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub element: Option<String>,
}

impl TargetParams {
    /// The target these params write.
    ///
    /// # Errors
    ///
    /// What is wrong when they write neither a point nor an element, or
    /// both, or a point that is not finite.
    pub fn target(&self) -> Result<Target, ParamError> {
        target(self.x, self.y, self.element.as_deref())
    }
}

/// `click`'s params: the target, the button (left by default) and how many
/// clicks, 1 or 2. `modifiers` is read only to refuse the request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickParams {
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub element: Option<String>,
    #[serde(default)]
    pub button: Option<Button>,
    #[serde(default)]
    pub count: Option<u8>,
    #[serde(default)]
    pub modifiers: Option<Value>,
}

impl ClickParams {
    /// The target, the button and the click count these params write.
    ///
    /// # Errors
    ///
    /// What is wrong with the target, or a count other than 1 or 2.
    pub fn read(&self) -> Result<(Target, Button, u8), ParamError> {
        let target = target(self.x, self.y, self.element.as_deref())?;
        let count = self.count.unwrap_or(1);
        if !(1..=2).contains(&count) {
            return Err(ParamError {
                message: format!("count is {count}; a click counts 1 or 2"),
                hint: None,
            });
        }
        Ok((target, self.button.unwrap_or_default(), count))
    }
}

/// `move`'s params: the target. `modifiers` is read only to refuse the
/// request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveParams {
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub element: Option<String>,
    #[serde(default)]
    pub modifiers: Option<Value>,
}

impl MoveParams {
    /// The target these params write.
    ///
    /// # Errors
    ///
    /// What is wrong with the target.
    pub fn target(&self) -> Result<Target, ParamError> {
        target(self.x, self.y, self.element.as_deref())
    }
}

/// The fewest and most moves a `drag` takes between its press and its
/// release, and how many it takes by default.
pub const DRAG_STEPS: std::ops::RangeInclusive<u8> = 2..=64;
const DEFAULT_DRAG_STEPS: u8 = 8;

/// `drag`'s params: where the left button goes down and where it comes up,
/// and how many moves lie between. `modifiers` is read only to refuse the
/// request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DragParams {
    pub from: TargetParams,
    pub to: TargetParams,
    #[serde(default)]
    pub steps: Option<u8>,
    #[serde(default)]
    pub modifiers: Option<Value>,
}

impl DragParams {
    /// The two targets and the step count these params write.
    ///
    /// # Errors
    ///
    /// What is wrong with either target, or a step count outside
    /// [`DRAG_STEPS`].
    pub fn read(&self) -> Result<(Target, Target, u8), ParamError> {
        let from = self.from.target().map_err(|refused| ParamError {
            message: format!("from: {}", refused.message),
            hint: refused.hint,
        })?;
        let to = self.to.target().map_err(|refused| ParamError {
            message: format!("to: {}", refused.message),
            hint: refused.hint,
        })?;
        let steps = self.steps.unwrap_or(DEFAULT_DRAG_STEPS);
        if !DRAG_STEPS.contains(&steps) {
            return Err(ParamError {
                message: format!(
                    "steps is {steps}; a drag takes {} to {} steps",
                    DRAG_STEPS.start(),
                    DRAG_STEPS.end()
                ),
                hint: None,
            });
        }
        Ok((from, to, steps))
    }
}

/// The most notches one `wheel` turns either way: its delta, 120 a notch,
/// must fit the message's signed 16-bit word.
pub const MAX_NOTCHES: i32 = 273;

/// `wheel`'s params: the target and the notches to turn, positive up (away
/// from the user). `modifiers` is read only to refuse the request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WheelParams {
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub element: Option<String>,
    pub notches: i32,
    #[serde(default)]
    pub modifiers: Option<Value>,
}

impl WheelParams {
    /// The target and the notches these params write.
    ///
    /// # Errors
    ///
    /// What is wrong with the target, or notches that are zero or past
    /// [`MAX_NOTCHES`] either way.
    pub fn read(&self) -> Result<(Target, i32), ParamError> {
        let target = target(self.x, self.y, self.element.as_deref())?;
        if self.notches == 0 || self.notches.unsigned_abs() > MAX_NOTCHES.unsigned_abs() {
            return Err(ParamError {
                message: format!(
                    "notches is {}; turn 1 to {MAX_NOTCHES} notches, positive up, negative down",
                    self.notches
                ),
                hint: None,
            });
        }
        Ok((target, self.notches))
    }
}

/// Why a request's params were refused, as its `bad_params` reply says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamError {
    pub message: String,
    pub hint: Option<String>,
}

/// `key`'s params: one keystroke, as gpui's keymap writes it.
///
/// One per request: gpui runs the events, observers and deferred work a
/// keystroke causes only when the update it was dispatched in ends, so a
/// second keystroke in the same update would land before the first one's
/// effects (a dialog it closes, say).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyParams {
    pub keys: String,
}

impl KeyParams {
    /// The keystroke `keys` names.
    ///
    /// # Errors
    ///
    /// What is wrong when `keys` names none or more than one, or one that
    /// does not parse or has modifiers but no key.
    pub fn keystroke(&self) -> Result<Keystroke, ParamError> {
        let sources: Vec<&str> = self.keys.split_whitespace().collect();
        let source = match sources.as_slice() {
            [source] => *source,
            [] => {
                return Err(ParamError {
                    message: "keys names no keystroke".to_owned(),
                    hint: Some(r#"write a keystroke like "ctrl-,""#.to_owned()),
                });
            }
            _ => {
                return Err(ParamError {
                    message: "send one keystroke per request".to_owned(),
                    hint: Some("send ctrl-k, then ctrl-s, as two requests".to_owned()),
                });
            }
        };
        match Keystroke::parse(source) {
            Ok(stroke) if !stroke.key.is_empty() => Ok(stroke),
            _ => Err(ParamError {
                message: format!(r#"not a keystroke: "{source}""#),
                hint: Some(r#"write a keystroke like "ctrl-,""#.to_owned()),
            }),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use serde_json::{Value, json};

    use super::{ErrorCode, Failure, FindParams, KeyParams, Outcome, Reply, Request, WheelParams};

    fn request(text: &str) -> Request {
        Request::decode(text).expect("a request")
    }

    fn failure(reply: Reply) -> Failure {
        match reply.outcome {
            Outcome::Error(failure) => Some(failure),
            Outcome::Result(_) => None,
        }
        .expect("an error reply")
    }

    #[test]
    fn a_request_round_trips() {
        let decoded = request(r#"{"id":1,"method":"ping","params":{}}"#);
        assert_eq!(
            decoded,
            Request {
                id: 1,
                method: "ping".to_owned(),
                params: json!({}),
            }
        );
        let text = serde_json::to_string(&decoded).expect("encodes");
        assert_eq!(request(&text), decoded);
    }

    #[test]
    fn a_result_reply_has_the_id_and_result() {
        let reply = Reply::ok(1, json!({"version": "0.7.0-alpha"}));
        let text = serde_json::to_string(&reply).expect("encodes");
        assert_eq!(text, r#"{"id":1,"result":{"version":"0.7.0-alpha"}}"#);
        let back: Reply = serde_json::from_str(&text).expect("decodes");
        assert_eq!(back, reply);
    }

    #[test]
    fn an_unknown_method_reply_lists_the_methods() {
        let reply = Reply::unknown_method(2, "nope");
        assert_eq!(
            serde_json::to_value(&reply).expect("encodes"),
            json!({
                "id": 2,
                "error": {
                    "code": "unknown_method",
                    "message": "unknown method: nope",
                    "hint": "methods: action, click, drag, find, key, move, pane_text, panic, paste, ping, screenshot, state, type_text, wait_for, wheel",
                },
            })
        );
        let text = serde_json::to_string(&reply).expect("encodes");
        let back: Reply = serde_json::from_str(&text).expect("decodes");
        assert_eq!(back, reply);
    }

    #[test]
    fn an_error_without_a_hint_leaves_it_out() {
        let reply = Reply::error(3, ErrorCode::NotFound, "no x".to_owned(), None);
        assert_eq!(
            serde_json::to_value(&reply).expect("encodes"),
            json!({"id": 3, "error": {"code": "not_found", "message": "no x"}})
        );
        let codes = [
            (ErrorCode::BadParams, "bad_params"),
            (ErrorCode::NotVisible, "not_visible"),
            (ErrorCode::Refused, "refused"),
        ];
        for (code, name) in codes {
            assert_eq!(serde_json::to_value(code).expect("encodes"), json!(name));
        }
    }

    #[test]
    fn missing_params_read_as_an_empty_object() {
        let decoded = request(r#"{"id":4,"method":"find"}"#);
        assert_eq!(decoded.params, Value::Null);
        let params: FindParams = decoded.params().expect("empty params decode");
        assert_eq!(params.prefix, None);
    }

    #[test]
    fn a_bad_request_is_answered_under_its_id_when_it_has_one() {
        for (text, id) in [
            (r#"{"id":5}"#, 5),
            (r#"{"id":5,"method":7}"#, 5),
            (r#"{"id":5,"params":{}}"#, 5),
            (r#"{"method":"ping"}"#, 0),
            (r#"{"id":-1,"method":"ping"}"#, 0),
            ("not json", 0),
        ] {
            let reply = Request::decode(text).expect_err("not a request");
            assert_eq!(reply.id, id, "{text}");
            assert_eq!(failure(reply).code, ErrorCode::BadParams, "{text}");
        }
    }

    #[test]
    fn a_key_names_one_keystroke() {
        let one = KeyParams {
            keys: " ctrl-, ".to_owned(),
        };
        let stroke = one.keystroke().expect("one keystroke");
        assert_eq!(stroke.key, ",");
        assert!(stroke.modifiers.control);
    }

    #[test]
    fn two_keystrokes_in_one_request_are_refused() {
        let two = KeyParams {
            keys: "ctrl-k ctrl-s".to_owned(),
        };
        let refused = two.keystroke().expect_err("two keystrokes are refused");
        assert_eq!(refused.message, "send one keystroke per request");
        assert_eq!(
            refused.hint.as_deref(),
            Some("send ctrl-k, then ctrl-s, as two requests")
        );
    }

    #[test]
    fn wheel_refuses_i32_min_notches() {
        let params = WheelParams {
            x: None,
            y: None,
            element: Some("pane-grid-p1".to_owned()),
            notches: i32::MIN,
            modifiers: None,
        };
        let refused = params
            .read()
            .expect_err("i32::MIN notches are refused before anything is posted");
        assert!(
            refused.message.contains("notches is"),
            "{}",
            refused.message
        );
    }

    #[test]
    fn empty_or_keyless_keys_are_refused() {
        for keys in ["", "  ", "ctrl-"] {
            let params = KeyParams {
                keys: keys.to_owned(),
            };
            assert!(params.keystroke().is_err(), "{keys:?} is refused");
        }
    }
}
