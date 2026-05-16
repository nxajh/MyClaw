use crate::providers::capability_tool::ToolResult;
use crate::providers::ToolCall;

use super::AgentLoop;

impl AgentLoop {
    pub(crate) fn build_tool_specs(&self) -> Vec<crate::providers::capability_chat::ToolSpec> {
        self.tool_executor.build_tool_specs()
    }

    pub(crate) async fn execute_tool(&mut self, call: &ToolCall) -> anyhow::Result<ToolResult> {
        // Copy autonomy so we can pass &mut self.session without a borrow conflict.
        let autonomy = self.session.session_override.autonomy;
        self.tool_executor.execute(call, &mut self.session, autonomy.as_ref()).await
    }
}
