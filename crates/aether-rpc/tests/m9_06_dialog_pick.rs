//! M9-06 DoD-4：server-initiated `dialog.pick` 协议 fixture（正常/超时/取消/迟到丢弃）。
//!
//! **仅协议预留**：M9 产品路径的 `ref_pick` / `startup_pick_target` 固定为壳本地命令
//! （ADR-029 §4.6），不经过本模块；59/4 路由计数不变。
//!
//! 测试豁免（AGENTS §2.2）：测试代码允许 unwrap/expect/panic。
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use aether_rpc::server_request::{
    ClientResponse, DialogPickParams, DialogPickResult, PendingServerRequests, PickKind,
    ResponseDisposition, ServerRequest, DIALOG_PICK_METHOD, DIALOG_PICK_TIMEOUT_MS,
    SERVER_REQUEST_TIMEOUT_MS,
};
use aether_rpc::Id;
use serde_json::{json, Value};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("test")
        .join("m9-06")
        .join("fixtures")
        .join("dialog-pick.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取 fixture {} 失败：{error}", path.display()));
    serde_json::from_str(&text).expect("解析 dialog-pick.json")
}

fn id_of(value: &Value) -> Id {
    value
        .get("id")
        .and_then(Id::from_value)
        .expect("fixture id 合法")
}

#[test]
fn dialog_pick_fixture_covers_normal_timeout_cancel_and_late_drop() {
    let fixture = fixture();
    assert_eq!(fixture["schema"], json!("aether.m9-06.dialog-pick.v1"));
    assert_eq!(fixture["method"], json!(DIALOG_PICK_METHOD));
    assert_eq!(
        fixture["timeouts_ms"]["default"],
        json!(SERVER_REQUEST_TIMEOUT_MS),
        "默认超时 300s"
    );
    assert_eq!(
        fixture["timeouts_ms"]["dialog"],
        json!(DIALOG_PICK_TIMEOUT_MS),
        "对话框超时 120s"
    );
    let now = fixture["now_ms"].as_u64().expect("now_ms");
    let case = |name: &str| fixture["cases"][name].clone();

    // ① 请求形状：params 严格解析；与服务端构造函数逐字段一致（id 由登记表分配）。
    let params = DialogPickParams::from_value(&case("request")["params"]).expect("params 合法");
    assert_eq!(params.kind, PickKind::File);
    let request = ServerRequest::dialog_pick(Id::String("srv-1".to_owned()), &params);
    assert_eq!(request.to_value(), case("request"), "请求帧与 fixture 漂移");

    // ② 正常响应：Accepted + path 解析（result 形状严格）。
    let mut pending = PendingServerRequests::new();
    pending
        .register(
            id_of(&request.to_value()),
            DIALOG_PICK_METHOD,
            now,
            DIALOG_PICK_TIMEOUT_MS,
        )
        .expect("登记");
    let disposition = pending
        .take_response(&case("normal_response"), now + 1)
        .expect("响应形状合法");
    match disposition {
        ResponseDisposition::Accepted {
            method, response, ..
        } => {
            assert_eq!(method, DIALOG_PICK_METHOD);
            let ClientResponse::Result(result) = response else {
                panic!("必须是 result");
            };
            assert_eq!(
                DialogPickResult::from_value(&result)
                    .expect("结果合法")
                    .path,
                Some("C:\\Users\\aether\\notes.md".to_owned())
            );
        }
        other => panic!("期望 Accepted，实际 {other:?}"),
    }
    assert!(pending.is_empty());

    // ③ 取消：client 以 `{path:null}` 返回（内部原因 cancelled，不是 aether_code）。
    let mut pending = PendingServerRequests::new();
    pending
        .register(
            Id::String("srv-2".to_owned()),
            DIALOG_PICK_METHOD,
            now,
            120_000,
        )
        .expect("登记");
    let disposition = pending
        .take_response(&case("cancel_response"), now + 1)
        .expect("响应形状合法");
    match disposition {
        ResponseDisposition::Accepted { response, .. } => {
            let ClientResponse::Result(result) = response else {
                panic!("必须是 result");
            };
            assert_eq!(
                DialogPickResult::from_value(&result)
                    .expect("结果合法")
                    .path,
                None
            );
        }
        other => panic!("期望 Accepted（取消），实际 {other:?}"),
    }

    // ④ 超时：120s 边界；迟到响应按 id 丢弃并计数。
    let timeout_params =
        DialogPickParams::from_value(&case("timeout_request")["params"]).expect("params 合法");
    assert_eq!(timeout_params.kind, PickKind::Directory);
    let timeout_request =
        ServerRequest::dialog_pick(Id::String("srv-3".to_owned()), &timeout_params);
    assert_eq!(timeout_request.to_value(), case("timeout_request"));
    let advance = case("timeout_advance_ms").as_u64().expect("advance");
    let mut pending = PendingServerRequests::new();
    pending
        .register(
            id_of(&timeout_request.to_value()),
            DIALOG_PICK_METHOD,
            now,
            DIALOG_PICK_TIMEOUT_MS,
        )
        .expect("登记");
    assert!(
        pending.expire(now + advance - 1).is_empty(),
        "119.999s 不超时"
    );
    let expired = pending.expire(now + advance);
    assert_eq!(expired.len(), 1, "120s 必须超时");
    assert_eq!(expired[0].reason.as_str(), "timeout");
    let disposition = pending
        .take_response(&case("late_response"), now + advance + 1)
        .expect("响应形状合法");
    assert!(matches!(disposition, ResponseDisposition::Late { .. }));
    assert_eq!(pending.late_dropped(), 1, "迟到响应必须丢弃并计数");

    // ⑤ 非法 params / 非法 result：拒绝且不登记。
    assert!(DialogPickParams::from_value(&case("invalid_params")).is_err());
    assert!(DialogPickResult::from_value(&case("invalid_result")).is_err());
    assert!(pending.is_empty());
}
