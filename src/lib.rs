use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

mod service;

pub use service::{
    create_router, parse_startup_options, proxy_config_from_env, proxy_config_from_vars,
    ProxyConfig, StartupOptions,
};

const TRUNCATED_TEXT_MIN_CHARS: usize = 120;

fn finish_is_truncated(
    finish_reason: Option<&str>,
    text_chars: usize,
    has_tool_calls: bool,
) -> bool {
    match finish_reason {
        Some("length") => has_tool_calls || text_chars < TRUNCATED_TEXT_MIN_CHARS,
        Some("stop" | "tool_calls") | None => false,
        Some(_) => true,
    }
}

fn short_id(prefix: &str) -> String {
    let raw_id = Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &raw_id[..24])
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn emit(event_type: &str, mut data: Value, sequence: usize) -> (String, usize) {
    if let Value::Object(object) = &mut data {
        object.entry("type".to_owned()).or_insert(json!(event_type));
        object
            .entry("sequence_number".to_owned())
            .or_insert(json!(sequence));
    }
    (
        format!("event: {event_type}\ndata: {data}\n\n"),
        sequence + 1,
    )
}

fn call_id(item: &Value) -> Option<&str> {
    item.get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
}

pub fn sanitize_input_items(items: &mut Vec<Value>) -> bool {
    let output_ids = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .collect::<HashSet<_>>();

    let mut call_ids = HashSet::new();
    for item in items.iter() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) != Some("function_call") {
                            continue;
                        }
                        if let Some(id) = call_id(part) {
                            call_ids.insert(id);
                        }
                    }
                }
            }
            Some("function_call") => {
                if let Some(id) = call_id(item) {
                    call_ids.insert(id);
                }
            }
            _ => {}
        }
    }
    let valid = call_ids
        .intersection(&output_ids)
        .copied()
        .collect::<HashSet<_>>();

    let mut changed = false;
    let mut retained = Vec::with_capacity(items.len());
    for item in &*items {
        if !item.is_object() {
            retained.push(item.clone());
            continue;
        }

        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let original = item.get("content").and_then(Value::as_array);
                let mut content = Vec::new();
                if let Some(parts) = original {
                    for part in parts {
                        let orphan = part.get("type").and_then(Value::as_str)
                            == Some("function_call")
                            && call_id(part).is_some_and(|id| !valid.contains(id));
                        if orphan {
                            changed = true;
                        } else {
                            content.push(part.clone());
                        }
                    }
                }
                if content.is_empty() {
                    if original.is_some_and(|parts| !parts.is_empty()) {
                        changed = true;
                    }
                    continue;
                }
                let mut clean = item.clone();
                clean["content"] = Value::Array(content);
                retained.push(clean);
            }
            Some("function_call") => {
                if call_id(item).is_none_or(|id| valid.contains(id)) {
                    retained.push(item.clone());
                } else {
                    changed = true;
                }
            }
            _ => retained.push(item.clone()),
        }
    }
    *items = retained;
    changed
}

fn is_plain_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("message")
        && !item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("function_call"))
            })
}

pub fn fix_tool_order(items: &mut Vec<Value>) -> bool {
    let mut result = Vec::with_capacity(items.len());
    let mut pending_calls = Vec::new();
    let mut batch_outputs = Vec::new();
    let mut pending_messages = Vec::new();
    let mut changed = false;

    fn flush(
        result: &mut Vec<Value>,
        pending_calls: &mut Vec<Value>,
        batch_outputs: &mut Vec<Value>,
        pending_messages: &mut Vec<Value>,
    ) {
        result.append(pending_calls);
        result.append(batch_outputs);
        result.append(pending_messages);
    }

    for item in &*items {
        if !item.is_object() {
            flush(
                &mut result,
                &mut pending_calls,
                &mut batch_outputs,
                &mut pending_messages,
            );
            result.push(item.clone());
            continue;
        }

        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if !batch_outputs.is_empty() {
                    flush(
                        &mut result,
                        &mut pending_calls,
                        &mut batch_outputs,
                        &mut pending_messages,
                    );
                }
                pending_calls.push(item.clone());
            }
            Some("function_call_output")
                if pending_calls
                    .iter()
                    .any(|call| call_id(call) == call_id(item)) =>
            {
                batch_outputs.push(item.clone());
            }
            _ if !pending_calls.is_empty() && is_plain_message(item) => {
                pending_messages.push(item.clone());
                changed = true;
            }
            _ => {
                flush(
                    &mut result,
                    &mut pending_calls,
                    &mut batch_outputs,
                    &mut pending_messages,
                );
                result.push(item.clone());
            }
        }
    }
    flush(
        &mut result,
        &mut pending_calls,
        &mut batch_outputs,
        &mut pending_messages,
    );
    *items = result;
    changed
}

fn normalized_output(value: &Value) -> String {
    let mut current = value.clone();
    if current.is_object() {
        current = current.get("output").unwrap_or(&current).clone();
    }
    if current.is_object() {
        current = match current.get("content") {
            Some(content) => content.clone(),
            None => Value::String(current.to_string()),
        };
    }
    match current {
        Value::Null => String::new(),
        Value::String(text) => text,
        other => other.to_string(),
    }
}

pub fn translate_request(responses: &Value, default_max_tokens: u64) -> Value {
    let mut messages = Vec::new();
    let mut items = responses
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    sanitize_input_items(&mut items);
    fix_tool_order(&mut items);

    if let Some(instructions) = responses.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    for item in &items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .unwrap_or(&vec![])
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("input_text" | "output_text") => {
                            text.push_str(
                                part.get("text").and_then(Value::as_str).unwrap_or_default(),
                            );
                        }
                        Some("function_call") => tool_calls.push(json!({
                            "id": call_id(part),
                            "type": "function",
                            "function": {
                                "name": part.get("name"),
                                "arguments": part.get("arguments").unwrap_or(&json!("")),
                            }
                        })),
                        _ => {}
                    }
                }

                match role {
                    "assistant" => {
                        let mut message = Map::new();
                        message.insert("role".to_owned(), json!("assistant"));
                        if !text.is_empty() {
                            message.insert("content".to_owned(), json!(text));
                        }
                        if !tool_calls.is_empty() {
                            message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
                        }
                        messages.push(Value::Object(message));
                    }
                    "user" | "system" | "developer" | "tool" => {
                        let chat_role = if role == "developer" { "system" } else { role };
                        messages.push(json!({"role": chat_role, "content": text}));
                    }
                    _ => {}
                }
            }
            Some("function_call_output") => messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id"),
                "content": item.get("output").map(normalized_output).unwrap_or_default(),
            })),
            _ => {}
        }
    }

    let tools = responses
        .get("tools")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.get("name"),
                            "description": tool.get("description"),
                            "parameters": tool.get("parameters"),
                        }
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let max_tokens = responses
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(default_max_tokens);
    let mut chat = json!({
        "model": responses.get("model"),
        "messages": messages,
        "stream": false,
        "max_tokens": max_tokens,
    });
    if let Some(reasoning) = responses.get("reasoning") {
        if reasoning.is_object() {
            chat["reasoning"] = reasoning.clone();
        }
    }
    for field in ["temperature", "top_p"] {
        if let Some(value) = responses.get(field) {
            if !value.is_null() {
                chat[field] = value.clone();
            }
        }
    }
    if !tools.is_empty() {
        chat["tools"] = Value::Array(tools);
    }
    if let Some(choice) = responses.get("tool_choice") {
        if !choice.is_null() {
            if choice.get("type").and_then(Value::as_str) == Some("function") {
                chat["tool_choice"] = json!({
                    "type": "function",
                    "function": {"name": choice.get("name")}
                });
            } else {
                chat["tool_choice"] = choice.clone();
            }
        }
    }
    chat
}

struct Emitter {
    sequence: usize,
}

impl Emitter {
    fn new() -> Self {
        Self { sequence: 0 }
    }

    fn emit(&mut self, event_type: &str, data: Value) -> String {
        let (payload, sequence) = emit(event_type, data, self.sequence);
        self.sequence = sequence;
        payload
    }
}

pub fn chat_to_sse(chat: &Value, model: &str) -> String {
    let mut emitter = Emitter::new();
    let response_id = short_id("resp");
    let now = unix_now();
    let mut output_items = Vec::new();
    let mut output = String::new();
    output += &emitter.emit(
        "response.created",
        json!({"response": {
            "id": response_id, "object": "response", "created_at": now,
            "status": "in_progress", "model": model, "output": []
        }}),
    );
    output += &emitter.emit(
        "response.in_progress",
        json!({"response": {"id": response_id, "object": "response", "status": "in_progress", "model": model}}),
    );

    let message = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish_reason = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str);
    let truncated =
        finish_is_truncated(finish_reason, text.chars().count(), !tool_calls.is_empty());

    for call in &tool_calls {
        let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
        let call_id_value = call
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| short_id("call"));
        let item = json!({
            "id": call_id_value, "type": "function_call", "call_id": call_id_value,
            "name": function.get("name"), "arguments": function.get("arguments").unwrap_or(&json!("")),
        });
        output += &emitter.emit(
            "response.output_item.added",
            json!({"output_index": output_items.len(), "item": merge_status(&item, "in_progress")}),
        );
        if !truncated {
            output += &emitter.emit(
                "response.output_item.done",
                json!({"output_index": output_items.len(), "item": merge_status(&item, "completed")}),
            );
        }
        output_items.push(item);
    }

    if !text.trim().is_empty() {
        let item_id = short_id("msg");
        let index = output_items.len();
        let initial_item = json!({
            "id": item_id, "type": "message", "status": "in_progress", "role": "assistant",
            "phase": "final_answer", "content": []
        });
        output += &emitter.emit(
            "response.output_item.added",
            json!({"output_index": index, "item": initial_item}),
        );
        output += &emitter.emit(
            "response.content_part.added",
            json!({
                "output_index": index, "content_index": 0, "item_id": item_id,
                "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
            }),
        );
        output += &emitter.emit(
            "response.output_text.delta",
            json!({"output_index": index, "content_index": 0, "item_id": item_id, "delta": text}),
        );
        if !truncated {
            output += &emitter.emit(
                "response.output_text.done",
                json!({"output_index": index, "content_index": 0, "item_id": item_id, "text": text}),
            );
            output += &emitter.emit(
                "response.content_part.done",
                json!({
                    "output_index": index, "content_index": 0, "item_id": item_id,
                    "part": {"type": "output_text", "text": text, "annotations": [], "logprobs": []}
                }),
            );
            let done_item = json!({
                "id": item_id, "type": "message", "status": "completed", "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": text, "annotations": [], "logprobs": []}]
            });
            output += &emitter.emit(
                "response.output_item.done",
                json!({"output_index": index, "item": done_item}),
            );
            output_items.push(done_item);
        }
    }

    if truncated {
        eprintln!(
            "[proxy] translate truncated: reason={finish_reason:?} text_chars={} tool_calls={}",
            text.chars().count(),
            tool_calls.len()
        );
        return output;
    }

    let usage = chat.get("usage").cloned().unwrap_or_else(|| json!({}));
    let usage_value = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
    output += &emitter.emit(
        "response.completed",
        json!({"response": {
            "id": response_id, "object": "response", "created_at": now, "completed_at": unix_now(),
            "status": "completed", "model": model, "output": output_items,
            "usage": {
                "input_tokens": usage_value("prompt_tokens"),
                "output_tokens": usage_value("completion_tokens"),
                "total_tokens": usage_value("total_tokens"),
            }
        }}),
    );
    output.push_str("event: ping\ndata: {\"type\":\"ping\",\"cost\":\"0\"}\n\n");
    output
}

fn merge_status(value: &Value, status: &str) -> Value {
    let mut merged = value.clone();
    if let Value::Object(object) = &mut merged {
        object.insert("status".to_owned(), json!(status));
    }
    merged
}

fn parse_sse_block(block: &str) -> (Option<String>, Option<Value>) {
    let mut event_name = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim().to_owned());
        }
    }
    if data_lines.is_empty() {
        return (event_name, None);
    }
    let data_text = data_lines.join("\n");
    (event_name, serde_json::from_str(&data_text).ok())
}

#[derive(Clone, Copy, PartialEq)]
enum PatchMode {
    Auto,
    Patch,
}

pub struct SsePatcher {
    model: Option<String>,
    mode: PatchMode,
    saw_message_item: bool,
    created_seen: bool,
    item_id: Option<String>,
    item_output_index: Value,
    text_parts: Vec<String>,
    done_emitted: bool,
    response_id: Option<String>,
    emitter: Emitter,
}

fn join_text_parts(parts: &[String]) -> String {
    let capacity = parts.iter().map(String::len).sum();
    let mut text = String::with_capacity(capacity);
    for part in parts {
        text.push_str(part);
    }
    text
}

impl SsePatcher {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: model.map(str::to_owned),
            mode: PatchMode::Auto,
            saw_message_item: false,
            created_seen: false,
            item_id: None,
            item_output_index: Value::Null,
            text_parts: Vec::new(),
            done_emitted: false,
            response_id: None,
            emitter: Emitter::new(),
        }
    }

    pub fn is_patch_mode(&self) -> bool {
        self.mode == PatchMode::Patch
    }

    pub fn process(&mut self, block: &str) -> String {
        let (event_name, object) = parse_sse_block(block);
        let event = event_name.as_deref();
        if self.mode == PatchMode::Auto {
            match event {
                Some("response.created") if object.is_some() => self.created_seen = true,
                Some("response.output_item.added") if object.is_some() => {
                    if object.as_ref().is_some_and(|value| {
                        value.pointer("/item/type").and_then(Value::as_str) == Some("message")
                    }) {
                        self.saw_message_item = true;
                    }
                }
                Some("response.output_text.delta")
                    if object.is_some() && !self.saw_message_item =>
                {
                    self.mode = PatchMode::Patch;
                }
                _ => {}
            }
        }

        match self.mode {
            PatchMode::Auto => format!("{block}\n\n"),
            PatchMode::Patch => self.process_patch(block, event, object),
        }
    }

    fn process_patch(&mut self, block: &str, event: Option<&str>, object: Option<Value>) -> String {
        let mut output = String::new();
        match (event, object) {
            (Some("response.created"), Some(_)) => {
                self.created_seen = true;
                output.push_str(block);
                output.push_str("\n\n");
            }
            (Some("response.output_item.added"), Some(object)) => {
                if object.pointer("/item/type").and_then(Value::as_str) == Some("message") {
                    self.item_id = object
                        .pointer("/item/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.item_output_index =
                        object.get("output_index").cloned().unwrap_or(Value::Null);
                    self.text_parts.clear();
                    self.done_emitted = false;
                }
                output.push_str(block);
                output.push_str("\n\n");
            }
            (Some("response.output_text.delta"), Some(mut object)) => {
                let delta = object
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if self.item_id.is_none() {
                    let response_id = object
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| self.response_id.clone())
                        .unwrap_or_else(|| short_id("resp"));
                    self.response_id = Some(response_id.clone());
                    if !self.created_seen {
                        let now = unix_now();
                        output += &self.emitter.emit(
                            "response.created",
                            json!({"response": {
                                "id": response_id, "object": "response", "created_at": now,
                                "status": "in_progress", "model": self.model, "output": []
                            }}),
                        );
                        output += &self.emitter.emit(
                            "response.in_progress",
                            json!({"response": {
                                "id": response_id, "object": "response", "status": "in_progress", "model": self.model
                            }}),
                        );
                        self.created_seen = true;
                    }
                    let item_id = short_id("msg");
                    let output_index = object
                        .get("output_index")
                        .cloned()
                        .filter(|value| !value.is_null())
                        .unwrap_or(json!(0));
                    let item_id_clone = item_id.clone();
                    let output_index_clone = output_index.clone();
                    output += &self.emitter.emit(
                        "response.output_item.added",
                        json!({
                            "output_index": output_index_clone,
                            "item": {
                                "id": item_id_clone, "type": "message", "status": "in_progress",
                                "role": "assistant", "phase": "final_answer", "content": []
                            }
                        }),
                    );
                    output += &self.emitter.emit(
                        "response.content_part.added",
                        json!({
                            "output_index": output_index_clone, "content_index": 0, "item_id": item_id_clone,
                            "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
                        }),
                    );
                    self.item_id = Some(item_id);
                    self.item_output_index = output_index;
                }
                self.text_parts.push(delta);
                let item_id = self.item_id.clone().unwrap_or_default();
                if let Value::Object(fields) = &mut object {
                    fields.remove("sequence_number");
                    fields.insert("item_id".to_owned(), json!(item_id));
                    fields.insert("output_index".to_owned(), self.item_output_index.clone());
                    fields.insert("content_index".to_owned(), json!(0));
                }
                output += &self.emitter.emit("response.output_text.delta", object);
            }
            (
                Some(
                    "response.output_text.done"
                    | "response.content_part.done"
                    | "response.output_item.done",
                ),
                Some(_),
            ) => {
                self.done_emitted = true;
                output.push_str(block);
                output.push_str("\n\n");
            }
            (Some("response.completed"), Some(object)) => {
                if self.item_id.is_some() && !self.done_emitted {
                    let full_text = join_text_parts(&self.text_parts);
                    let item_id = self.item_id.clone().unwrap_or_default();
                    let output_index = self.item_output_index.clone();
                    output += &self.emitter.emit(
                        "response.output_text.done",
                        json!({
                            "output_index": output_index, "content_index": 0, "item_id": item_id,
                            "text": full_text, "logprobs": []
                        }),
                    );
                    output += &self.emitter.emit(
                        "response.content_part.done",
                        json!({
                            "output_index": output_index, "content_index": 0, "item_id": item_id,
                            "part": {"type": "output_text", "text": full_text, "annotations": [], "logprobs": []}
                        }),
                    );
                    output += &self.emitter.emit(
                        "response.output_item.done",
                        json!({
                            "output_index": output_index,
                            "item": {
                                "id": item_id, "type": "message", "status": "completed", "role": "assistant",
                                "phase": "final_answer",
                                "content": [{"type": "output_text", "text": full_text, "annotations": [], "logprobs": []}]
                            }
                        }),
                    );
                    self.done_emitted = true;
                }

                let mut patched_response =
                    object.get("response").cloned().unwrap_or_else(|| json!({}));
                if let Value::Object(response) = &mut patched_response {
                    response
                        .entry("object".to_owned())
                        .or_insert(json!("response"));
                    response
                        .entry("status".to_owned())
                        .or_insert(json!("completed"));
                    response
                        .entry("created_at".to_owned())
                        .or_insert(json!(unix_now()));
                    response
                        .entry("completed_at".to_owned())
                        .or_insert(json!(unix_now()));
                    let item_id = self.item_id.clone();
                    let output = response
                        .entry("output".to_owned())
                        .or_insert_with(|| json!([]));
                    if let Value::Array(items) = output {
                        let expected_id = item_id.as_deref();
                        let exists = items.iter().any(|item| {
                            item.get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|found| Some(found) == expected_id)
                        });
                        if !exists {
                            let full_text = join_text_parts(&self.text_parts);
                            if let Some(item_id) = &item_id {
                                items.push(json!({
                                    "id": item_id, "type": "message", "status": "completed", "role": "assistant",
                                    "phase": "final_answer",
                                    "content": [{"type": "output_text", "text": full_text, "annotations": [], "logprobs": []}]
                                }));
                            }
                        }
                    }
                }
                output += &self
                    .emitter
                    .emit("response.completed", json!({"response": patched_response}));
            }
            _ => {
                output.push_str(block);
                output.push_str("\n\n");
            }
        }
        output
    }
}

#[derive(Clone, Default)]
struct AccumulatedToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

pub struct ChatStreamAssembler {
    model: Option<String>,
    response_id: String,
    emitter: Emitter,
    started: bool,
    message_id: Option<String>,
    message_index: usize,
    text_parts: Vec<String>,
    tool_calls: HashMap<usize, AccumulatedToolCall>,
    tool_order: Vec<usize>,
    output_state: Box<OutputOrderState>,
    usage: Value,
    cost: Option<Value>,
    finished: bool,
    pub finish_reason: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Copy)]
enum OutputSlot {
    Tool(usize),
    Message,
}

struct OutputOrderState {
    tool_seen: HashSet<usize>,
    output_order: Vec<OutputSlot>,
}

impl ChatStreamAssembler {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            model: model.map(str::to_owned),
            response_id: short_id("resp"),
            emitter: Emitter::new(),
            started: false,
            message_id: None,
            message_index: 0,
            text_parts: Vec::new(),
            tool_calls: HashMap::new(),
            tool_order: Vec::new(),
            output_state: Box::new(OutputOrderState {
                tool_seen: HashSet::new(),
                output_order: Vec::new(),
            }),
            usage: Value::Null,
            cost: None,
            finished: false,
            finish_reason: None,
            truncated: false,
        }
    }

    pub fn done(&self) -> bool {
        self.finished
    }

    /// Terminate a stream whose upstream ended before a normal finish marker.
    ///
    /// Partial deltas already emitted remain visible to the consumer, but no
    /// completion event is produced so the caller can retry the response.
    pub fn finish_eof(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        self.truncated = true;
        eprintln!(
            "[proxy] translate truncated: upstream EOF finish_reason={:?} text_chars={} tool_calls={} usage={}",
            self.finish_reason,
            self.text_parts.iter().map(String::len).sum::<usize>(),
            self.tool_order.len(),
            self.usage
        );
        String::new()
    }

    pub fn process(&mut self, raw: &str) -> String {
        let mut output = String::new();
        for block in raw.split("\n\n").filter(|block| !block.trim().is_empty()) {
            if self.finished {
                break;
            }
            output.push_str(&self.process_block(block));
        }
        output
    }

    fn process_block(&mut self, block: &str) -> String {
        if block.trim() == "data: [DONE]" {
            return self.finish();
        }
        let (_, object) = parse_sse_block(block);
        let Some(object) = object else {
            return String::new();
        };
        if !object.is_object() {
            return String::new();
        }
        if let Some(cost) = object.get("cost") {
            self.cost = Some(cost.clone());
        }
        let Some(choice) = object
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return String::new();
        };
        let mut output = String::new();
        if let Some(usage) = choice.get("usage") {
            self.usage = usage.clone();
        }
        if let Some(usage) = object.get("usage") {
            self.usage = usage.clone();
        }
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    output += &self.content(content);
                }
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .unwrap_or(&vec![])
            {
                self.accumulate_tool(call);
            }
        }
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish_reason.to_owned());
            output += &self.finish();
        }
        output
    }

    fn ensure_started(&mut self) -> String {
        if self.started {
            return String::new();
        }
        self.started = true;
        let now = unix_now();
        let response_id = self.response_id.clone();
        let model = self.model.clone();
        self.emitter.emit(
            "response.created",
            json!({"response": {
                "id": response_id, "object": "response", "created_at": now, "status": "in_progress",
                "model": model, "output": []
            }}),
        ) + &self.emitter.emit(
            "response.in_progress",
            json!({"response": {
                "id": response_id, "object": "response", "status": "in_progress", "model": model
            }}),
        )
    }

    fn content(&mut self, text: &str) -> String {
        let mut output = self.ensure_started();
        if self.message_id.is_none() {
            let item_id = short_id("msg");
            let index = self.output_state.output_order.len();
            let id_for_events = item_id.clone();
            let index_for_events = index;
            output += &self.emitter.emit(
                "response.output_item.added",
                json!({
                    "output_index": index_for_events,
                    "item": {
                        "id": id_for_events, "type": "message", "status": "in_progress",
                        "role": "assistant", "phase": "final_answer", "content": []
                    }
                }),
            );
            output += &self.emitter.emit(
                "response.content_part.added",
                json!({
                    "output_index": index_for_events, "content_index": 0, "item_id": id_for_events,
                    "part": {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
                }),
            );
            self.message_id = Some(item_id);
            self.message_index = index;
            self.output_state.output_order.push(OutputSlot::Message);
        }
        self.text_parts.push(text.to_owned());
        let item_id = self.message_id.clone().unwrap_or_default();
        output
            + &self.emitter.emit(
                "response.output_text.delta",
                json!({
                    "output_index": self.message_index, "content_index": 0,
                    "item_id": item_id, "delta": text
                }),
            )
    }

    fn accumulate_tool(&mut self, call: &Value) {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(self.tool_order.len());
        if self.output_state.tool_seen.insert(index) {
            self.tool_order.push(index);
            self.output_state.output_order.push(OutputSlot::Tool(index));
        }
        let slot = self.tool_calls.entry(index).or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            slot.id = Some(id.to_owned());
        }
        let function = call.get("function");
        if let Some(name) = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
        {
            slot.name.push_str(name);
        }
        if let Some(arguments) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
        {
            slot.arguments.push_str(arguments);
        }
    }

    fn finish(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        let full_text = join_text_parts(&self.text_parts);
        let abnormal = finish_is_truncated(
            self.finish_reason.as_deref(),
            full_text.chars().count(),
            !self.tool_order.is_empty(),
        );
        if abnormal {
            self.truncated = true;
            eprintln!(
                "[proxy] translate truncated: reason={:?} text_chars={} tool_calls={} usage={}",
                self.finish_reason,
                full_text.chars().count(),
                self.tool_order.len(),
                self.usage
            );
            return String::new();
        }

        let mut output = self.ensure_started();
        let now = unix_now();
        let mut items = Vec::new();
        for slot in self.output_state.output_order.iter().copied() {
            match slot {
                OutputSlot::Tool(index) => {
                    let call = self.tool_calls.get(&index).cloned().unwrap_or_default();
                    let call_id = call.id.unwrap_or_else(|| short_id("call"));
                    let item = json!({
                        "id": call_id, "type": "function_call", "call_id": call_id,
                        "name": if call.name.is_empty() { Value::Null } else { json!(call.name) },
                        "arguments": call.arguments,
                    });
                    let item_index = items.len();
                    output += &self.emitter.emit(
                        "response.output_item.added",
                        json!({"output_index": item_index, "item": merge_status(&item, "in_progress")}),
                    );
                    output += &self.emitter.emit(
                        "response.output_item.done",
                        json!({"output_index": item_index, "item": merge_status(&item, "completed")}),
                    );
                    items.push(item);
                }
                OutputSlot::Message => {
                    let Some(message_id) = &self.message_id else {
                        continue;
                    };
                    let message_index = items.len();
                    output += &self.emitter.emit(
                        "response.output_text.done",
                        json!({
                            "output_index": message_index, "content_index": 0, "item_id": message_id,
                            "text": full_text, "logprobs": []
                        }),
                    );
                    output += &self.emitter.emit(
                        "response.content_part.done",
                        json!({
                            "output_index": message_index, "content_index": 0, "item_id": message_id,
                            "part": {"type": "output_text", "text": full_text, "annotations": [], "logprobs": []}
                        }),
                    );
                    let done_item = json!({
                        "id": message_id, "type": "message", "status": "completed", "role": "assistant",
                        "phase": "final_answer",
                        "content": [{"type": "output_text", "text": full_text, "annotations": [], "logprobs": []}]
                    });
                    output += &self.emitter.emit(
                        "response.output_item.done",
                        json!({"output_index": message_index, "item": done_item}),
                    );
                    items.push(done_item);
                }
            }
        }
        let usage = if self.usage.is_object() {
            self.usage.clone()
        } else {
            json!({})
        };
        let usage_value = |field: &str| usage.get(field).and_then(Value::as_u64).unwrap_or(0);
        output += &self.emitter.emit(
            "response.completed",
            json!({"response": {
                "id": self.response_id, "object": "response", "created_at": now, "completed_at": now,
                "status": "completed", "model": self.model, "output": items,
                "usage": {
                    "input_tokens": usage_value("prompt_tokens"),
                    "output_tokens": usage_value("completion_tokens"),
                    "total_tokens": usage_value("total_tokens"),
                }
            }}),
        );
        let cost = self
            .cost
            .clone()
            .map(|value| value.to_string().trim_matches('"').to_owned())
            .unwrap_or_else(|| "0".to_owned());
        output.push_str(&format!(
            "event: ping\ndata: {{\"type\":\"ping\",\"cost\":\"{cost}\"}}\n\n"
        ));
        output
    }
}
