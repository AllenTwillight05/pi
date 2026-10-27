//! 行为测试使用固定模型脚本；有限组合枚举用于寻找反例，不构成形式化证明。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pi_agent_loop::{
    AgentContext, AgentEvent, AgentTool, AgentToolResult, AssistantContent, AssistantMessage,
    BoxFuture, CancellationToken, Core, Effect, EndReason, Input, LoopError, Message, Model,
    ModelRequest, OperationError, State, StopReason, ToolCall, ToolDeclaration, ToolRegistry,
    run_agent_loop, run_agent_loop_continue,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

fn user() -> Message {
    Message::User {
        content: "run".into(),
    }
}

fn call(value: Value) -> ToolCall {
    ToolCall {
        id: "same-id".into(),
        name: "echo".into(),
        arguments: value,
        arguments_error: None,
    }
}

fn assistant(calls: Vec<ToolCall>, stop_reason: StopReason) -> AssistantMessage {
    let mut content = vec![AssistantContent::Text {
        text: "response".into(),
    }];
    content.extend(calls.into_iter().map(AssistantContent::ToolCall));
    AssistantMessage {
        content,
        stop_reason,
        error_message: None,
    }
}

fn declaration() -> ToolDeclaration {
    ToolDeclaration {
        name: "echo".into(),
        description: "Echo an integer".into(),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "required": ["value"],
            "additionalProperties": false,
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    value: i64,
}

fn validate(arguments: &Value) -> Result<Value, OperationError> {
    serde_json::from_value::<Arguments>(arguments.clone())
        .map_err(|error| OperationError(error.to_string()))?;
    Ok(arguments.clone())
}

fn success(value: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![value.to_string()],
        details: value,
        is_error: false,
    }
}

enum ModelAction {
    Reply(AssistantMessage),
    Fail,
    WaitForCancellation(Arc<Notify>),
}

struct ScriptModel {
    actions: VecDeque<ModelAction>,
    requests: Vec<ModelRequest>,
}

impl ScriptModel {
    fn replies(messages: Vec<AssistantMessage>) -> Self {
        Self {
            actions: messages.into_iter().map(ModelAction::Reply).collect(),
            requests: Vec::new(),
        }
    }
}

impl Model for ScriptModel {
    fn complete<'a>(
        &'a mut self,
        request: ModelRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AssistantMessage, OperationError>> {
        self.requests.push(request);
        let action = self.actions.pop_front().expect("unexpected model request");
        Box::pin(async move {
            match action {
                ModelAction::Reply(message) => Ok(message),
                ModelAction::Fail => Err(OperationError("model offline".into())),
                ModelAction::WaitForCancellation(entered) => {
                    entered.notify_one();
                    cancellation.cancelled().await;
                    // 即使适配器返回正常响应，执行层的取消快照也应阻止其工具执行。
                    Ok(assistant(
                        vec![call(json!({"value": 9}))],
                        StopReason::ToolUse,
                    ))
                }
            }
        })
    }
}

enum ToolBehavior {
    Success,
    Fail,
    ReportError,
    WaitForCancellation(Arc<Notify>),
    CancelDuringValidation(CancellationToken),
}

struct ProbeTool {
    calls: Arc<Mutex<Vec<i64>>>,
    behavior: ToolBehavior,
}

impl AgentTool for ProbeTool {
    fn declaration(&self) -> ToolDeclaration {
        declaration()
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<Value, OperationError> {
        let validated = validate(arguments)?;
        if let ToolBehavior::CancelDuringValidation(token) = &self.behavior {
            token.cancel();
        }
        Ok(validated)
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        arguments: Value,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<AgentToolResult, OperationError>> {
        Box::pin(async move {
            let params: Arguments = serde_json::from_value(arguments)
                .map_err(|error| OperationError(error.to_string()))?;
            self.calls.lock().unwrap().push(params.value);
            match &self.behavior {
                ToolBehavior::Fail => Err(OperationError("tool failed".into())),
                ToolBehavior::ReportError => Ok(AgentToolResult {
                    content: vec!["reported failure".into()],
                    details: json!({ "preserved": true }),
                    is_error: true,
                }),
                ToolBehavior::WaitForCancellation(entered) => {
                    entered.notify_one();
                    cancellation.cancelled().await;
                    // 模拟副作用在取消前已发生、工具仍返回成功的情况。
                    Ok(success(json!(params.value)))
                }
                _ => Ok(success(json!(params.value))),
            }
        })
    }
}

fn tools(calls: &Arc<Mutex<Vec<i64>>>, behavior: ToolBehavior) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry
        .register(ProbeTool {
            calls: calls.clone(),
            behavior,
        })
        .unwrap();
    registry
}

fn assert_lifecycle(events: &[AgentEvent], reason: EndReason) {
    assert!(matches!(events.first(), Some(AgentEvent::AgentStart)));
    assert!(
        matches!(events.last(), Some(AgentEvent::AgentEnd { reason: actual, .. }) if *actual == reason)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnStart))
            .count(),
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count(),
    );
    for (index, event) in events.iter().enumerate() {
        if let AgentEvent::MessageStart(message) = event {
            assert_eq!(
                events.get(index + 1),
                Some(&AgentEvent::MessageEnd(message.clone()))
            );
        }
    }
}

#[tokio::test]
async fn sequential_tools_preserve_history_results_and_event_order() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(&executed, ToolBehavior::Success);
    let mut model = ScriptModel::replies(vec![
        assistant(
            vec![call(json!({"value": 1})), call(json!({"value": 2}))],
            StopReason::ToolUse,
        ),
        assistant(Vec::new(), StopReason::Stop),
    ]);
    let system = Message::System {
        content: "fixed system prompt".into(),
    };
    let mut events = Vec::new();
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext {
            messages: vec![system.clone()],
        },
        &mut model,
        &registry,
        &CancellationToken::new(),
        &mut |e| events.push(e.clone()),
    )
    .await
    .unwrap();

    assert_eq!(*executed.lock().unwrap(), vec![1, 2]);
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.new_messages.len(), 5);
    assert_eq!(outcome.context.messages.len(), 6);
    assert_eq!(model.requests.len(), 2);
    assert_eq!(model.requests[0].messages, vec![system.clone(), user()]);
    assert_eq!(model.requests[1].messages, outcome.context.messages[..5]);
    for request in &model.requests {
        assert_eq!(request.messages[0], system);
        assert_eq!(request.tools, vec![declaration()]);
    }
    let mut positions = Vec::new();
    for (index, message) in outcome.new_messages.iter().enumerate() {
        if let Message::ToolResult(result) = message {
            assert_eq!(result.tool_call_id, "same-id");
            assert!(result.duration_ms.is_some());
            positions.push((index, result.result.details.clone()));
        }
    }
    assert_eq!(positions, vec![(2, json!(1)), (3, json!(2))]);
    let tool_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionStart(_) => Some("start"),
            AgentEvent::ToolExecutionEnd(_) => Some("end"),
            _ => None,
        })
        .collect();
    assert_eq!(tool_events, ["start", "end", "start", "end"]);
    for (index, event) in events.iter().enumerate() {
        if let AgentEvent::ToolExecutionEnd(result) = event {
            assert_eq!(
                events[index + 1],
                AgentEvent::MessageStart(Message::ToolResult(result.clone()))
            );
        }
    }
    assert_lifecycle(&events, EndReason::Completed);
}

#[tokio::test]
async fn invalid_calls_become_error_results_and_never_execute() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(&executed, ToolBehavior::Success);
    let mut unknown = call(json!({"value": 1}));
    unknown.name = "missing".into();
    let mut malformed = call(json!({"value": 2}));
    malformed.arguments_error = Some("unterminated JSON object".into());
    let mut model = ScriptModel::replies(vec![
        assistant(
            vec![
                unknown,
                malformed,
                call(json!({})),
                call(json!({"value": "2"})),
                call(json!({"value": 3, "extra": true})),
                call(json!([1, 2])),
            ],
            StopReason::ToolUse,
        ),
        assistant(Vec::new(), StopReason::Stop),
    ]);
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &CancellationToken::new(),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(executed.lock().unwrap().is_empty());
    let results: Vec<_> = outcome
        .new_messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 6);
    assert!(
        results
            .iter()
            .all(|r| r.result.is_error && r.duration_ms.is_none())
    );
    assert!(results[0].result.content[0].contains("not found"));
    assert!(results[1].result.content[0].contains("unterminated JSON"));
    assert_eq!(model.requests.len(), 2);
}

#[tokio::test]
async fn length_truncated_calls_are_rejected_even_when_valid_and_can_be_reissued() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(&executed, ToolBehavior::Success);
    let mut model = ScriptModel::replies(vec![
        assistant(
            vec![call(json!({"value": 1})), call(json!({"value": 2}))],
            StopReason::Length,
        ),
        assistant(vec![call(json!({"value": 3}))], StopReason::ToolUse),
        assistant(Vec::new(), StopReason::Stop),
    ]);
    let mut events = Vec::new();
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &CancellationToken::new(),
        &mut |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(*executed.lock().unwrap(), [3]);
    let results: Vec<_> = outcome
        .new_messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    for result in &results[..2] {
        assert!(result.result.is_error);
        assert!(result.result.content[0].contains("output token limit"));
        assert_eq!(result.duration_ms, None);
    }
    assert!(!results[2].result.is_error);
    assert_lifecycle(&events, EndReason::Completed);
}

#[tokio::test]
async fn tool_failures_are_preserved_and_returned_to_the_model() {
    for behavior in [ToolBehavior::Fail, ToolBehavior::ReportError] {
        let executed = Arc::new(Mutex::new(Vec::new()));
        let registry = tools(&executed, behavior);
        let mut model = ScriptModel::replies(vec![
            assistant(vec![call(json!({"value": 1}))], StopReason::ToolUse),
            assistant(Vec::new(), StopReason::Stop),
        ]);
        let outcome = run_agent_loop(
            vec![user()],
            AgentContext::default(),
            &mut model,
            &registry,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(*executed.lock().unwrap(), [1]);
        let Message::ToolResult(result) = &model.requests[1].messages[2] else {
            panic!("missing result")
        };
        assert!(result.result.is_error);
        assert!(result.duration_ms.is_some());
        if result.result.content[0] == "reported failure" {
            assert_eq!(result.result.details, json!({"preserved": true}));
        } else {
            assert_eq!(result.result.content, ["tool failed"]);
        }
    }
}

#[tokio::test]
async fn model_errors_and_aborted_responses_are_hard_exits() {
    for reason in [StopReason::Error, StopReason::Aborted] {
        let executed = Arc::new(Mutex::new(Vec::new()));
        let registry = tools(&executed, ToolBehavior::Success);
        let mut model =
            ScriptModel::replies(vec![assistant(vec![call(json!({"value": 1}))], reason)]);
        let mut events = Vec::new();
        let outcome = run_agent_loop(
            vec![user()],
            AgentContext::default(),
            &mut model,
            &registry,
            &CancellationToken::new(),
            &mut |e| events.push(e.clone()),
        )
        .await
        .unwrap();
        let end = if reason == StopReason::Error {
            EndReason::Failed
        } else {
            EndReason::Aborted
        };
        assert_eq!(outcome.reason, end);
        assert!(executed.lock().unwrap().is_empty());
        assert_eq!(outcome.new_messages.len(), 2);
        assert_lifecycle(&events, end);
    }
    let mut model = ScriptModel {
        actions: VecDeque::from([ModelAction::Fail]),
        requests: Vec::new(),
    };
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &ToolRegistry::default(),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.reason, EndReason::Failed);
    let Some(Message::Assistant(message)) = outcome.new_messages.last() else {
        panic!("missing error")
    };
    assert_eq!(message.error_message.as_deref(), Some("model offline"));
}

#[tokio::test]
async fn continuation_returns_only_new_messages_and_checks_its_input() {
    let context = AgentContext {
        messages: vec![user()],
    };
    let mut model = ScriptModel::replies(vec![assistant(Vec::new(), StopReason::Stop)]);
    let mut events = Vec::new();
    let outcome = run_agent_loop_continue(
        context.clone(),
        &mut model,
        &ToolRegistry::default(),
        &CancellationToken::new(),
        &mut |e| events.push(e.clone()),
    )
    .await
    .unwrap();
    assert_eq!(outcome.new_messages.len(), 1);
    assert_eq!(outcome.context.messages.len(), 2);
    assert_eq!(model.requests[0].messages, context.messages);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::MessageEnd(Message::User { .. })))
    );
    assert_lifecycle(&events, EndReason::Completed);

    let mut unused = ScriptModel::replies(Vec::new());
    for (context, expected) in [
        (AgentContext::default(), LoopError::EmptyContinuation),
        (
            AgentContext {
                messages: vec![Message::Assistant(assistant(Vec::new(), StopReason::Stop))],
            },
            LoopError::AssistantContinuation,
        ),
    ] {
        assert_eq!(
            run_agent_loop_continue(
                context,
                &mut unused,
                &ToolRegistry::default(),
                &CancellationToken::new(),
                &mut |_| panic!("invalid input emitted event"),
            )
            .await,
            Err(expected)
        );
    }
    assert!(unused.requests.is_empty());
}

#[tokio::test]
async fn run_futures_are_send() {
    fn assert_send<T: Send>(future: T) -> T {
        future
    }

    let mut model = ScriptModel::replies(vec![
        assistant(Vec::new(), StopReason::Stop),
        assistant(Vec::new(), StopReason::Stop),
    ]);
    let token = CancellationToken::new();
    let mut events = Vec::new();
    let mut emit = |event: &AgentEvent| events.push(event.clone());
    assert_send(run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &ToolRegistry::default(),
        &token,
        &mut emit,
    ))
    .await
    .unwrap();
    assert_send(run_agent_loop_continue(
        AgentContext {
            messages: vec![user()],
        },
        &mut model,
        &ToolRegistry::default(),
        &token,
        &mut emit,
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn immediately_ready_operations_allow_concurrent_cancellation() {
    // 有限脚本保证缺少调度让步时测试会失败，而不是永久挂起。
    let mut replies = vec![assistant(vec![call(json!({"value": 1}))], StopReason::ToolUse); 32];
    replies.push(assistant(Vec::new(), StopReason::Stop));
    let mut model = ScriptModel::replies(replies);
    let token = CancellationToken::new();
    let entered = Notify::new();
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(&executed, ToolBehavior::Success);
    let mut events = Vec::new();
    let mut emit = |event: &AgentEvent| {
        if matches!(event, AgentEvent::ToolExecutionStart(_)) {
            entered.notify_one();
        }
        events.push(event.clone());
    };
    let run = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &token,
        &mut emit,
    );
    let cancel = async {
        entered.notified().await;
        token.cancel();
    };
    // 固定先轮询 run，避免取消分支先运行而掩盖饥饿问题。
    let (outcome, ()) = tokio::join!(biased; run, cancel);
    assert_eq!(outcome.unwrap().reason, EndReason::Aborted);
    assert!(model.requests.len() < 33);
    assert_lifecycle(&events, EndReason::Aborted);
}

#[tokio::test]
async fn cancellation_before_start_does_not_request_model_or_tools() {
    let token = CancellationToken::new();
    token.cancel();
    let mut model = ScriptModel::replies(Vec::new());
    let mut events = Vec::new();
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &ToolRegistry::default(),
        &token,
        &mut |e| events.push(e.clone()),
    )
    .await
    .unwrap();
    assert_eq!(outcome.reason, EndReason::Aborted);
    assert!(model.requests.is_empty());
    assert_lifecycle(&events, EndReason::Aborted);
}

#[tokio::test]
async fn cancellation_while_awaiting_model_prevents_its_tool_calls() {
    let token = CancellationToken::new();
    let entered = Arc::new(Notify::new());
    let mut model = ScriptModel {
        actions: VecDeque::from([ModelAction::WaitForCancellation(entered.clone())]),
        requests: Vec::new(),
    };
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(&executed, ToolBehavior::Success);
    let mut events = Vec::new();
    let cancel = async {
        entered.notified().await;
        token.cancel();
    };
    let mut emit = |e: &AgentEvent| events.push(e.clone());
    let run = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &token,
        &mut emit,
    );
    let (outcome, ()) = tokio::join!(run, cancel);
    assert_eq!(outcome.unwrap().reason, EndReason::Aborted);
    assert!(executed.lock().unwrap().is_empty());
    assert_lifecycle(&events, EndReason::Aborted);
}

#[tokio::test]
async fn cancellation_during_validation_prevents_execution() {
    let token = CancellationToken::new();
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(
        &executed,
        ToolBehavior::CancelDuringValidation(token.clone()),
    );
    let mut model = ScriptModel::replies(vec![assistant(
        vec![call(json!({"value": 1}))],
        StopReason::ToolUse,
    )]);
    let outcome = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &token,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert_eq!(outcome.reason, EndReason::Aborted);
    assert!(executed.lock().unwrap().is_empty());
    let Some(Message::ToolResult(result)) = outcome.new_messages.last() else {
        panic!("missing error result")
    };
    assert!(result.result.is_error);
    assert!(result.duration_ms.is_none());
}

#[tokio::test]
async fn cancellation_during_tool_preserves_its_result_and_stops_the_remaining_batch() {
    let token = CancellationToken::new();
    let entered = Arc::new(Notify::new());
    let executed = Arc::new(Mutex::new(Vec::new()));
    let registry = tools(
        &executed,
        ToolBehavior::WaitForCancellation(entered.clone()),
    );
    let mut model = ScriptModel::replies(vec![assistant(
        vec![call(json!({"value": 1})), call(json!({"value": 2}))],
        StopReason::ToolUse,
    )]);
    let mut events = Vec::new();
    let cancel = async {
        entered.notified().await;
        token.cancel();
    };
    let mut emit = |e: &AgentEvent| events.push(e.clone());
    let run = run_agent_loop(
        vec![user()],
        AgentContext::default(),
        &mut model,
        &registry,
        &token,
        &mut emit,
    );
    let (outcome, ()) = tokio::join!(run, cancel);
    let outcome = outcome.unwrap();
    assert_eq!(outcome.reason, EndReason::Aborted);
    assert_eq!(*executed.lock().unwrap(), [1]);
    assert_eq!(model.requests.len(), 1);
    let Some(Message::ToolResult(result)) = outcome.new_messages.last() else {
        panic!("missing completed result")
    };
    assert!(!result.result.is_error);
    assert_eq!(result.result.details, json!(1));
    assert_lifecycle(&events, EndReason::Aborted);
}

#[tokio::test]
async fn event_callback_can_cancel_before_a_pending_effect_starts() {
    for cancel_before_tool in [false, true] {
        let token = CancellationToken::new();
        let executed = Arc::new(Mutex::new(Vec::new()));
        let registry = tools(&executed, ToolBehavior::Success);
        let mut model = ScriptModel::replies(vec![assistant(
            vec![call(json!({"value": 1}))],
            StopReason::ToolUse,
        )]);
        let mut events = Vec::new();
        let outcome = run_agent_loop(
            vec![user()],
            AgentContext::default(),
            &mut model,
            &registry,
            &token,
            &mut |event| {
                if (cancel_before_tool && matches!(event, AgentEvent::ToolExecutionStart(_)))
                    || (!cancel_before_tool && matches!(event, AgentEvent::AgentStart))
                {
                    token.cancel();
                }
                events.push(event.clone());
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(model.requests.len(), usize::from(cancel_before_tool));
        assert!(executed.lock().unwrap().is_empty());
        assert_lifecycle(&events, EndReason::Aborted);
    }
}

#[test]
fn protocol_errors_do_not_change_state_and_terminal_states_never_schedule_again() {
    let mut core = Core::new(vec![user()], AgentContext::default(), vec![declaration()]).unwrap();
    assert_eq!(
        core.advance(Input::ArgumentsValidated {
            result: Ok(json!({})),
            cancelled: false
        }),
        Err(LoopError::UnexpectedInput)
    );
    assert_eq!(core.state(), State::Ready);
    assert!(core.context().messages.is_empty());
    core.advance(Input::Start { cancelled: false }).unwrap();
    core.advance(Input::ModelReturned {
        message: assistant(vec![call(json!({"value": 1}))], StopReason::ToolUse),
        cancelled: false,
    })
    .unwrap();
    let before = core.context().clone();
    assert_eq!(
        core.advance(Input::ToolReturned {
            result: Ok(success(json!(1))),
            duration_ms: Some(0),
            cancelled: false
        }),
        Err(LoopError::UnexpectedInput)
    );
    assert_eq!(core.state(), State::AwaitValidation);
    assert_eq!(core.context(), &before);
    core.advance(Input::ArgumentsValidated {
        result: Err(OperationError("invalid".into())),
        cancelled: false,
    })
    .unwrap();
    core.advance(Input::ModelReturned {
        message: assistant(Vec::new(), StopReason::Stop),
        cancelled: false,
    })
    .unwrap();
    assert_eq!(core.state(), State::Done(EndReason::Completed));
    for input in [
        Input::Start { cancelled: false },
        Input::ArgumentsValidated {
            result: Ok(json!({})),
            cancelled: false,
        },
        Input::ToolReturned {
            result: Ok(success(json!(1))),
            duration_ms: Some(0),
            cancelled: false,
        },
    ] {
        let step = core.advance(input).unwrap();
        assert!(step.events.is_empty());
        assert!(step.effect.is_none());
    }
    assert!(matches!(
        Core::new(
            Vec::new(),
            AgentContext::default(),
            vec![declaration(), declaration()]
        ),
        Err(LoopError::InvalidToolName(_))
    ));
    let mut unnamed = declaration();
    unnamed.name.clear();
    assert!(matches!(
        Core::new(Vec::new(), AgentContext::default(), vec![unnamed]),
        Err(LoopError::InvalidToolName(_))
    ));
    assert_eq!(
        Core::new(Vec::new(), AgentContext::default(), Vec::new())
            .unwrap()
            .into_outcome(),
        Err(LoopError::NotFinished)
    );
}

#[test]
fn bounded_combinations_preserve_validation_order_and_truncation_invariants() {
    // 穷举 0..=3 个调用，每个位置可为合法、校验失败、未知工具或解析失败；
    // 组合五种停止原因，共 425 条轨迹。所有调用故意复用同一个 ID。
    for count in 0..=3_u32 {
        for pattern in 0..4_usize.pow(count) {
            for reason in [
                StopReason::Stop,
                StopReason::ToolUse,
                StopReason::Length,
                StopReason::Error,
                StopReason::Aborted,
            ] {
                let mut calls = Vec::new();
                let mut expected_execution = Vec::new();
                for position in 0..count {
                    let kind = (pattern / 4_usize.pow(position)) % 4;
                    let mut call = call(json!({"value": position}));
                    match kind {
                        0 => expected_execution.push(json!({"value": position})),
                        1 => call.arguments = json!({"value": "wrong type"}),
                        2 => call.name = "missing".into(),
                        _ => call.arguments_error = Some("malformed".into()),
                    }
                    calls.push(call);
                }
                if matches!(
                    reason,
                    StopReason::Length | StopReason::Error | StopReason::Aborted
                ) {
                    expected_execution.clear();
                }
                let mut core =
                    Core::new(vec![user()], AgentContext::default(), vec![declaration()]).unwrap();
                let mut step = core.advance(Input::Start { cancelled: false }).unwrap();
                let mut requests = 0;
                let mut executed = Vec::new();
                let mut all_events = Vec::new();
                let mut validated_arguments = None;
                let mut remaining_steps = 20;
                loop {
                    remaining_steps -= 1;
                    assert!(remaining_steps > 0, "bounded trace did not terminate");
                    all_events.extend(step.events);
                    let Some(effect) = step.effect else { break };
                    let input = match effect {
                        Effect::RequestModel(request) => {
                            requests += 1;
                            if requests == 2 {
                                assert_eq!(
                                    request
                                        .messages
                                        .iter()
                                        .filter(|m| matches!(m, Message::ToolResult(_)))
                                        .count(),
                                    count as usize
                                );
                            }
                            assert!(requests <= 2);
                            Input::ModelReturned {
                                message: if requests == 1 {
                                    assistant(calls.clone(), reason)
                                } else {
                                    assistant(Vec::new(), StopReason::Stop)
                                },
                                cancelled: false,
                            }
                        }
                        Effect::ValidateTool { call, .. } => {
                            assert!(!matches!(
                                reason,
                                StopReason::Length | StopReason::Error | StopReason::Aborted
                            ));
                            let result = validate(&call.arguments);
                            validated_arguments = result.as_ref().ok().cloned();
                            Input::ArgumentsValidated {
                                result,
                                cancelled: false,
                            }
                        }
                        Effect::ExecuteTool { arguments, .. } => {
                            assert_eq!(validated_arguments.take(), Some(arguments.clone()));
                            executed.push(arguments.clone());
                            Input::ToolReturned {
                                result: Ok(success(arguments)),
                                duration_ms: Some(0),
                                cancelled: false,
                            }
                        }
                    };
                    step = core.advance(input).unwrap();
                }
                assert_eq!(executed, expected_execution);
                let outcome = core.into_outcome().unwrap();
                let results: Vec<_> = outcome
                    .new_messages
                    .iter()
                    .filter_map(|m| match m {
                        Message::ToolResult(r) => Some(r),
                        _ => None,
                    })
                    .collect();
                if matches!(reason, StopReason::Error | StopReason::Aborted) {
                    assert!(results.is_empty());
                } else {
                    assert_eq!(results.len(), count as usize);
                    for (result, call) in results.iter().zip(&calls) {
                        assert_eq!(result.tool_name, call.name);
                        assert_eq!(result.tool_call_id, call.id);
                    }
                }
                assert_lifecycle(&all_events, outcome.reason);
            }
        }
    }
}
