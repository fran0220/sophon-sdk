//! Native advisory judgments through an explicitly configured System One route.
//! One bounded POST, no retries/redirects. Dropping execution stops the local
//! wait, not remote inference or billing. Never replay an unknown outcome.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::{Error, protocol::ToolSpec};

const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionConfig {
    pub endpoint: DecisionEndpoint,
    pub model: String,
    /// Versioned relay base URL ending in /v1, never a model-selected URL.
    pub base_url: String,
    /// Trusted-host relay credential; never included in tool inputs or outputs.
    pub bearer_token: String,
}

#[derive(Clone, Serialize, Deserialize, TS)]
pub enum DecisionEndpoint {
    #[serde(rename = "system-one")]
    SystemOne,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, TS)]
#[serde(untagged)]
pub enum DecisionContent {
    Text(String),
    Object(BTreeMap<String, Value>),
    Array(Vec<Value>),
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<DecisionContent>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<DecisionContent>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionQuestion {
    Noul {
        instructions: DecisionContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: DecisionContent,
        criteria: BTreeMap<String, Option<DecisionContent>>,
    },
    Score {
        instructions: DecisionContent,
        criteria: Vec<DecisionContent>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DecisionRequest {
    pub state: DecisionContent,
    pub questions: BTreeMap<String, DecisionQuestion>,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum DecisionAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
        legend: BTreeMap<String, DecisionContent>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DecisionUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionResult {
    pub request_id: String,
    pub tool_call_id: String,
    pub elapsed_ms: u32,
    pub requested_model: String,
    pub model: String,
    pub answers: BTreeMap<String, DecisionAnswer>,
    pub usage: DecisionUsage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    model: String,
    answers: BTreeMap<String, DecisionAnswer>,
    usage: DecisionUsage,
}

pub(crate) struct DecisionService {
    config: DecisionConfig,
    url: Url,
    client: Client,
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 256
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}

impl DecisionService {
    pub(crate) fn new(config: DecisionConfig) -> Result<Self, Error> {
        let mut url = Url::parse(&config.base_url).map_err(|_| {
            Error::invalid_config("decision baseUrl must be a versioned HTTP(S) URL")
        })?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if !(url.scheme() == "https" || (url.scheme() == "http" && local))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.path().trim_end_matches('/').ends_with("/v1")
            || !valid_model(&config.model)
            || config.bearer_token.trim().is_empty()
            || reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.bearer_token))
                .is_err()
        {
            return Err(Error::invalid_config("invalid explicit decision route"));
        }
        url.set_path(&format!("{}/systemone", url.path().trim_end_matches('/')));
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::invalid_config("cannot construct decision client"))?;
        Ok(Self {
            config,
            url,
            client,
        })
    }

    pub(crate) fn tool_spec() -> ToolSpec {
        let content =
            json!({"anyOf":[{"type":"string"},{"type":"object"},{"type":"array","items":{}}]});
        let nullable = json!({"anyOf":[content.clone(),{"type":"null"}]});
        ToolSpec {
            name: "evaluate_decisions".into(),
            description: "Evaluate advisory semantic judgments over supplied evidence. Batch independent questions sharing state: noul returns probability of yes, choice selects among candidates, score rates ordered descriptive levels. Instructions and descriptions may be strings, objects, or arrays. Include uncertainty/no-match candidates where needed. Questions cannot see each other's answers; IDs are not instructions. Use deterministic checks for known rules. Results are not permission, proof, or execution: concrete tools execute and verify actions. No automatic retries; cancellation/timeout may leave remote inference/billing unknown. Never submit credentials or unrelated private data.".into(),
            input_schema: json!({"type":"object","additionalProperties":false,"required":["state","questions"],"properties":{
                "state":content,
                "questions":{"type":"object","minProperties":1,"additionalProperties":{"anyOf":[
                    {"type":"object","additionalProperties":false,"required":["type","instructions"],"properties":{
                        "type":{"const":"noul"},"instructions":content,"criteria":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"properties":{"true":nullable,"false":nullable}}]}}},
                    {"type":"object","additionalProperties":false,"required":["type","instructions","criteria"],"properties":{
                        "type":{"const":"choice"},"instructions":content,"criteria":{"type":"object","minProperties":1,"maxProperties":255,"additionalProperties":nullable}}},
                    {"type":"object","additionalProperties":false,"required":["type","instructions","criteria"],"properties":{
                        "type":{"const":"score"},"instructions":content,"criteria":{"type":"array","minItems":2,"maxItems":10,"items":content}}}
                ]}}
            }}),
        }
    }

    pub(crate) async fn execute(&self, args: Value, tool_call_id: String) -> Result<Value, Error> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let fail = |code: &str, outcome: &str| {
            Error::Operation(format!(
                "decision {code}; requestId={request_id}; elapsedMs={}; outcome={outcome}; no automatic retry; remote cancellation/billing not confirmed",
                started.elapsed().as_millis()
            ))
        };
        let request: DecisionRequest =
            serde_json::from_value(args).map_err(|_| fail("invalid_arguments", "not_submitted"))?;
        validate_request(&request).map_err(|_| fail("invalid_arguments", "not_submitted"))?;
        let body = serde_json::to_vec(
            &json!({"model":self.config.model,"state":request.state,"questions":request.questions}),
        )
        .map_err(|_| fail("invalid_arguments", "not_submitted"))?;
        if body.len() > MAX_BYTES {
            return Err(fail("request_too_large", "not_submitted"));
        }
        let mut response = self
            .client
            .post(self.url.clone())
            .bearer_auth(&self.config.bearer_token)
            .header("content-type", "application/json")
            .header("x-client-request-id", &request_id)
            .body(body)
            .send()
            .await
            .map_err(|_| fail("transport_or_timeout", "unknown"))?;
        if !response.status().is_success() {
            return Err(fail(
                &format!("http_{}", response.status().as_u16()),
                "remote_error",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| fail("response_interrupted", "unknown"))?
        {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(fail("response_too_large", "unknown"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: Response =
            serde_json::from_slice(&bytes).map_err(|_| fail("invalid_response", "unknown"))?;
        validate_response(&request, &response).map_err(|_| fail("invalid_response", "unknown"))?;
        serde_json::to_value(DecisionResult {
            request_id,
            tool_call_id,
            elapsed_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
            requested_model: self.config.model.clone(),
            model: response.model,
            answers: response.answers,
            usage: response.usage,
        })
        .map_err(|_| Error::Operation("cannot encode decision result".into()))
    }
}

fn validate_request(request: &DecisionRequest) -> Result<(), ()> {
    if request.questions.is_empty() {
        return Err(());
    }
    for (id, question) in &request.questions {
        if id.trim().is_empty() {
            return Err(());
        }
        let instructions = match question {
            DecisionQuestion::Noul { instructions, .. } => instructions,
            DecisionQuestion::Choice {
                instructions,
                criteria,
            } => {
                if criteria.is_empty()
                    || criteria.len() > 255
                    || criteria.keys().any(|key| key.trim().is_empty())
                {
                    return Err(());
                }
                instructions
            }
            DecisionQuestion::Score {
                instructions,
                criteria,
            } => {
                if !(2..=10).contains(&criteria.len()) {
                    return Err(());
                }
                instructions
            }
        };
        let empty = match instructions {
            DecisionContent::Text(text) => text.trim().is_empty(),
            DecisionContent::Object(value) => value.is_empty(),
            DecisionContent::Array(value) => value.is_empty(),
        };
        if empty {
            return Err(());
        }
    }
    Ok(())
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn distribution(
    probabilities: &BTreeMap<String, f64>,
    keys: impl Iterator<Item = String>,
) -> Result<(), ()> {
    let keys: std::collections::BTreeSet<_> = keys.collect();
    if probabilities
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        != keys
        || !probabilities.values().copied().all(probability)
        || (probabilities.values().sum::<f64>() - 1.0).abs() > 1e-6
    {
        return Err(());
    }
    Ok(())
}

fn validate_response(request: &DecisionRequest, response: &Response) -> Result<(), ()> {
    if !valid_model(&response.model) || !request.questions.keys().eq(response.answers.keys()) {
        return Err(());
    }
    for (id, question) in &request.questions {
        match (question, &response.answers[id]) {
            (DecisionQuestion::Noul { .. }, DecisionAnswer::Noul { noul })
                if probability(*noul) => {}
            (
                DecisionQuestion::Choice { criteria, .. },
                DecisionAnswer::Choice {
                    choice,
                    confidence,
                    probabilities,
                },
            ) => {
                distribution(probabilities, criteria.keys().cloned())?;
                if !probability(*confidence)
                    || !criteria.contains_key(choice)
                    || probabilities
                        .values()
                        .any(|p| *p > probabilities[choice] + 1e-6)
                {
                    return Err(());
                }
            }
            (
                DecisionQuestion::Score { criteria, .. },
                DecisionAnswer::Score {
                    score,
                    confidence,
                    probabilities,
                    legend,
                },
            ) => {
                distribution(probabilities, (0..criteria.len()).map(|n| n.to_string()))?;
                let expected: BTreeMap<_, _> = criteria
                    .iter()
                    .enumerate()
                    .map(|(i, value)| (i.to_string(), value.clone()))
                    .collect();
                let mean: f64 = (0..criteria.len())
                    .map(|i| i as f64 * probabilities[&i.to_string()])
                    .sum();
                // API examples round reported scores to two decimal places.
                if !probability(*confidence)
                    || !score.is_finite()
                    || *score < 0.0
                    || *score > (criteria.len() - 1) as f64
                    || (*score - mean).abs() > 0.005001
                    || *legend != expected
                {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"state":[{"failed":true}],"questions":{
            "ready":{"type":"noul","instructions":{"question":"Ready?"},"criteria":{"true":["ready"],"false":{"failed":true}}},
            "action":{"type":"choice","instructions":"Select evidence-based action","criteria":{"inspect":{"kind":"read"},"none":null}},
            "severity":{"type":"score","instructions":["Rate blocking impact"],"criteria":["cosmetic",{"blocked":true},["unusable"]]}
        }})
    }

    fn response() -> Value {
        json!({"model":"resolved-model-1","usage":{"input_tokens":137,"output_tokens":29},"answers":{
            "ready":{"type":"noul","noul":0.13},
            "action":{"type":"choice","choice":"inspect","confidence":0.71,"probabilities":{"inspect":0.83,"none":0.17}},
            "severity":{"type":"score","score":1.55,"confidence":0.42,"probabilities":{"0":0.10,"1":0.25,"2":0.65},"legend":{"0":"cosmetic","1":{"blocked":true},"2":["unusable"]}}
        }})
    }

    #[test]
    fn exact_typed_answers_and_distributions_are_validated() {
        let request = serde_json::from_value(request()).unwrap();
        assert!(validate_request(&request).is_ok());
        assert!(validate_response(&request, &serde_json::from_value(response()).unwrap()).is_ok());
        for (pointer, replacement) in [
            ("/model", json!("")),
            ("/usage/input_tokens", json!(-1)),
            ("/usage/output_tokens", json!(1.5)),
            ("/usage", Value::Null),
            (
                "/answers/ready",
                json!({"type":"choice","choice":"inspect","confidence":1,"probabilities":{"inspect":1}}),
            ),
            ("/answers/ready/noul", json!(1.01)),
            ("/answers/action/choice", json!("invented")),
            ("/answers/action/choice", json!("none")),
            ("/answers/action/confidence", json!(-0.1)),
            (
                "/answers/action/probabilities",
                json!({"inspect":0.83,"extra":0.17}),
            ),
            ("/answers/action/probabilities/none", json!(0.01)),
            ("/answers/severity/score", json!(1.75)),
            ("/answers/severity/score", json!(2.1)),
            ("/answers/severity/legend/1", json!({"blocked":false})),
            ("/answers/severity/probabilities/0", json!(-0.1)),
        ] {
            let mut value = response();
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                serde_json::from_value(value)
                    .ok()
                    .is_none_or(|value| validate_response(&request, &value).is_err()),
                "{pointer}"
            );
        }
        for id in ["ready", "extra"] {
            let mut value = response();
            if id == "extra" {
                value["answers"][id] = json!({"type":"noul","noul":0.3});
            } else {
                value["answers"].as_object_mut().unwrap().remove(id);
            }
            assert!(validate_response(&request, &serde_json::from_value(value).unwrap()).is_err());
        }
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!probability(invalid));
        }
    }

    #[test]
    fn request_bounds_reject_before_submission_without_restricting_structured_content() {
        for state in [json!(""), json!({}), json!([])] {
            let mut value = request();
            value["state"] = state;
            assert!(validate_request(&serde_json::from_value(value).unwrap()).is_ok());
        }
        for invalid in [Value::Null, json!(false), json!(7)] {
            let mut value = request();
            value["state"] = invalid;
            assert!(serde_json::from_value::<DecisionRequest>(value).is_err());
        }
        for (count, valid) in [(0, false), (1, true), (255, true), (256, false)] {
            let mut value = request();
            value["questions"]["action"]["criteria"] = (0..count)
                .map(|i| (i.to_string(), Value::Null))
                .collect::<serde_json::Map<_, _>>()
                .into();
            assert_eq!(
                validate_request(&serde_json::from_value(value).unwrap()).is_ok(),
                valid
            );
        }
        for (count, valid) in [(1, false), (2, true), (10, true), (11, false)] {
            let mut value = request();
            value["questions"]["severity"]["criteria"] = json!(vec!["level"; count]);
            assert_eq!(
                validate_request(&serde_json::from_value(value).unwrap()).is_ok(),
                valid
            );
        }
        for instructions in [json!(" "), json!({}), json!([])] {
            let mut value = request();
            value["questions"]["ready"]["instructions"] = instructions;
            assert!(validate_request(&serde_json::from_value(value).unwrap()).is_err());
        }
        let mut value = request();
        value["questions"] = json!({});
        assert!(validate_request(&serde_json::from_value(value).unwrap()).is_err());
    }

    #[tokio::test]
    async fn oversized_request_never_opens_the_configured_route() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let service = DecisionService::new(DecisionConfig {
            endpoint: DecisionEndpoint::SystemOne,
            model: "published-route".into(),
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            bearer_token: "credential-must-not-leak".into(),
        })
        .unwrap();
        let mut input = request();
        input["state"] = json!("x".repeat(MAX_BYTES));
        let error = service
            .execute(input, "tool-73".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("request_too_large") && error.contains("not_submitted"));
        assert!(!error.contains("credential-must-not-leak"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
