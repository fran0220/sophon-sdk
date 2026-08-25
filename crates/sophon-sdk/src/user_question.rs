// Copyright 2026 OriginGame contributors
// Licensed under the Apache License, Version 2.0.

//! Typed product-UI seam for the native agent's non-blocking questions.
//!
//! Calling [`UserQuestionUi::present`] waits for the person's response, but it
//! does not pause the agent loop: the Session coordinator owns that wait and
//! consumes an accepted response only at a later model-step boundary. The
//! Session event stream remains the authority for whether a submitted answer
//! was actually consumed (`Answered`) or ultimately closed (`Unanswered`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The reverse request method is reserved to this typed seam. It may not be
/// installed as a generic Host extension method.
pub(crate) const USER_QUESTION_METHOD: &str = "x.ai/ask_user_question";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserQuestionMode {
    Default,
    Plan,
}

/// One choice in a question. Identities are SDK-issued and opaque to the Host;
/// labels and supporting content remain exactly what the agent supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestionOption {
    pub id: String,
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestion {
    pub id: String,
    pub prompt: String,
    pub options: Vec<UserQuestionOption>,
    pub multi_select: bool,
}

/// One native question form, already bound to the Session that opened it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestionRequest {
    pub session_id: String,
    pub request_id: String,
    pub questions: Vec<UserQuestion>,
    pub mode: UserQuestionMode,
}

/// One question's person-authored response.
///
/// `option_ids` names choices from the corresponding request. `other` is the
/// free-text alternative and may accompany choices for a multi-select form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestionAnswer {
    pub question_id: String,
    pub option_ids: Vec<String>,
    pub other: Option<String>,
}

/// The four outcomes supported by the native agent question protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserQuestionResponse {
    Answered(Vec<UserQuestionAnswer>),
    /// Continue the plan interview in ordinary conversation, preserving any
    /// answers already entered in the form.
    ChatAboutThis(Vec<UserQuestionAnswer>),
    /// Finish the plan interview with the partial answers already entered.
    SkipInterview(Vec<UserQuestionAnswer>),
    Dismissed,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("user question UI failed: {message}")]
pub struct UserQuestionUiError {
    pub message: String,
}

#[async_trait::async_trait]
pub trait UserQuestionUi: Send + Sync + 'static {
    async fn present(
        &self,
        request: UserQuestionRequest,
    ) -> Result<UserQuestionResponse, UserQuestionUiError>;
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRequest {
    session_id: String,
    tool_call_id: String,
    questions: Vec<WireQuestion>,
    mode: WireMode,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireQuestion {
    question: String,
    options: Vec<WireOption>,
    #[serde(default)]
    multi_select: Option<bool>,
}

#[derive(Clone, serde::Deserialize)]
struct WireOption {
    label: String,
    description: String,
    #[serde(default)]
    preview: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireMode {
    Default,
    Plan,
}

fn question_id(request_id: &str, index: usize) -> String {
    format!("{request_id}:question:{index}")
}

fn option_id(question_id: &str, index: usize) -> String {
    format!("{question_id}:option:{index}")
}

#[derive(Debug)]
pub(crate) enum UserQuestionDispatchError {
    InvalidParams,
    Failed,
}

pub(crate) async fn dispatch(
    ui: Arc<dyn UserQuestionUi>,
    params: serde_json::Value,
) -> Result<serde_json::Value, UserQuestionDispatchError> {
    let wire: WireRequest =
        serde_json::from_value(params).map_err(|_| UserQuestionDispatchError::InvalidParams)?;
    if wire.session_id.trim().is_empty()
        || wire.tool_call_id.trim().is_empty()
        || wire.questions.is_empty()
    {
        return Err(UserQuestionDispatchError::InvalidParams);
    }

    let questions = wire
        .questions
        .iter()
        .enumerate()
        .map(|(question_index, question)| {
            let id = question_id(&wire.tool_call_id, question_index);
            UserQuestion {
                id: id.clone(),
                prompt: question.question.clone(),
                options: question
                    .options
                    .iter()
                    .enumerate()
                    .map(|(option_index, option)| UserQuestionOption {
                        id: option_id(&id, option_index),
                        label: option.label.clone(),
                        description: option.description.clone(),
                        preview: option.preview.clone(),
                    })
                    .collect(),
                multi_select: question.multi_select.unwrap_or(false),
            }
        })
        .collect::<Vec<_>>();
    let request = UserQuestionRequest {
        session_id: wire.session_id,
        request_id: wire.tool_call_id,
        questions: questions.clone(),
        mode: match wire.mode {
            WireMode::Default => UserQuestionMode::Default,
            WireMode::Plan => UserQuestionMode::Plan,
        },
    };
    let mode = request.mode;
    let response = ui
        .present(request)
        .await
        .map_err(|_| UserQuestionDispatchError::Failed)?;
    if mode == UserQuestionMode::Default
        && matches!(
            response,
            UserQuestionResponse::ChatAboutThis(_) | UserQuestionResponse::SkipInterview(_)
        )
    {
        return Err(UserQuestionDispatchError::Failed);
    }
    response_json(&questions, response).map_err(|_| UserQuestionDispatchError::Failed)
}

fn response_json(
    questions: &[UserQuestion],
    response: UserQuestionResponse,
) -> Result<serde_json::Value, UserQuestionUiError> {
    let (outcome, answers) = match response {
        UserQuestionResponse::Answered(answers) => ("accepted", Some(answers)),
        UserQuestionResponse::ChatAboutThis(answers) => ("chat_about_this", Some(answers)),
        UserQuestionResponse::SkipInterview(answers) => ("skip_interview", Some(answers)),
        UserQuestionResponse::Dismissed => ("cancelled", None),
    };
    let Some(answers) = answers else {
        return Ok(serde_json::json!({ "outcome": outcome }));
    };

    let by_id = questions
        .iter()
        .map(|question| (question.id.as_str(), question))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut wire_answers = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();
    for answer in answers {
        let Some(question) = by_id.get(answer.question_id.as_str()).copied() else {
            return Err(UserQuestionUiError {
                message: "an answer named a question outside its request".into(),
            });
        };
        if !seen.insert(answer.question_id.clone()) {
            return Err(UserQuestionUiError {
                message: "a question was answered more than once".into(),
            });
        }
        let options = question
            .options
            .iter()
            .map(|option| (option.id.as_str(), option))
            .collect::<HashMap<_, _>>();
        let mut selected = HashSet::new();
        let mut labels = Vec::new();
        let mut selected_preview = None;
        for option_id in answer.option_ids {
            let Some(option) = options.get(option_id.as_str()).copied() else {
                return Err(UserQuestionUiError {
                    message: "an answer named an option outside its question".into(),
                });
            };
            if !selected.insert(option.id.as_str()) {
                return Err(UserQuestionUiError {
                    message: "an option was selected more than once".into(),
                });
            }
            labels.push(option.label.clone());
            if labels.len() == 1 {
                selected_preview = option.preview.clone();
            } else {
                selected_preview = None;
            }
        }
        let other = answer.other.map(|text| text.trim().to_owned());
        if other.as_deref() == Some("") {
            return Err(UserQuestionUiError {
                message: "a free-text answer must not be blank".into(),
            });
        }
        if other.is_some() {
            labels.push("Other".into());
        }
        if !question.multi_select && labels.len() > 1 {
            return Err(UserQuestionUiError {
                message: "a single-select question received several answers".into(),
            });
        }
        if labels.is_empty() {
            return Err(UserQuestionUiError {
                message: "an answer must select an option or include free text".into(),
            });
        }
        wire_answers.insert(question.prompt.clone(), serde_json::json!(&labels));
        if selected_preview.is_some() || other.is_some() {
            annotations.insert(
                question.prompt.clone(),
                serde_json::json!({
                    "preview": selected_preview,
                    "notes": other,
                }),
            );
        }
    }

    let mut result =
        serde_json::Map::from_iter([("outcome".into(), serde_json::Value::String(outcome.into()))]);
    let answer_key = if outcome == "accepted" {
        "answers"
    } else {
        "partial_answers"
    };
    if outcome == "accepted" {
        result.insert(answer_key.into(), serde_json::Value::Object(wire_answers));
        if !annotations.is_empty() {
            result.insert("annotations".into(), serde_json::Value::Object(annotations));
        }
    } else {
        let partial = wire_answers
            .into_iter()
            .filter_map(|(question, labels)| {
                let labels = labels.as_array()?;
                let answer = match labels.as_slice() {
                    [label] if label.as_str() != Some("Other") => label.clone(),
                    _ => annotations
                        .get(&question)
                        .and_then(|annotation| annotation.get("notes"))
                        .filter(|notes| !notes.is_null())
                        .cloned()
                        .unwrap_or_else(|| {
                            serde_json::Value::String(
                                labels
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            )
                        }),
                };
                Some((question, answer))
            })
            .collect();
        result.insert(answer_key.into(), serde_json::Value::Object(partial));
    }
    Ok(serde_json::Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Ui {
        requests: Mutex<Vec<UserQuestionRequest>>,
        response: Mutex<Option<UserQuestionResponse>>,
    }

    #[async_trait::async_trait]
    impl UserQuestionUi for Ui {
        async fn present(
            &self,
            request: UserQuestionRequest,
        ) -> Result<UserQuestionResponse, UserQuestionUiError> {
            self.requests.lock().unwrap().push(request);
            self.response
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| UserQuestionUiError {
                    message: "the fixture has no response".into(),
                })
        }
    }

    fn wire_request() -> serde_json::Value {
        serde_json::json!({
            "sessionId": "session-1",
            "toolCallId": "ask-1",
            "mode": "default",
            "questions": [{
                "question": "Which database?",
                "options": [{
                    "label": "SQLite",
                    "description": "Embedded and local",
                    "preview": "schema.sql"
                }, {
                    "label": "Postgres",
                    "description": "Shared service"
                }],
                "multiSelect": false
            }]
        })
    }

    #[tokio::test]
    async fn typed_ui_conforms_to_the_native_reverse_question_contract() {
        let ui = Arc::new(Ui {
            requests: Mutex::new(Vec::new()),
            response: Mutex::new(Some(UserQuestionResponse::Answered(vec![
                UserQuestionAnswer {
                    question_id: "ask-1:question:0".into(),
                    option_ids: vec!["ask-1:question:0:option:0".into()],
                    other: None,
                },
            ]))),
        });

        let response = dispatch(ui.clone(), wire_request()).await.unwrap();

        let requests = ui.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].session_id, "session-1");
        assert_eq!(requests[0].request_id, "ask-1");
        assert_eq!(requests[0].mode, UserQuestionMode::Default);
        assert_eq!(requests[0].questions[0].prompt, "Which database?");
        assert_eq!(requests[0].questions[0].options[0].label, "SQLite");
        assert_eq!(
            response,
            serde_json::json!({
                "outcome": "accepted",
                "answers": {"Which database?": ["SQLite"]},
                "annotations": {
                    "Which database?": {
                        "preview": "schema.sql",
                        "notes": null
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn an_answer_cannot_escape_the_request_that_opened_it() {
        let ui = Arc::new(Ui {
            requests: Mutex::new(Vec::new()),
            response: Mutex::new(Some(UserQuestionResponse::Answered(vec![
                UserQuestionAnswer {
                    question_id: "some-other-question".into(),
                    option_ids: Vec::new(),
                    other: Some("invented".into()),
                },
            ]))),
        });

        assert!(matches!(
            dispatch(ui, wire_request()).await,
            Err(UserQuestionDispatchError::Failed)
        ));
    }
}
