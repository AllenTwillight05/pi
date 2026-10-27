//! 单次演示与交互聊天共用的 DeepSeek 适配器。

use std::collections::HashMap;
use std::path::Path;

use pi_agent_loop::{
    AssistantContent, AssistantMessage, BoxFuture, CancellationToken, Message, Model, ModelRequest,
    OperationError, StopReason, ToolCall,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

#[cfg(test)]
#[path = "deepseek_tests.rs"]
mod tests;

pub(crate) struct DeepSeek {
    pub(crate) client: Client,
    pub(crate) endpoint: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) requests: usize,
    pub(crate) demo: bool,
}

impl Model for DeepSeek {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>> {
        Box::pin(async move {
            // 这是演示程序的请求预算，不改变 Core 的无隐含轮数上限契约。
            if self.demo && self.requests >= 4 {
                return Err(OperationError("Demo stopped after 4 model requests".into()));
            }
            self.requests += 1;
            if self.demo {
                println!("[DeepSeek] 请求 {}，模型 {}", self.requests, self.model);
            }
            let send = async {
                let response = self
                    .client
                    .post(&self.endpoint)
                    .bearer_auth(&self.api_key)
                    .json(&request_body(&self.model, request, self.demo))
                    .send()
                    .await
                    .map_err(|error| OperationError(error.to_string()))?;
                if !response.status().is_success() {
                    // 不回显服务端响应，避免认证错误内容包含 key。
                    return Err(OperationError(format!(
                        "DeepSeek HTTP {}",
                        response.status()
                    )));
                }
                let response = response
                    .json::<Completion>()
                    .await
                    .map_err(|error| OperationError(error.to_string()))?;
                parse_response(response)
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(OperationError("Operation aborted".into())),
                result = send => result,
            }
        })
    }
}

fn request_body(model: &str, request: ModelRequest, demo: bool) -> Value {
    let has_tool_result = request
        .messages
        .iter()
        .any(|message| matches!(message, Message::ToolResult(_)));
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| match message {
            Message::System { content } => json!({"role": "system", "content": content}),
            Message::User { content } => json!({"role": "user", "content": content}),
            Message::ToolResult(result) => json!({
                "role": "tool",
                "tool_call_id": result.tool_call_id,
                "content": result.result.content.join("\n"),
            }),
            Message::Assistant(message) => {
                let mut texts = Vec::new();
                let mut calls = Vec::new();
                for content in &message.content {
                    match content {
                        AssistantContent::Text { text } => texts.push(text.as_str()),
                        AssistantContent::ToolCall(call) => {
                            calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments.to_string(),
                                },
                            }));
                        }
                    }
                }
                let mut wire = json!({"role": "assistant", "content": texts.join("\n")});
                if !calls.is_empty() {
                    wire["tool_calls"] = json!(calls);
                }
                wire
            }
        })
        .collect();
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                },
            })
        })
        .collect();
    json!({
        "model": model,
        "messages": messages,
        "tools": tools,
        // 只有单次演示强制首次调用工具；聊天允许直接回答。
        "tool_choice": if demo && !has_tool_result { "required" } else { "auto" },
        "thinking": {"type": "disabled"},
        "stream": false,
        "max_tokens": 512,
    })
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    finish_reason: String,
    message: CompletionMessage,
}

#[derive(Deserialize)]
struct CompletionMessage {
    content: Option<String>,
    tool_calls: Option<Vec<FunctionCall>>,
}

#[derive(Deserialize)]
struct FunctionCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: Function,
}

#[derive(Deserialize)]
struct Function {
    name: String,
    arguments: String,
}

fn parse_response(response: Completion) -> Result<AssistantMessage, OperationError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| OperationError("DeepSeek returned no choices".into()))?;
    let stop_reason = match choice.finish_reason.as_str() {
        "stop" => StopReason::Stop,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::Length,
        reason => return Err(OperationError(format!("DeepSeek finish_reason: {reason}"))),
    };
    let mut content = Vec::new();
    if let Some(text) = choice.message.content {
        content.push(AssistantContent::Text { text });
    }
    for call in choice.message.tool_calls.unwrap_or_default() {
        if call.kind != "function" {
            return Err(OperationError(format!(
                "Unsupported tool type: {}",
                call.kind
            )));
        }
        let (arguments, arguments_error) = match serde_json::from_str(&call.function.arguments) {
            Ok(arguments) => (arguments, None),
            Err(error) => (Value::Null, Some(error.to_string())),
        };
        content.push(AssistantContent::ToolCall(ToolCall {
            id: call.id,
            name: call.function.name,
            arguments,
            arguments_error,
        }));
    }
    Ok(AssistantMessage {
        content,
        stop_reason,
        error_message: None,
    })
}

pub(crate) fn configuration(
    path: &Path,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<(String, String), OperationError> {
    // 只解析文件，不修改进程环境，避免异步程序中的全局环境写入。
    let local = match dotenvy::from_path_iter(path) {
        Ok(entries) => entries
            .collect::<Result<HashMap<_, _>, _>>()
            // dotenvy 的原始解析错误会包含整行，不能直接回显其中的 key。
            .map_err(|_| {
                OperationError(format!(
                    "Invalid configuration syntax in {}",
                    path.display()
                ))
            })?,
        Err(error) if error.not_found() => HashMap::new(),
        Err(_) => return Err(OperationError(format!("Cannot read {}", path.display()))),
    };
    let api_key = environment("DEEPSEEK_API_KEY")
        .or_else(|| local.get("DEEPSEEK_API_KEY").cloned())
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            OperationError(
                "Set DEEPSEEK_API_KEY in package/rust/.env.local or the environment".into(),
            )
        })?;
    let model = environment("DEEPSEEK_MODEL")
        .or_else(|| local.get("DEEPSEEK_MODEL").cloned())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| "deepseek-flash".into());
    Ok((api_key, model))
}
