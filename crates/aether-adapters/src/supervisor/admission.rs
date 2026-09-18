//! D5 适配器准入（评审修订 #1）：manifest 必须命中「官方内置白名单」，第三方一律拒绝。
//!
//! - 命中：允许加载（后续走正常启动流程）；
//! - 未命中：拒绝加载 → `disabled + status_reason=untrusted` + 审计记录；
//! - P0 不提供签名机制（无证书基础设施），白名单是唯一准入依据；P3 沙箱就绪后
//!   用户可在明确信任确认下放宽（D9），届时另行 ADR。
//!
//! 白名单取值与 `migrations/0001_init.sql` 的 `runtimes.id` 注释一致。

use std::collections::BTreeSet;

/// 官方内置运行时白名单（DDL 注释：codex | claude-code | deepseek-harness | pi | hermes | mock）。
pub const OFFICIAL_RUNTIME_IDS: [&str; 6] = [
    "mock",
    "codex",
    "claude-code",
    "deepseek-harness",
    "pi",
    "hermes",
];

/// 适配器 manifest（启动一个运行时所需的静态信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeManifest {
    /// 运行时 id（`runtimes.id`）。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 类型（如 `mock` / `acp`）。
    pub kind: String,
    /// 适配器版本（semver）。
    pub version: String,
    /// 线协议版本（D6 `hello.protocol` 期望值）。
    pub protocol: String,
    /// 是否官方内置（由内置清单声明；用户可写文件一律 `false`）。
    pub official: bool,
    /// 可执行文件路径。
    pub program: std::path::PathBuf,
    /// 启动参数（`--launch-token` 由监督器追加，不在 manifest 中）。
    pub args: Vec<String>,
    /// 是否参与预热（enabled）。
    pub enabled: bool,
}

impl RuntimeManifest {
    /// 便捷构造（测试/内置清单）。
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        program: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: "mock".to_owned(),
            version: "0.1.0".to_owned(),
            protocol: crate::protocol::PROTOCOL_VERSION.to_owned(),
            official: false,
            program: program.into(),
            args: Vec::new(),
            enabled: true,
        }
    }

    /// 声明为官方内置。
    pub fn official(mut self, official: bool) -> Self {
        self.official = official;
        self
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// 准入判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// 命中官方白名单。
    Admitted,
    /// 未命中：拒绝加载（`disabled + untrusted`）。
    Untrusted {
        /// 拒绝原因（审计与 UI 展示）。
        detail: String,
    },
}

impl AdmissionDecision {
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }
}

/// 准入策略（默认仅官方内置 id）。
#[derive(Debug, Clone)]
pub struct AdmissionPolicy {
    allowlist: BTreeSet<String>,
}

impl AdmissionPolicy {
    /// 官方内置白名单策略（D5 默认）。
    pub fn official() -> Self {
        Self {
            allowlist: OFFICIAL_RUNTIME_IDS
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
        }
    }

    /// 自定义白名单（测试/未来受信第三方扩展；扩展须走 ADR）。
    pub fn with_allowlist<I, S>(ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowlist: ids.into_iter().map(Into::into).collect(),
        }
    }

    pub fn is_allowlisted(&self, runtime_id: &str) -> bool {
        self.allowlist.contains(runtime_id)
    }

    pub fn allowlist(&self) -> impl Iterator<Item = &str> {
        self.allowlist.iter().map(String::as_str)
    }

    /// 判定 manifest：必须**同时**声明 `official` 且命中白名单。
    pub fn admit(&self, manifest: &RuntimeManifest) -> AdmissionDecision {
        let allowlisted = self.is_allowlisted(&manifest.id);
        match (manifest.official, allowlisted) {
            (true, true) => AdmissionDecision::Admitted,
            (true, false) => AdmissionDecision::Untrusted {
                detail: format!(
                    "manifest 自称官方但 id={} 不在官方白名单（评审 #1）",
                    manifest.id
                ),
            },
            (false, _) => AdmissionDecision::Untrusted {
                detail: format!(
                    "manifest id={} 未声明官方内置（第三方 manifest 一律拒绝，评审 #1）",
                    manifest.id
                ),
            },
        }
    }
}

impl Default for AdmissionPolicy {
    fn default() -> Self {
        Self::official()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, official: bool) -> RuntimeManifest {
        RuntimeManifest::new(id, id, "C:/bin/adapter.exe").official(official)
    }

    #[test]
    fn official_builtin_is_admitted() {
        let policy = AdmissionPolicy::official();
        let decision = policy.admit(&manifest("mock", true));
        assert_eq!(decision, AdmissionDecision::Admitted);
        assert!(decision.is_admitted());
    }

    #[test]
    fn third_party_manifest_requires_trusted() {
        let policy = AdmissionPolicy::official();
        let third_party = manifest("evil-adapter", false);
        let decision = policy.admit(&third_party);
        match decision {
            AdmissionDecision::Untrusted { detail } => {
                assert!(detail.contains("evil-adapter"));
                assert!(detail.contains("第三方"));
            }
            AdmissionDecision::Admitted => panic!("第三方 manifest 不得放行"),
        }
    }

    #[test]
    fn self_claimed_official_id_must_still_be_allowlisted() {
        let policy = AdmissionPolicy::official();
        let decision = policy.admit(&manifest("not-in-list", true));
        assert!(matches!(decision, AdmissionDecision::Untrusted { .. }));
    }

    #[test]
    fn allowlist_contains_ddl_documented_ids() {
        let policy = AdmissionPolicy::official();
        for id in OFFICIAL_RUNTIME_IDS {
            assert!(policy.is_allowlisted(id), "{id}");
        }
        assert!(!policy.is_allowlisted("third-party"));
    }

    #[test]
    fn custom_allowlist_is_respected() {
        let policy = AdmissionPolicy::with_allowlist(["mock", "codex"]);
        assert_eq!(
            policy.admit(&manifest("codex", true)),
            AdmissionDecision::Admitted
        );
        assert!(matches!(
            policy.admit(&manifest("claude-code", true)),
            AdmissionDecision::Untrusted { .. }
        ));
    }

    #[test]
    fn manifest_builder_defaults_are_safe() {
        let manifest = RuntimeManifest::new("mock", "Mock", "bin/mock");
        assert!(!manifest.official, "默认必须为非官方（默认拒绝）");
        assert!(manifest.enabled);
        assert!(manifest.protocol == "1.0");
    }
}
