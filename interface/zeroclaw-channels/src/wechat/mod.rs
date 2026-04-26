//! WeChat channel adapter (stub)

use super::Channel;

/// WeChat channel stub.
pub struct WeChatAdapter;

impl Channel for WeChatAdapter {
    fn name(&self) -> &str {
        "wechat"
    }
}