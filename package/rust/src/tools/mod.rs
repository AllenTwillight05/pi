//! 工具实现与默认注册入口；新增默认工具时在此注册。

mod add;

pub use add::Add;

use crate::{LoopError, ToolRegistry};

/// 创建默认工具注册表；当前只有用于验证工具链路的 add。
pub fn default_registry() -> Result<ToolRegistry, LoopError> {
    let mut registry = ToolRegistry::default();
    registry.register(Add)?;
    Ok(registry)
}
