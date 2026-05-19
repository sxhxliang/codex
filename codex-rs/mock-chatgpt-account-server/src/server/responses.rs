use serde_json::Value;
use serde_json::json;

use crate::server::state::APPLY_PATCH_APPROVAL_DEMO_CALL_ID;
use crate::server::state::APPLY_PATCH_APPROVAL_DEMO_FILE;
use crate::server::state::APPLY_PATCH_APPROVAL_DEMO_FILE_CONTENT;
use crate::server::state::APPLY_PATCH_APPROVAL_DEMO_TRIGGER;
use crate::server::state::AppState;

pub fn build_responses_events(state: &AppState, payload: &Value) -> Vec<Value> {
    let mut prompt = extract_text_fragments(payload.get("input")).join("\n");
    prompt = prompt.trim().to_string();
    if prompt.is_empty() {
        prompt = payload
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("Mock request")
            .to_string();
    }

    let response_id = format!("resp_{}", random_id(6));
    let mut events = vec![json!({
        "type": "response.created",
        "response": { "id": response_id },
    })];

    if has_apply_patch_output(payload.get("input")) {
        let output_text = approval_demo_completed_text();
        let message_id = format!("msg_{}", random_id(4));
        let token_count = token_count(&prompt) + token_count(output_text);
        events.extend(build_assistant_message_events(&message_id, output_text));
        events.push(completed_event(
            &response_id,
            &prompt,
            output_text,
            token_count,
        ));
        return events;
    }

    if prompt
        .to_lowercase()
        .contains(APPLY_PATCH_APPROVAL_DEMO_TRIGGER)
    {
        let patch = approval_demo_patch();
        events.push(json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "name": "apply_patch",
                "arguments": json!({ "input": patch }).to_string(),
                "call_id": APPLY_PATCH_APPROVAL_DEMO_CALL_ID,
            },
        }));
        events.push(json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "output": [],
                "usage": {
                    "input_tokens": token_count(&prompt).max(1),
                    "input_tokens_details": Value::Null,
                    "output_tokens": 1,
                    "output_tokens_details": Value::Null,
                    "total_tokens": token_count(&prompt).max(1) + 1,
                },
            },
        }));
        return events;
    }

    let output_text = state.response_text_for_prompt(&prompt);
    let token_total = token_count(&prompt) + token_count(&output_text);
    if payload.get("generate").and_then(Value::as_bool) != Some(false) {
        let message_id = format!("msg_{}", random_id(4));
        events.extend(build_assistant_message_events(&message_id, &output_text));
    }
    events.push(completed_event(
        &response_id,
        &prompt,
        &output_text,
        token_total,
    ));
    events
}

fn build_assistant_message_events(message_id: &str, output_text: &str) -> Vec<Value> {
    vec![
        json!({
            "type": "response.output_item.added",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": message_id,
                "content": [
                    {
                        "type": "output_text",
                        "text": "",
                    }
                ],
            },
        }),
        json!({
            "type": "response.output_text.delta",
            "delta": output_text,
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": message_id,
                "content": [
                    {
                        "type": "output_text",
                        "text": output_text,
                    }
                ],
            },
        }),
    ]
}

fn completed_event(
    response_id: &str,
    prompt: &str,
    output_text: &str,
    token_total: usize,
) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "output": [],
            "usage": {
                "input_tokens": token_count(prompt).max(1),
                "input_tokens_details": Value::Null,
                "output_tokens": token_count(output_text).max(1),
                "output_tokens_details": Value::Null,
                "total_tokens": token_total.max(2),
            },
        },
    })
}

fn approval_demo_patch() -> String {
    format!(
        "*** Begin Patch\n*** Add File: {APPLY_PATCH_APPROVAL_DEMO_FILE}\n+{}\n*** End Patch\n",
        APPLY_PATCH_APPROVAL_DEMO_FILE_CONTENT.trim_end()
    )
}

fn approval_demo_completed_text() -> &'static str {
    "apply_patch approval demo completed: the client approved the file change and created APPROVAL_DEMO.txt."
}

pub(crate) fn extract_text_fragments(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .flat_map(|item| extract_text_fragments(Some(item)))
            .collect(),
        Value::Object(map) => {
            let mut fragments = Vec::new();
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                fragments.push(text.to_string());
            }
            if let Some(content) = map.get("content") {
                fragments.extend(extract_text_fragments(Some(content)));
            }
            if let Some(input) = map.get("input") {
                fragments.extend(extract_text_fragments(Some(input)));
            }
            fragments
        }
        Value::Bool(_) | Value::Null | Value::Number(_) => Vec::new(),
    }
}

fn has_apply_patch_output(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::Array(items) => items.iter().any(|item| has_apply_patch_output(Some(item))),
        Value::Object(map) => {
            if map
                .get("call_id")
                .and_then(Value::as_str)
                .map(|call_id| call_id == APPLY_PATCH_APPROVAL_DEMO_CALL_ID)
                .unwrap_or(false)
                && map
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|kind| kind == "function_call_output" || kind == "custom_tool_call_output")
                    .unwrap_or(false)
            {
                return true;
            }
            map.get("content")
                .map(|content| has_apply_patch_output(Some(content)))
                .unwrap_or(false)
                || map
                    .get("input")
                    .map(|input| has_apply_patch_output(Some(input)))
                    .unwrap_or(false)
        }
        Value::Bool(_) | Value::Null | Value::Number(_) | Value::String(_) => false,
    }
}

fn token_count(text: &str) -> usize {
    text.split_whitespace().count()
}

fn random_id(len: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string()[..len].to_string()
}
