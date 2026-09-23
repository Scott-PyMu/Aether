//! JSON 透传类型（M3-01/T14）。
//!
//! 用途：命令入参与 `aether://event` 事件 payload 在类型绑定中导出为 TS `unknown`。
//!
//! 为何不直接用 `serde_json::Value`：tauri-specta 2.0.0-rc.25 的 serde 相位格式化
//! 对 `Value` 的内联递归定义（`Value -> Vec<Value> / Map<String, Value>`）会栈溢出或
//! 报「递归内联类型无法展开」（`AetherEvent.payload` 路径）。该包装将 JSON 值以不透明
//! 引用映射为 TS `unknown`，线协议序列化（`#[serde(transparent)]`）与 `Value` 完全一致。
//!
//! 契约不变：命令入参不做 schema 级校验，一律由
//! [`crate::ipc::validate::parse_strict`] / [`crate::ipc::validate::parse_no_params`] 执行。

/// JSON 透传载荷（serde 透明；Specta 侧映射为 TS `unknown`）。
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct JsonPayload(pub serde_json::Value);

impl JsonPayload {
    /// 取出底层 JSON 值（校验入口使用）。
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }
}

impl specta::Type for JsonPayload {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <specta_typescript::Unknown as specta::Type>::definition(types)
    }
}
