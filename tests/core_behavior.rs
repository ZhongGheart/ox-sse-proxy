use ox_sse_proxy::{
    chat_to_sse, fix_tool_order, sanitize_input_items, translate_request, ChatStreamAssembler,
    SsePatcher,
};
use serde_json::{json, Value};

#[test]
fn sse_patcher_adds_missing_message_events() {
    let raw = concat!(
        "event: response.created\ndata: {\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"delta\":\"你好\"}\n\n",
        "event: response.completed\ndata: {\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    let mut patcher = SsePatcher::new(Some("ox-alpha-free"));
    let patched = raw
        .split("\n\n")
        .filter(|block| !block.is_empty())
        .map(|block| patcher.process(block))
        .collect::<String>();
    assert!(patcher.is_patch_mode());
    assert!(patched.contains("response.output_item.added"));
    assert!(patched.contains("\"type\":\"message\""));
    assert!(patched.contains("response.output_text.done"));
    assert!(patched.contains("response.output_item.done"));
}

#[test]
fn sse_patcher_passes_complete_message_events_through() {
    let raw = concat!(
        "event: response.created\ndata: {\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_item.added\ndata: {\"item\":{\"id\":\"m1\",\"type\":\"message\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"delta\":\"你好\"}\n\n",
        "event: response.completed\ndata: {\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[]}}\n\n",
    );
    let mut patcher = SsePatcher::new(Some("ox-alpha-free"));
    let actual = raw
        .split("\n\n")
        .filter(|block| !block.is_empty())
        .map(|block| patcher.process(block))
        .collect::<String>();
    assert_eq!(actual, raw);
}

#[test]
fn translates_responses_request_to_chat_request() {
    let responses = json!({
        "model": "ox-alpha-free",
        "instructions": "你是助手",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call_output", "call_id": "c1", "output": {"output": "42"}}
        ],
        "tools": [{"type":"function","name":"f1","description":"d","parameters":{"type":"object"}}],
        "max_output_tokens": 123,
        "temperature": 0.7,
        "reasoning": {"effort": "high"}
    });
    let chat = translate_request(&responses, 16384);
    assert_eq!(chat["max_tokens"], json!(123));
    assert_eq!(chat["temperature"], json!(0.7));
    assert_eq!(chat["reasoning"], json!({"effort":"high"}));
    assert_eq!(chat["tools"][0]["function"]["name"], json!("f1"));
    assert!(chat["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == json!("system") && message["content"] == json!("你是助手")
    }));
    assert!(chat["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| { message["role"] == json!("tool") && message["content"] == json!("42") }));

    let without_limit = translate_request(&json!({"model":"m","input":[]}), 16384);
    assert_eq!(without_limit["max_tokens"], json!(16384));
}

#[test]
fn sanitizes_orphan_tool_calls() {
    let mut items = vec![
        json!({"type":"message","role":"assistant","content":[{"type":"function_call","call_id":"c1"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"function_call","call_id":"c2"}]}),
        json!({"type":"function_call_output","call_id":"c2","output":"ok"}),
    ];
    let changed = sanitize_input_items(&mut items);
    assert!(changed);
    assert!(!Value::Array(items.clone()).to_string().contains("c1"));
    assert!(Value::Array(items).to_string().contains("c2"));
}

#[test]
fn fixes_interleaved_and_parallel_tool_order() {
    let mut items = vec![
        json!({"type":"function_call","call_id":"c1"}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"先看"}]}),
        json!({"type":"function_call_output","call_id":"c1"}),
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"继续"}]}),
    ];
    assert!(fix_tool_order(&mut items));
    let types: Vec<_> = items
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "function_call",
            "function_call_output",
            "message",
            "message"
        ]
    );

    let mut parallel = vec![
        json!({"type":"function_call","call_id":"a"}),
        json!({"type":"function_call","call_id":"b"}),
        json!({"type":"message","content":[{"type":"output_text","text":"并行"}],"role":"assistant"}),
        json!({"type":"function_call_output","call_id":"a"}),
        json!({"type":"function_call_output","call_id":"b"}),
    ];
    assert!(fix_tool_order(&mut parallel));
    let types: Vec<_> = parallel
        .iter()
        .map(|item| item["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output",
            "message"
        ]
    );
}

#[test]
fn converts_nonstream_chat_to_responses_sse() {
    let chat = json!({
        "choices":[{"message":{"role":"assistant","content":"回答"}}],
        "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
    });
    let sse = chat_to_sse(&chat, "ox-alpha-free");
    assert!(sse.contains("\"回答\""));
    assert!(sse.contains("response.completed"));
    assert!(sse.contains("response.output_item.added"));
    assert!(sse.contains("event: ping"));
}

#[test]
fn assembles_chat_stream_incrementally_and_detects_truncation() {
    let mut assembler = ChatStreamAssembler::new(Some("ox-alpha-free"));
    let out = assembler.process(concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ));
    assert!(out.contains("\"delta\":\"你\""));
    assert!(out.contains("\"delta\":\"好\""));
    assert!(out.contains("\"text\":\"你好\""));
    assert!(out.contains("response.completed"));
    assert!(out.contains("event: ping"));
    assert!(!assembler.truncated);

    let mut truncated = ChatStreamAssembler::new(Some("ox-alpha-free"));
    let out = truncated.process(concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"只\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"说一半\"},\"finish_reason\":\"length\"}]}",
    ));
    assert!(truncated.truncated);
    assert!(out.contains("\"delta\":\"只\""));
    assert!(!out.contains("response.completed"));
    assert!(!out.contains("response.output_text.done"));
    assert!(!out.contains("response.output_item.done"));
    assert!(!out.contains("\"type\":\"function_call\""));

    let mut long_length = ChatStreamAssembler::new(Some("ox-alpha-free"));
    let long_text = "长".repeat(200);
    long_length.process(&format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{long_text}\"}},\"finish_reason\":\"length\"}}]}}\n\n",
    ));
    assert!(!long_length.truncated);

    let mut eof = ChatStreamAssembler::new(Some("ox-alpha-free"));
    eof.process("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"部分\"}}]}");
    assert!(!eof.done());

    let mut network_error = ChatStreamAssembler::new(Some("ox-alpha-free"));
    network_error.process(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"network_error\"}]}",
    );
    assert!(network_error.truncated);
}

#[test]
fn mixed_chat_stream_assigns_unique_indices_and_preserves_arrival_order() {
    let mut assembler = ChatStreamAssembler::new(Some("ox-alpha-free"));
    let mut output =
        assembler.process("data: {\"choices\":[{\"delta\":{\"content\":\"先说\"}}]}\n\n");
    output.push_str(&assembler.process(&format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"lookup","arguments":"{\\\"q\\\":\\\"x\\\"}"}}]}}]})
    )));
    output.push_str(&assembler.process(
        "data: {\"choices\":[{\"delta\":{\"content\":\"后说\"},\"finish_reason\":\"tool_calls\"}]}\n\n",
    ));

    let completed = output
        .split("event: response.completed\n")
        .nth(1)
        .and_then(|event| event.lines().find(|line| line.starts_with("data: ")))
        .map(|line| serde_json::from_str::<Value>(&line[6..]).unwrap())
        .expect("completed response");
    let items = completed["response"]["output"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], json!("message"));
    assert_eq!(items[1]["type"], json!("function_call"));

    let indices = output
        .lines()
        .filter(|line| line.starts_with("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(&line[6..]).ok())
        .filter_map(|event| event.get("output_index").and_then(Value::as_u64))
        .collect::<Vec<_>>();
    assert!(indices.contains(&0));
    assert!(indices.contains(&1));
    assert!(!indices.iter().any(|index| *index > 1));
}

#[test]
fn eof_termination_marks_stream_truncated_without_completion_and_is_idempotent() {
    let mut assembler = ChatStreamAssembler::new(Some("ox-alpha-free"));
    let output = assembler.process("data: {\"choices\":[{\"delta\":{\"content\":\"部分\"}}]}\n\n");
    assert!(!assembler.done());

    let eof = assembler.finish_eof();
    assert!(assembler.done());
    assert!(assembler.truncated);
    assert!(!output.contains("response.completed"));
    assert!(!eof.contains("response.completed"));
    assert!(assembler.finish_eof().is_empty());
}

#[test]
fn nonstream_length_finish_reason_uses_stream_truncation_rule() {
    let chat = json!({
        "choices":[{"finish_reason":"length","message":{"role":"assistant","content":"只说一半"}}]
    });
    let sse = chat_to_sse(&chat, "ox-alpha-free");
    assert!(sse.contains("response.output_item.added"));
    assert!(sse.contains("response.content_part.added"));
    assert!(sse.contains("response.output_text.delta"));
    assert!(!sse.contains("response.output_text.done"));
    assert!(!sse.contains("response.content_part.done"));
    assert!(!sse.contains("response.output_item.done"));
    assert!(!sse.contains("response.completed"));
}
