//! 仅测试使用本地 HTTP 服务；运行 deepseek 示例始终请求官方接口。

use super::*;
use pi_agent_loop::tools::{Add, default_registry};
use pi_agent_loop::{AgentContext, AgentTool, EndReason, run_agent_loop};
use std::env;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[test]
fn local_configuration_supports_files_environment_overrides_and_safe_errors() {
    let path = env::temp_dir().join(format!(
        "pi-deepseek-env-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    assert!(configuration(&path, |_| None).is_err());
    assert_eq!(
        configuration(&path, |name| (name == "DEEPSEEK_API_KEY")
            .then(|| "env-key".into()))
        .unwrap(),
        ("env-key".into(), "deepseek-flash".into()),
    );
    fs::write(
        &path,
        "# Local configuration\nDEEPSEEK_API_KEY='file-key'\nDEEPSEEK_MODEL=\"file-model\"\n",
    )
    .unwrap();
    assert_eq!(
        configuration(&path, |_| None).unwrap(),
        ("file-key".into(), "file-model".into())
    );
    assert_eq!(
        configuration(&path, |name| Some(format!("override-{name}"))).unwrap(),
        (
            "override-DEEPSEEK_API_KEY".into(),
            "override-DEEPSEEK_MODEL".into()
        ),
    );
    fs::write(&path, "DEEPSEEK_API_KEY='secret-key\n").unwrap();
    let error = configuration(&path, |_| None).unwrap_err();
    assert!(error.0.contains("Invalid configuration syntax"));
    assert!(!error.0.contains("secret-key"));
    fs::write(&path, "DEEPSEEK_API_KEY=\n").unwrap();
    assert!(configuration(&path, |_| None).is_err());
    fs::remove_file(&path).unwrap();
}

fn model(endpoint: String) -> DeepSeek {
    DeepSeek {
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        endpoint,
        api_key: "test-key".into(),
        model: "deepseek-flash".into(),
        requests: 0,
        demo: true,
    }
}

fn tool_response(arguments: &str, finish_reason: &str) -> Value {
    json!({"choices": [{
        "finish_reason": finish_reason,
        "message": {
            "content": null,
            "tool_calls": [{"id": "call-1", "type": "function", "function": {
                "name": "add", "arguments": arguments,
            }}],
        },
    }]})
}

async fn serve(listener: TcpListener, replies: Vec<(u16, Value)>) -> Vec<Value> {
    let mut requests = Vec::new();
    for (status, reply) in replies {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, "POST /chat/completions HTTP/1.1\r\n");
        let mut length = None;
        let mut authorized = false;
        loop {
            line.clear();
            assert!(stream.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
            if name.eq_ignore_ascii_case("authorization") {
                authorized = value.trim() == "Bearer test-key";
            }
        }
        assert!(authorized);
        let mut body = vec![0; length.unwrap()];
        stream.read_exact(&mut body).await.unwrap();
        requests.push(serde_json::from_slice(&body).unwrap());
        let reply = reply.to_string();
        let response = format!(
            "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len(),
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
    requests
}

#[tokio::test]
async fn http_round_trip_executes_add_and_sends_its_result_back() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = model(format!(
        "http://{}/chat/completions",
        listener.local_addr().unwrap()
    ));
    let server = tokio::spawn(serve(
        listener,
        vec![
            (200, tool_response(r#"{"a":1,"b":2}"#, "tool_calls")),
            (
                200,
                json!({"choices": [{"finish_reason": "stop", "message": {"content": "答案是 3", "tool_calls": null}}]}),
            ),
        ],
    ));
    let tools = default_registry().unwrap();
    let outcome = timeout(
        Duration::from_secs(5),
        run_agent_loop(
            vec![Message::User {
                content: "计算 1 + 2".into(),
            }],
            AgentContext {
                messages: vec![Message::System {
                    content: "使用 add 工具。".into(),
                }],
            },
            &mut model,
            &tools,
            &CancellationToken::new(),
            &mut |_| {},
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let requests = timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(model.requests, 2);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "deepseek-flash");
    assert_eq!(requests[0]["thinking"]["type"], "disabled");
    assert_eq!(requests[0]["tool_choice"], "required");
    assert_eq!(requests[0]["tools"][0]["function"]["name"], "add");
    assert_eq!(requests[1]["tool_choice"], "auto");
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call-1");
    assert_eq!(
        serde_json::from_str::<Value>(
            messages[2]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        json!({"a": 1, "b": 2})
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call-1", "content": "3"})
    );
    assert!(
        matches!(&outcome.new_messages[2], Message::ToolResult(result)
        if !result.result.is_error && result.result.details == json!({"sum": 3}))
    );
}

#[tokio::test]
async fn chat_uses_auto_tools_and_retains_history_beyond_demo_request_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = model(format!(
        "http://{}/chat/completions",
        listener.local_addr().unwrap()
    ));
    model.demo = false;
    let server = tokio::spawn(serve(
        listener,
        (0..6)
            .map(|index| {
                (
                    200,
                    json!({"choices": [{
                        "finish_reason": "stop", "message": {"content": format!("回答 {index}")}
                    }]}),
                )
            })
            .collect(),
    ));
    let tools = default_registry().unwrap();
    let mut context = AgentContext::default();
    for index in 0..6 {
        let outcome = run_agent_loop(
            vec![Message::User {
                content: format!("问题 {index}"),
            }],
            context,
            &mut model,
            &tools,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(outcome.reason, EndReason::Completed);
        context = outcome.context;
    }
    let requests = timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 6);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request["tool_choice"], "auto");
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), index * 2 + 1);
        assert_eq!(messages[0]["content"], "问题 0");
        if index > 0 {
            assert_eq!(
                messages[index * 2 - 1]["content"],
                format!("回答 {}", index - 1)
            );
        }
        assert_eq!(messages.last().unwrap()["content"], format!("问题 {index}"));
    }
}

#[test]
fn malformed_arguments_and_truncation_cannot_become_executable_calls() {
    for (arguments, reason) in [("{", "tool_calls"), (r#"{"a":1,"b":2}"#, "length")] {
        let message =
            parse_response(serde_json::from_value(tool_response(arguments, reason)).unwrap())
                .unwrap();
        let AssistantContent::ToolCall(call) = &message.content[0] else {
            panic!("missing tool call")
        };
        if reason == "tool_calls" {
            assert!(call.arguments_error.is_some());
        } else {
            assert_eq!(message.stop_reason, StopReason::Length);
        }
        let mut core =
            pi_agent_loop::Core::new(Vec::new(), AgentContext::default(), vec![Add.declaration()])
                .unwrap();
        core.advance(pi_agent_loop::Input::Start { cancelled: false })
            .unwrap();
        let step = core
            .advance(pi_agent_loop::Input::ModelReturned {
                message,
                cancelled: false,
            })
            .unwrap();
        assert!(matches!(
            step.effect,
            Some(pi_agent_loop::Effect::RequestModel(_))
        ));
        assert!(core.context().messages.iter().any(
            |message| matches!(message, Message::ToolResult(result) if result.result.is_error)
        ));
    }
    for response in [
        json!({"choices": []}),
        json!({"choices": [{"finish_reason": "content_filter", "message": {"content": null}}]}),
    ] {
        assert!(parse_response(serde_json::from_value(response).unwrap()).is_err());
    }
}

#[tokio::test]
async fn authentication_errors_fail_without_echoing_the_response_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = model(format!(
        "http://{}/chat/completions",
        listener.local_addr().unwrap()
    ));
    let server = tokio::spawn(serve(
        listener,
        vec![(401, json!({"error": {"message": "secret-key"}}))],
    ));
    let error = model
        .complete(
            ModelRequest {
                messages: Vec::new(),
                tools: Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.0.contains("401"));
    assert!(!error.0.contains("secret-key"));
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_http_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = model(format!(
        "http://{}/chat/completions",
        listener.local_addr().unwrap()
    ));
    let token = CancellationToken::new();
    let request = model.complete(
        ModelRequest {
            messages: Vec::new(),
            tools: Vec::new(),
        },
        &token,
    );
    let cancel = async {
        let (_socket, _) = listener.accept().await.unwrap();
        token.cancel();
    };
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(request, cancel)
    })
    .await
    .unwrap();
    assert_eq!(result, Err(OperationError("Operation aborted".into())));
}
