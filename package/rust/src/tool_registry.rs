//! 工具注册表：集中保存声明和实现，供模型请求与执行层使用。

use crate::runtime::AgentTool;
use crate::types::{LoopError, ToolDeclaration};

struct RegisteredTool {
    declaration: ToolDeclaration,
    implementation: Box<dyn AgentTool>,
}

/// 按注册顺序保存工具；运行期间由共享借用保证工具表不会被修改。
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    /// 注册一个工具，立即读取并缓存声明；空名称或重复名称返回错误，不修改注册表。
    pub fn register(&mut self, tool: impl AgentTool + 'static) -> Result<(), LoopError> {
        let declaration = tool.declaration();
        if declaration.name.is_empty()
            || self
                .tools
                .iter()
                .any(|entry| entry.declaration.name == declaration.name)
        {
            return Err(LoopError::InvalidToolName(declaration.name));
        }
        self.tools.push(RegisteredTool {
            declaration,
            implementation: Box::new(tool),
        });
        Ok(())
    }

    /// 返回声明快照，顺序与执行层使用的工具索引一致。
    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.tools
            .iter()
            .map(|entry| entry.declaration.clone())
            .collect()
    }

    pub(crate) fn tool(&self, index: usize) -> &dyn AgentTool {
        self.tools[index].implementation.as_ref()
    }
}
