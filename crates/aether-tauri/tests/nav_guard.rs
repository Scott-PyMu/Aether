//! 导航拦截与系统浏览器转交矩阵（M1-08 DoD 2）。
//!
//! CSP 在真实 WebView 中的生效由 E2E 探针断言（scripts/test/m1-08）；
//! 本文件覆盖分类矩阵与转交决策。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use aether_tauri::nav::{
    apply_navigation_decision, browser_command, classify, ExternalOpener, NavigationAction,
};
use url::Url;

#[derive(Default)]
struct RecordingOpener {
    opened: Mutex<Vec<String>>,
}

impl RecordingOpener {
    fn opened(&self) -> Vec<String> {
        match self.opened.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl ExternalOpener for RecordingOpener {
    fn open(&self, url: &Url) -> std::io::Result<()> {
        match self.opened.lock() {
            Ok(mut guard) => guard.push(url.to_string()),
            Err(poisoned) => poisoned.into_inner().push(url.to_string()),
        }
        Ok(())
    }
}

fn url(raw: &str) -> Url {
    Url::parse(raw).expect("合法 URL")
}

fn dev_url() -> Url {
    url("http://localhost:5173")
}

#[test]
fn local_app_origins_are_allowed() {
    let dev = dev_url();
    let allowed = [
        "tauri://localhost/index.html",
        "http://tauri.localhost/index.html",
        "https://tauri.localhost/index.html#section",
        "http://localhost:5173/",
        "http://localhost:5173/assets/app.js",
        "about:blank",
    ];
    for raw in allowed {
        assert_eq!(
            classify(&url(raw), Some(&dev)),
            NavigationAction::Allow,
            "{raw} 应放行"
        );
    }
}

#[test]
fn dev_origin_is_rejected_in_release_configuration() {
    // devUrl 仅在 dev 构建传入；release 下 localhost 与 tauri.localhost 不同源。
    assert_eq!(
        classify(&url("http://localhost:5173/"), None),
        NavigationAction::OpenExternal
    );
}

#[test]
fn external_http_urls_are_handed_to_system_browser() {
    for raw in [
        "https://example.com/",
        "http://127.0.0.1:8931/pixel.png",
        "https://evil.example/path?q=1",
    ] {
        let action = classify(&url(raw), None);
        assert_eq!(action, NavigationAction::OpenExternal, "{raw}");
        let opener = RecordingOpener::default();
        let allowed = apply_navigation_decision(action, &url(raw), &opener);
        assert!(!allowed, "外链不得在 WebView 内导航");
        assert_eq!(opener.opened(), vec![raw.to_string()], "必须转交系统浏览器");
    }
}

#[test]
fn dangerous_schemes_are_blocked_without_handoff() {
    for raw in [
        "file:///C:/Windows/System32/drivers/etc/hosts",
        "data:text/html,<script>alert(1)</script>",
        "javascript:alert(1)",
        "aether-probe://report",
    ] {
        let action = classify(&url(raw), None);
        assert_eq!(action, NavigationAction::Block, "{raw}");
        let opener = RecordingOpener::default();
        let allowed = apply_navigation_decision(action, &url(raw), &opener);
        assert!(!allowed, "危险 scheme 必须阻断");
        assert!(opener.opened().is_empty(), "危险 scheme 不得转交浏览器");
    }
}

#[test]
fn allowed_navigation_does_not_open_browser() {
    let opener = RecordingOpener::default();
    let allowed = apply_navigation_decision(
        NavigationAction::Allow,
        &url("http://tauri.localhost/index.html"),
        &opener,
    );
    assert!(allowed);
    assert!(opener.opened().is_empty());
}

#[test]
fn browser_command_passes_url_as_argument_without_shell() {
    let target = url("https://example.com/a?b=1&c=2");
    let (program, args) = browser_command(&target);
    assert!(!program.is_empty());
    let joined = args.join(" ");
    assert!(
        joined.contains(target.as_str()),
        "URL 必须原样作为参数：{joined}"
    );
}
