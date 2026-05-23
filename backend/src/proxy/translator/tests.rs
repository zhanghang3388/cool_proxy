use serde_json::Value;

use super::request::translate_request;
use super::response_aggregate::Aggregator;
use super::response_stream::StreamTranslator;

fn parse(s: &str) -> Value {
    serde_json::from_str(s).expect("fixture must be valid JSON")
}

#[test]
fn req_simple_text_matches_fixture() {
    let in_body = include_str!("./fixtures/req_simple_text.in.json").as_bytes();
    let expected = parse(include_str!("./fixtures/req_simple_text.expected.json"));
    let got = translate_request("gpt-5-codex", in_body, true);
    assert_eq!(
        got,
        expected,
        "got = {}",
        serde_json::to_string_pretty(&got).unwrap()
    );
}

#[test]
fn req_with_tools_matches_fixture() {
    let in_body = include_str!("./fixtures/req_with_tools.in.json").as_bytes();
    let expected = parse(include_str!("./fixtures/req_with_tools.expected.json"));
    let got = translate_request("gpt-5-codex", in_body, true);
    assert_eq!(
        got,
        expected,
        "got = {}",
        serde_json::to_string_pretty(&got).unwrap()
    );
}

#[test]
fn req_long_tool_name_is_shortened_consistently() {
    // 模拟一个超长 tool name，确保 tool_choice / messages 里的引用一致
    let long_name = "mcp__some_long_server__".to_string() + &"x".repeat(80);
    let in_body = serde_json::json!({
        "model": "gpt-5-codex",
        "messages": [{"role": "user", "content": "go"}],
        "tools": [{
            "type": "function",
            "function": {"name": long_name, "parameters": {"type": "object"}}
        }],
        "tool_choice": {"type": "function", "function": {"name": long_name}},
        "stream": true
    })
    .to_string();
    let got = translate_request("gpt-5-codex", in_body.as_bytes(), true);
    let tool_name = got["tools"][0]["name"].as_str().unwrap();
    let choice_name = got["tool_choice"]["name"].as_str().unwrap();
    assert!(tool_name.len() <= 64);
    assert_eq!(tool_name, choice_name);
    assert_ne!(tool_name, long_name);
}

#[test]
fn req_reasoning_effort_defaults_to_medium() {
    let in_body = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let got = translate_request("m", in_body, true);
    assert_eq!(got["reasoning"]["effort"], "medium");
    assert_eq!(got["reasoning"]["summary"], "auto");
    assert_eq!(got["include"][0], "reasoning.encrypted_content");
    assert_eq!(got["parallel_tool_calls"], true);
    assert_eq!(got["store"], false);
}

#[test]
fn req_multimodal_image_url_and_file_pass_through() {
    let in_body = include_str!("./fixtures/req_multimodal.in.json").as_bytes();
    let expected = parse(include_str!("./fixtures/req_multimodal.expected.json"));
    let got = translate_request("gpt-5", in_body, true);
    assert_eq!(
        got,
        expected,
        "got = {}",
        serde_json::to_string_pretty(&got).unwrap()
    );
}

#[test]
fn req_response_format_json_schema_maps_to_text_format() {
    let in_body = include_str!("./fixtures/req_response_format.in.json").as_bytes();
    let expected = parse(include_str!("./fixtures/req_response_format.expected.json"));
    let got = translate_request("gpt-5", in_body, true);
    assert_eq!(
        got,
        expected,
        "got = {}",
        serde_json::to_string_pretty(&got).unwrap()
    );
}

fn run_stream_translator(events: &str, original_request: &[u8]) -> Vec<Value> {
    let mut tr = StreamTranslator::new("gpt-5-codex", original_request);
    let mut chunks: Vec<Value> = Vec::new();
    for line in events.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: Value = serde_json::from_str(line).unwrap();
        for c in tr.push(&ev) {
            chunks.push(c);
        }
    }
    chunks
}

#[test]
fn stream_text_emits_content_deltas_and_finish_stop() {
    let events = include_str!("./fixtures/resp_text_stream.events.jsonl");
    let chunks = run_stream_translator(events, b"{}");
    // 期望：两条 content delta + 一条 finish_reason=stop（带 usage）
    assert_eq!(chunks.len(), 3, "got {} chunks: {:?}", chunks.len(), chunks);
    assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], ", world!");
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
    assert_eq!(chunks[2]["usage"]["prompt_tokens"], 10);
    assert_eq!(chunks[2]["usage"]["completion_tokens"], 4);
    assert_eq!(chunks[2]["usage"]["total_tokens"], 14);
    // id 应该来自 response.created
    for c in &chunks {
        assert_eq!(c["id"], "resp_123");
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["model"], "gpt-5-codex");
    }
}

#[test]
fn stream_tool_call_announces_then_streams_args_then_finish_tool_calls() {
    let events = include_str!("./fixtures/resp_tool_call_stream.events.jsonl");
    let original_request = br#"{"tools":[{"type":"function","function":{"name":"calc"}}]}"#;
    let chunks = run_stream_translator(events, original_request);
    // 期望：announce(name=calc,id=call_1) + 两条 arg delta + finish_reason=tool_calls
    // function_call_arguments.done 因为已经收到 delta 应该被吃掉
    assert_eq!(chunks.len(), 4, "got {} chunks: {:?}", chunks.len(), chunks);
    let announce = &chunks[0]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(announce["index"], 0);
    assert_eq!(announce["id"], "call_1");
    assert_eq!(announce["type"], "function");
    assert_eq!(announce["function"]["name"], "calc");
    assert_eq!(announce["function"]["arguments"], "");

    let d1 = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(d1["function"]["arguments"], "{\"expr");
    let d2 = &chunks[2]["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(d2["function"]["arguments"], "\":\"2+2\"}");

    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn aggregate_text_stream_into_chat_completion() {
    let events = include_str!("./fixtures/resp_text_stream.events.jsonl");
    let mut agg = Aggregator::new("gpt-5-codex", b"{}");
    for line in events.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: Value = serde_json::from_str(line).unwrap();
        agg.push(&ev);
    }
    assert!(agg.is_completed());
    let out = agg.finalize();
    assert_eq!(out["object"], "chat.completion");
    assert_eq!(out["choices"][0]["message"]["role"], "assistant");
    assert_eq!(out["choices"][0]["message"]["content"], "Hello, world!");
    assert_eq!(out["choices"][0]["finish_reason"], "stop");
    assert_eq!(out["usage"]["prompt_tokens"], 10);
    assert_eq!(out["usage"]["completion_tokens"], 4);
    assert_eq!(out["id"], "resp_123");
}

#[test]
fn aggregate_tool_call_stream_into_chat_completion() {
    let events = include_str!("./fixtures/resp_tool_call_stream.events.jsonl");
    let original_request = br#"{"tools":[{"type":"function","function":{"name":"calc"}}]}"#;
    let mut agg = Aggregator::new("gpt-5-codex", original_request);
    for line in events.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: Value = serde_json::from_str(line).unwrap();
        agg.push(&ev);
    }
    let out = agg.finalize();
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    let tool_calls = &out["choices"][0]["message"]["tool_calls"];
    assert!(tool_calls.is_array());
    assert_eq!(tool_calls[0]["id"], "call_1");
    assert_eq!(tool_calls[0]["function"]["name"], "calc");
    assert_eq!(tool_calls[0]["function"]["arguments"], "{\"expr\":\"2+2\"}");
    // tool-only 响应：content 应为 null
    assert!(out["choices"][0]["message"]["content"].is_null());
}

#[test]
fn stream_image_generation_dedup_and_emits_data_url() {
    let events = include_str!("./fixtures/resp_image_stream.events.jsonl");
    let chunks = run_stream_translator(events, b"{}");
    // 期望：一条 text delta + 第一条 image partial + 第三条 image partial（第二条与第一条重复被吃掉）
    // + finish_reason=stop。重复的不输出，所以总共 4 条。
    assert_eq!(chunks.len(), 4, "got {} chunks: {:?}", chunks.len(), chunks);
    assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "sure");
    let img1 = &chunks[1]["choices"][0]["delta"]["images"][0];
    assert_eq!(img1["type"], "image_url");
    assert_eq!(
        img1["image_url"]["url"].as_str().unwrap(),
        "data:image/png;base64,AAAA"
    );
    let img2 = &chunks[2]["choices"][0]["delta"]["images"][0];
    assert_eq!(
        img2["image_url"]["url"].as_str().unwrap(),
        "data:image/png;base64,BBBB"
    );
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "stop");
}

#[test]
fn aggregate_image_generation_collects_unique_images() {
    let events = include_str!("./fixtures/resp_image_stream.events.jsonl");
    let mut agg = Aggregator::new("gpt-5", b"{}");
    for line in events.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ev: Value = serde_json::from_str(line).unwrap();
        agg.push(&ev);
    }
    let out = agg.finalize();
    let imgs = &out["choices"][0]["message"]["images"];
    assert!(imgs.is_array(), "got {imgs:?}");
    let arr = imgs.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(
        arr[0]["image_url"]["url"].as_str().unwrap(),
        "data:image/png;base64,AAAA"
    );
    assert_eq!(
        arr[1]["image_url"]["url"].as_str().unwrap(),
        "data:image/png;base64,BBBB"
    );
}
