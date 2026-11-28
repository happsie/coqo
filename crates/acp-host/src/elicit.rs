// SPDX-License-Identifier: Apache-2.0

//! ACP form elicitations (`elicitation/create`, mode `form`) as questions
//! in the chat (#339, ADR 0029).
//!
//! The Claude adapter asks Claude Code's `AskUserQuestion` this way (and
//! disables the tool for a client that does not advertise
//! `elicitation.form`); MCP servers' elicitations come the same way. A form
//! is a flat JSON Schema object; each property becomes one
//! [`m::AgentQuestion`]:
//!
//! | Property | Question |
//! |---|---|
//! | `string` with `oneOf` / `enum` | `single`, one option per value |
//! | `array` of `anyOf` / `enum` items | `multi` |
//! | `boolean` | `single`: Yes / No |
//! | `string` | `text` |
//! | `number`, `integer` | `number` |
//!
//! A `string` property `<key>_custom` next to the select `<key>` (the
//! adapter's per-question "Other" box) is folded into that question as its
//! typed answer (`other: true`). An option's `_meta.*.preview` (the
//! adapter's `_claude/askUserQuestionOption.preview`) is its preview.
//!
//! The answer goes back in the shape the schema asks for: the picked
//! value (a string, or `true`/`false`), an array of values, the typed text
//! (in `<key>_custom` for "Other"), or a number. Skipping is `decline` (the
//! adapter tells Claude the user skipped), anything else `cancel`.

use std::collections::BTreeMap;

use agent_client_protocol::v1::schema as acp;
use coqo_agents::PermissionAsk;
use coqo_agents::model as m;
use serde_json::{Map, Value};

/// The suffix of a select's "Other" companion field.
const OTHER_SUFFIX: &str = "_custom";

/// How a field's answer is written back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    /// A string: a picked value or typed text.
    String,
    /// An array of picked values.
    Array,
    /// A picked `true` / `false`.
    Boolean,
    Number,
    Integer,
}

#[derive(Clone, Debug)]
struct Field {
    key: String,
    wire: Wire,
    /// The "Other" companion field's key.
    other: Option<String>,
}

/// A form elicitation, as questions, and how to answer it.
#[derive(Clone, Debug)]
pub(crate) struct Form {
    /// What the session manager asks.
    pub ask: PermissionAsk,
    fields: Vec<Field>,
}

/// Where a form elicitation goes.
pub(crate) struct Target {
    pub session: acp::SessionId,
    pub tool_call: Option<acp::ToolCallId>,
}

/// The session (and tool call) a request is scoped to, if it is a form
/// for a session.
pub(crate) fn target(req: &acp::CreateElicitationRequest) -> Option<Target> {
    let acp::ElicitationMode::Form(form) = &req.mode else {
        return None;
    };
    match &form.scope {
        acp::ElicitationScope::Session(s) => Some(Target {
            session: s.session_id.clone(),
            tool_call: s.tool_call_id.clone(),
        }),
        _ => None,
    }
}

/// `question_10` after `question_9`: keys ordered with their numbers as
/// numbers.
fn natural_key(key: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    for c in key.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            if !digits.is_empty() {
                parts.push((
                    std::mem::take(&mut text),
                    digits.parse().unwrap_or(u64::MAX),
                ));
                digits.clear();
            }
            text.push(c);
        }
    }
    parts.push((text, digits.parse().unwrap_or(0)));
    parts
}

/// The preview an option carries in its `_meta`: a `preview` string at
/// the top or one level down.
fn preview(meta: Option<&Map<String, Value>>) -> Option<String> {
    let meta = meta?;
    let of = |m: &Map<String, Value>| m.get("preview")?.as_str().map(str::to_owned);
    of(meta).or_else(|| meta.values().filter_map(Value::as_object).find_map(of))
}

fn choice(o: &acp::EnumOption) -> m::QuestionChoice {
    m::QuestionChoice {
        value: o.value.clone(),
        label: if o.title.trim().is_empty() {
            o.value.clone()
        } else {
            o.title.clone()
        },
        description: o.description.clone().filter(|d| !d.trim().is_empty()),
        preview: preview(o.meta.as_ref()),
    }
}

fn plain(values: &[String]) -> Vec<m::QuestionChoice> {
    values
        .iter()
        .map(|v| m::QuestionChoice {
            value: v.clone(),
            label: v.clone(),
            description: None,
            preview: None,
        })
        .collect()
}

/// A property as a question, before its text is chosen.
struct Shape {
    wire: Wire,
    kind: m::AgentQuestionKind,
    options: Vec<m::QuestionChoice>,
    title: Option<String>,
    description: Option<String>,
}

/// One property as a question (without its text), or `None` if Coqo
/// can't show it.
fn shape(schema: &acp::ElicitationPropertySchema) -> Option<Shape> {
    use m::AgentQuestionKind as K;
    let (wire, kind, options, title, description) = match schema {
        acp::ElicitationPropertySchema::String(s) => {
            let options = match (&s.one_of, &s.enum_values) {
                (Some(one_of), _) if !one_of.is_empty() => one_of.iter().map(choice).collect(),
                (_, Some(values)) if !values.is_empty() => plain(values),
                _ => Vec::new(),
            };
            let kind = if options.is_empty() {
                K::Text
            } else {
                K::Single
            };
            (
                Wire::String,
                kind,
                options,
                s.title.clone(),
                s.description.clone(),
            )
        }
        acp::ElicitationPropertySchema::Array(a) => {
            let options = match &a.items {
                acp::MultiSelectItems::String(items) => plain(&items.values),
                acp::MultiSelectItems::Titled(items) => items.options.iter().map(choice).collect(),
                _ => return None,
            };
            if options.is_empty() {
                return None;
            }
            (
                Wire::Array,
                K::Multi,
                options,
                a.title.clone(),
                a.description.clone(),
            )
        }
        acp::ElicitationPropertySchema::Boolean(b) => (
            Wire::Boolean,
            K::Single,
            vec![
                m::QuestionChoice {
                    value: "true".into(),
                    label: "Yes".into(),
                    description: None,
                    preview: None,
                },
                m::QuestionChoice {
                    value: "false".into(),
                    label: "No".into(),
                    description: None,
                    preview: None,
                },
            ],
            b.title.clone(),
            b.description.clone(),
        ),
        acp::ElicitationPropertySchema::Number(n) => (
            Wire::Number,
            K::Number,
            Vec::new(),
            n.title.clone(),
            n.description.clone(),
        ),
        acp::ElicitationPropertySchema::Integer(n) => (
            Wire::Integer,
            K::Number,
            Vec::new(),
            n.title.clone(),
            n.description.clone(),
        ),
        _ => return None,
    };
    Some(Shape {
        wire,
        kind,
        options,
        title,
        description,
    })
}

fn is_free_text(schema: &acp::ElicitationPropertySchema) -> bool {
    matches!(schema, acp::ElicitationPropertySchema::String(s)
        if s.one_of.as_ref().is_none_or(Vec::is_empty)
            && s.enum_values.as_ref().is_none_or(Vec::is_empty))
}

/// `s` unchanged, unless it is blank. Question text and labels are never
/// rewritten: skills parse markers in them (ADR 0030).
fn nonempty(s: Option<&String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty()).cloned()
}

/// A form elicitation as questions. An error (for the agent) when it is no
/// form, or has nothing Coqo can show, or a required field Coqo can't
/// show.
pub(crate) fn form(
    req: &acp::CreateElicitationRequest,
    tool_call: Option<&acp::ToolCallId>,
) -> Result<Form, String> {
    let acp::ElicitationMode::Form(form) = &req.mode else {
        return Err("Coqo shows only form elicitations".into());
    };
    let schema = &form.requested_schema;
    let required = schema.required.clone().unwrap_or_default();
    let mut keys: Vec<&String> = schema.properties.keys().collect();
    keys.sort_by_key(|k| natural_key(k));
    // The "Other" companions of selects.
    let companions: BTreeMap<&str, &str> = keys
        .iter()
        .filter_map(|k| {
            let base = k.strip_suffix(OTHER_SUFFIX)?;
            let base_schema = schema.properties.get(base)?;
            let select = shape(base_schema).is_some_and(|s| {
                matches!(
                    s.kind,
                    m::AgentQuestionKind::Single | m::AgentQuestionKind::Multi
                )
            }) && !matches!(base_schema, acp::ElicitationPropertySchema::Boolean(_));
            (select && is_free_text(&schema.properties[k.as_str()])).then_some((base, k.as_str()))
        })
        .collect();
    let folded: Vec<&str> = companions.values().copied().collect();
    let mut shaped = Vec::new();
    for key in keys {
        if folded.contains(&key.as_str()) {
            continue;
        }
        let Some(shape) = shape(&schema.properties[key]) else {
            if required.contains(key) {
                return Err(format!("Coqo can't show the required field {key:?}"));
            }
            tracing::warn!(field = %key, "leaving out an elicitation field Coqo can't show");
            continue;
        };
        shaped.push((key, shape));
    }
    if shaped.is_empty() {
        return Err("the form has no field Coqo can show".into());
    }
    let message = req.message.clone();
    let single = shaped.len() == 1;
    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for (key, shape) in shaped {
        let Shape {
            wire,
            kind,
            options,
            title,
            description,
        } = shape;
        let (title, description) = (nonempty(title.as_ref()), nonempty(description.as_ref()));
        // The adapter puts a lone question in the message, and its header
        // in the title.
        let question = description
            .clone()
            .or_else(|| (single && !message.trim().is_empty()).then(|| message.clone()))
            .or_else(|| title.clone())
            .unwrap_or_else(|| key.clone());
        let header = title.filter(|t| *t != question);
        let other = companions.get(key.as_str()).map(|k| (*k).to_owned());
        questions.push(m::AgentQuestion {
            id: key.clone(),
            header,
            question,
            kind,
            options,
            other: other.is_some(),
            required: required.contains(key),
            anchor: None,
        });
        fields.push(Field {
            key: key.clone(),
            wire,
            other,
        });
    }
    let title = if message.is_empty() {
        questions[0].question.clone()
    } else {
        message
    };
    Ok(Form {
        ask: PermissionAsk {
            title,
            tool_call: tool_call.map(|id| m::ToolCallUpdate {
                id: m::ToolCallId(id.0.to_string()),
                ..m::ToolCallUpdate::default()
            }),
            options: Vec::new(),
            questions,
        },
        fields,
    })
}

/// The answer to a form, for the agent.
pub(crate) fn response(
    form: &Form,
    outcome: m::PermissionOutcome,
) -> acp::CreateElicitationResponse {
    let action = match outcome {
        m::PermissionOutcome::Answered { answers } => {
            let mut content = BTreeMap::new();
            for field in &form.fields {
                let Some(answer) = answers.iter().find(|a| a.id == field.key) else {
                    continue;
                };
                let text = answer
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                let value: Option<acp::ElicitationContentValue> = match field.wire {
                    Wire::String => answer
                        .picks
                        .first()
                        .cloned()
                        .or_else(|| field.other.is_none().then(|| text.map(str::to_owned))?)
                        .map(Into::into),
                    Wire::Array => (!answer.picks.is_empty()).then(|| answer.picks.clone().into()),
                    Wire::Boolean => answer.picks.first().map(|p| (p == "true").into()),
                    Wire::Number => text.and_then(|t| t.parse::<f64>().ok()).map(Into::into),
                    Wire::Integer => text.and_then(|t| t.parse::<i64>().ok()).map(Into::into),
                };
                if let Some(value) = value {
                    content.insert(field.key.clone(), value);
                }
                if let (Some(other), Some(text)) = (&field.other, text) {
                    content.insert(other.clone(), text.into());
                }
            }
            acp::ElicitationAction::Accept(acp::ElicitationAcceptAction::new().content(content))
        }
        m::PermissionOutcome::Declined => acp::ElicitationAction::Decline,
        m::PermissionOutcome::Selected { .. } | m::PermissionOutcome::Cancelled => {
            acp::ElicitationAction::Cancel
        }
    };
    acp::CreateElicitationResponse::new(action)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// What claude-agent-acp 0.113.0 sends for `AskUserQuestion`
    /// (`askUserQuestionsToCreateRequest`): two questions, the second a
    /// multi-select, each with its "Other" box.
    fn ask_user_question() -> acp::CreateElicitationRequest {
        serde_json::from_value(json!({
            "mode": "form",
            "sessionId": "s1",
            "toolCallId": "toolu_1",
            "message": "Please answer the following questions.",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {
                        "type": "string", "title": "Colour", "description": "Which colour?",
                        "oneOf": [
                            {"const": "Red", "title": "Red", "description": "Warm",
                             "_meta": {"_claude/askUserQuestionOption": {"preview": "#f00"}}},
                            {"const": "Blue", "title": "Blue"}
                        ]
                    },
                    "question_0_custom": {"type": "string", "title": "Other",
                        "description": "Type your own answer, or add a note to the option you chose above (optional)."},
                    "question_1": {
                        "type": "array", "title": "Pets", "description": "Which pets?",
                        "items": {"anyOf": [{"const": "Cat", "title": "Cat"}, {"const": "Dog", "title": "Dog"}]}
                    },
                    "question_1_custom": {"type": "string", "title": "Other"}
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn ask_user_question_becomes_questions_with_other() {
        let req = ask_user_question();
        let target = target(&req).unwrap();
        assert_eq!(&*target.session.0, "s1");
        let form = form(&req, target.tool_call.as_ref()).unwrap();
        let qs = &form.ask.questions;
        assert_eq!(qs.len(), 2, "{qs:?}");
        assert_eq!(form.ask.title, "Please answer the following questions.");
        assert_eq!(form.ask.tool_call.as_ref().unwrap().id.0, "toolu_1");
        assert_eq!(qs[0].id, "question_0");
        assert_eq!(qs[0].question, "Which colour?");
        assert_eq!(qs[0].header.as_deref(), Some("Colour"));
        assert_eq!(qs[0].kind, m::AgentQuestionKind::Single);
        assert!(qs[0].other);
        assert_eq!(qs[0].options[0].description.as_deref(), Some("Warm"));
        assert_eq!(qs[0].options[0].preview.as_deref(), Some("#f00"));
        assert_eq!(qs[1].kind, m::AgentQuestionKind::Multi);
        assert_eq!(qs[1].options.len(), 2);
    }

    #[test]
    fn a_lone_question_takes_the_message() {
        let req: acp::CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "form", "sessionId": "s1", "message": "Which colour do you like?",
            "requestedSchema": {"type": "object", "properties": {
                "question_0": {"type": "string", "title": "Colour",
                    "oneOf": [{"const": "Red", "title": "Red"}]},
                "question_0_custom": {"type": "string", "title": "Other"}
            }}
        }))
        .unwrap();
        let form = form(&req, None).unwrap();
        assert_eq!(form.ask.questions.len(), 1);
        assert_eq!(form.ask.questions[0].question, "Which colour do you like?");
        assert_eq!(form.ask.questions[0].header.as_deref(), Some("Colour"));
    }

    #[test]
    fn question_text_and_labels_pass_through_unchanged() {
        // gstack's markers and `(recommended)` suffix, which its own hooks
        // parse (ADR 0030): shown and answered exactly as sent.
        let text = " <gstack-qid:abc123> Ship it now?\n";
        let label = "Ship (recommended)";
        let req: acp::CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "form", "sessionId": "s1", "message": text,
            "requestedSchema": {"type": "object", "properties": {
                "question_0": {"type": "string", "title": "Ship",
                    "oneOf": [{"const": label, "title": label}, {"const": "Wait", "title": "Wait"}]}
            }}
        }))
        .unwrap();
        let form = form(&req, None).unwrap();
        let q = &form.ask.questions[0];
        assert_eq!(q.question, text);
        assert_eq!(q.options[0].label, label);
        let answered = response(
            &form,
            m::PermissionOutcome::Answered {
                answers: vec![m::QuestionAnswer {
                    id: "question_0".into(),
                    picks: vec![q.options[0].value.clone()],
                    text: None,
                }],
            },
        );
        assert_eq!(content(&answered)["content"]["question_0"], label);
    }

    fn content(r: &acp::CreateElicitationResponse) -> Value {
        serde_json::to_value(r).unwrap()
    }

    #[test]
    fn answers_go_back_in_the_schema_shape() {
        let form = form(&ask_user_question(), None).unwrap();
        let answered = response(
            &form,
            m::PermissionOutcome::Answered {
                answers: vec![
                    m::QuestionAnswer {
                        id: "question_0".into(),
                        picks: vec![],
                        text: Some("Green".into()),
                    },
                    m::QuestionAnswer {
                        id: "question_1".into(),
                        picks: vec!["Cat".into(), "Dog".into()],
                        text: Some("Fish".into()),
                    },
                ],
            },
        );
        assert_eq!(
            content(&answered),
            json!({"action": "accept", "content": {
                "question_0_custom": "Green",
                "question_1": ["Cat", "Dog"],
                "question_1_custom": "Fish",
            }})
        );
        assert_eq!(
            content(&response(&form, m::PermissionOutcome::Declined)),
            json!({"action": "decline"})
        );
        assert_eq!(
            content(&response(&form, m::PermissionOutcome::Cancelled)),
            json!({"action": "cancel"})
        );
    }

    #[test]
    fn plain_fields_become_text_number_and_yes_no() {
        let req: acp::CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "form", "sessionId": "s1", "message": "Configure the server",
            "requestedSchema": {"type": "object", "required": ["name"], "properties": {
                "name": {"type": "string", "title": "Name"},
                "port": {"type": "integer", "title": "Port"},
                "tls": {"type": "boolean", "title": "Use TLS"}
            }}
        }))
        .unwrap();
        let form = form(&req, None).unwrap();
        let kinds: Vec<_> = form.ask.questions.iter().map(|q| q.kind).collect();
        assert_eq!(
            kinds,
            [
                m::AgentQuestionKind::Text,
                m::AgentQuestionKind::Number,
                m::AgentQuestionKind::Single
            ]
        );
        assert!(form.ask.questions[0].required);
        let answer = |id: &str, picks: &[&str], text: Option<&str>| m::QuestionAnswer {
            id: id.into(),
            picks: picks.iter().map(|p| (*p).to_owned()).collect(),
            text: text.map(Into::into),
        };
        let r = response(
            &form,
            m::PermissionOutcome::Answered {
                answers: vec![
                    answer("name", &[], Some("api")),
                    answer("port", &[], Some("8080")),
                    answer("tls", &["true"], None),
                ],
            },
        );
        assert_eq!(
            content(&r),
            json!({"action": "accept", "content": {"name": "api", "port": 8080, "tls": true}})
        );
    }

    #[test]
    fn a_form_coqo_cannot_show_is_an_error() {
        let req: acp::CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "form", "sessionId": "s1", "message": "?",
            "requestedSchema": {"type": "object", "properties": {}}
        }))
        .unwrap();
        assert!(form(&req, None).is_err());
        let url: acp::CreateElicitationRequest = serde_json::from_value(json!({
            "mode": "url", "sessionId": "s1", "message": "Sign in",
            "elicitationId": "e1", "url": "https://example.com"
        }))
        .unwrap();
        assert!(target(&url).is_none());
        assert!(form(&url, None).is_err());
    }
}
