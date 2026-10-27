//! 验证交互入口的上下文传递、命令、工具循环和失败后的下一次输入。

use super::*;
use std::collections::VecDeque;
use std::io::Cursor;

use pi_agent_loop::tools::Add;
use pi_agent_loop::{AgentTool, AgentToolResult, LoopError, ToolCall, ToolDeclaration};
use serde_json::{Value, json};

struct Script {
    replies: VecDeque<Result<AssistantMessage, OperationError>>,
    requests: Vec<ModelRequest>,
}

impl Model for Script {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>> {
        self.requests.push(request);
        let reply = self.replies.pop_front().expect("unexpected model request");
        Box::pin(async move { reply })
    }
}

fn answer(text: &str) -> Result<AssistantMessage, OperationError> {
    Ok(AssistantMessage {
        content: vec![AssistantContent::Text { text: text.into() }],
        stop_reason: StopReason::Stop,
        error_message: None,
    })
}

#[tokio::test]
async fn terminal_preserves_conversation_and_tool_results_then_resets() {
    let mut model = Script {
        replies: VecDeque::from([
            Ok(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1, "b": 2}),
                    arguments_error: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            }),
            answer("答案是 3"),
            answer("刚才的结果是 3"),
            answer("新会话没有上一轮历史"),
        ]),
        requests: Vec::new(),
    };
    let mut input =
        Cursor::new("\n/help\n/unknown\n计算 1 + 2\n刚才的结果？\n/new\n你好\n/exit\n不会发送\n");
    let mut output = Vec::new();
    chat(
        &mut model,
        &default_registry().unwrap(),
        &mut input,
        &mut output,
        "test",
    )
    .await
    .unwrap();
    assert!(model.replies.is_empty());
    assert_eq!(model.requests.len(), 4);
    assert_eq!(model.requests[0].messages.len(), 2);
    let with_result = &model.requests[1].messages;
    assert!(matches!(&with_result[3], Message::ToolResult(result)
        if result.tool_call_id == "c1" && result.result.content == ["3"]));
    let followup = &model.requests[2].messages;
    assert_eq!(followup.len(), 6);
    assert_eq!(&followup[..4], with_result);
    assert!(matches!(&followup[4], Message::Assistant(message)
        if message.content == [AssistantContent::Text { text: "答案是 3".into() }]));
    assert_eq!(
        followup[5],
        Message::User {
            content: "刚才的结果？".into()
        }
    );
    assert_eq!(
        model.requests[3].messages,
        vec![
            new_context().messages.remove(0),
            Message::User {
                content: "你好".into()
            }
        ]
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("未知命令：/unknown"));
    assert!(output.contains("tool> add"));
    assert!(output.contains("tool result> 3"));
    assert_eq!(output.matches("assistant> 答案是 3").count(), 1);
    assert!(output.contains("已新建会话。"));
    assert!(!output.contains("不会发送"));
}

#[tokio::test]
async fn failed_request_keeps_history_and_allows_followup_until_eof() {
    let mut model = Script {
        replies: VecDeque::from([
            Err(OperationError("DeepSeek HTTP 503".into())),
            answer("现在可以回答"),
        ]),
        requests: Vec::new(),
    };
    let mut output = Vec::new();
    chat(
        &mut model,
        &ToolRegistry::default(),
        &mut Cursor::new("你好\n再试一次"),
        &mut output,
        "test",
    )
    .await
    .unwrap();
    assert_eq!(model.requests.len(), 2);
    assert_eq!(model.requests[1].messages.len(), 4);
    assert!(
        matches!(&model.requests[1].messages[2], Message::Assistant(message)
        if message.stop_reason == StopReason::Error)
    );
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Error: DeepSeek HTTP 503"));
    assert!(output.contains("本次请求未完成"));
    assert!(output.contains("assistant> 现在可以回答"));
}

#[tokio::test]
async fn offline_mode_remembers_previous_input_and_new_clears_it() {
    let mut output = Vec::new();
    chat(
        &mut Mock,
        &ToolRegistry::default(),
        &mut Cursor::new("第一条\n第二条\n/new\n第三条\n/quit\n"),
        &mut output,
        "mock",
    )
    .await
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("第二条；上一条输入：第一条"));
    assert!(output.contains("第三条；上一条输入：无"));
}

struct Echo {
    name: &'static str,
}

impl AgentTool for Echo {
    fn declaration(&self) -> ToolDeclaration {
        ToolDeclaration {
            name: self.name.into(),
            description: "Echo text".into(),
            parameters: json!({"type": "string"}),
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<Value, OperationError> {
        if arguments.is_string() {
            Ok(arguments.clone())
        } else {
            Err(OperationError("Expected a string".into()))
        }
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        arguments: Value,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AgentToolResult, OperationError>> {
        Box::pin(async move {
            Ok(AgentToolResult {
                content: vec![arguments.as_str().unwrap().into()],
                details: arguments,
                is_error: false,
            })
        })
    }
}

#[tokio::test]
async fn newly_registered_tool_and_add_share_declaration_validation_and_execution() {
    let mut tools = default_registry().unwrap();
    tools.register(Echo { name: "echo" }).unwrap();
    // 注册失败不能覆盖原来的 add，也不能影响后续索引和执行目标。
    for name in ["", "add"] {
        assert_eq!(
            tools.register(Echo { name }),
            Err(LoopError::InvalidToolName(name.into()))
        );
    }
    let mut model = Script {
        replies: VecDeque::from([
            Ok(AssistantMessage {
                content: vec![
                    AssistantContent::ToolCall(ToolCall {
                        id: "echo-1".into(),
                        name: "echo".into(),
                        arguments: json!("hello"),
                        arguments_error: None,
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "add-1".into(),
                        name: "add".into(),
                        arguments: json!({"a": 2, "b": 5}),
                        arguments_error: None,
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "echo-invalid".into(),
                        name: "echo".into(),
                        arguments: json!(7),
                        arguments_error: None,
                    }),
                ],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            }),
            answer("done"),
        ]),
        requests: Vec::new(),
    };
    let mut output = Vec::new();
    chat(
        &mut model,
        &tools,
        &mut Cursor::new("use tools\n/exit\n"),
        &mut output,
        "test",
    )
    .await
    .unwrap();
    assert_eq!(
        model.requests[0].tools,
        vec![Add.declaration(), Echo { name: "echo" }.declaration()]
    );
    let results: Vec<_> = model.requests[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].tool_call_id, "echo-1");
    assert_eq!(results[0].result.content, ["hello"]);
    assert_eq!(results[1].tool_call_id, "add-1");
    assert_eq!(results[1].result.details, json!({"sum": 7}));
    assert!(results[2].result.is_error);
    assert_eq!(results[2].duration_ms, None);
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("tool result> hello"));
    assert!(output.contains("tool result> 7"));
    assert!(output.contains("tool error> Expected a string"));
}

#[tokio::test]
async fn unregistered_add_is_neither_declared_nor_executed() {
    let mut model = Script {
        replies: VecDeque::from([
            Ok(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "add".into(),
                    arguments: json!({"a": 1, "b": 2}),
                    arguments_error: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            }),
            answer("no tools"),
        ]),
        requests: Vec::new(),
    };
    let mut output = Vec::new();
    chat(
        &mut model,
        &ToolRegistry::default(),
        &mut Cursor::new("calculate\n/exit\n"),
        &mut output,
        "test",
    )
    .await
    .unwrap();
    assert!(
        model
            .requests
            .iter()
            .all(|request| request.tools.is_empty())
    );
    assert!(!SYSTEM.contains("add"));
    let Message::ToolResult(result) = &model.requests[1].messages[3] else {
        panic!("missing tool error")
    };
    assert!(result.result.is_error);
    assert_eq!(result.duration_ms, None);
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("tool error> Tool add not found")
    );
}
