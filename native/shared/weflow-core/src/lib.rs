//! WeFlow 第三方可插拔组件共享核心库。
//!
//! 模块规划（随实现逐步补充）：
//! - `db`：SQLCipher 打开/句柄管理、微信 4.x db_storage 布局探查
//! - `schema`：message/session/contact/sns/emoticon/hardlink 各库表结构常量与查询
//! - `dat`：微信 .dat V1/V2/V3 媒体解密与重新加密、WXGF 容器识别
//! - `hardlink`：图片/视频 hardlink 文件解析
//! - `voice`：语音（silk）数据读取
//! - `json`：C ABI 出参 JSON 序列化的公共约定

pub mod dat;
pub mod db;
pub mod keystore;
pub mod messages;
pub mod schema;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
