//! 用于验证通用工具调用链路的整数加法工具。

use crate::{
    AgentTool, AgentToolResult, BoxFuture, CancellationToken, OperationError, ToolDeclaration,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AddArguments {
    a: i64,
    b: i64,
}

/// 严格校验整数参数，并检查加法溢出。
pub struct Add;

impl AgentTool for Add {
    fn declaration(&self) -> ToolDeclaration {
        ToolDeclaration {
            name: "add".into(),
            description: "计算两个整数的和".into(),
            parameters: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"],
                "additionalProperties": false,
            }),
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<Value, OperationError> {
        serde_json::from_value::<AddArguments>(arguments.clone())
            .map_err(|error| OperationError(error.to_string()))?;
        Ok(arguments.clone())
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        arguments: Value,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AgentToolResult, OperationError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(OperationError("Operation aborted".into()));
            }
            let args: AddArguments = serde_json::from_value(arguments)
                .map_err(|error| OperationError(error.to_string()))?;
            let sum = args
                .a
                .checked_add(args.b)
                .ok_or_else(|| OperationError("Integer overflow".into()))?;
            Ok(AgentToolResult {
                content: vec![sum.to_string()],
                details: json!({ "sum": sum }),
                is_error: false,
            })
        })
    }
}
