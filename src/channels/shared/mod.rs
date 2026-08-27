//! channels::shared — 跨渠道共享的机制层（L5 内部）。
//!
//! 三渠道（QQ Bot / Telegram / WeChat）中结构完全同构的机制收编至此，
//! 渠道自身只保留协议特有的发送动作与节奏参数：
//!
//! - [`TypingKeepAlive`]：typing 保活任务生命周期（见 `typing.rs`）
//!
//! 本模块只依赖 channels 内部类型（message 等），不引入对 L3 tools 或
//! L6 的引用，分层上仍属 L5。

mod typing;

pub use typing::{TypingKeepAlive, TypingParams};
