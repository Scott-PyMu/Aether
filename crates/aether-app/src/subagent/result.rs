//! M10-02/ADR-032：子 Agent 结构化结果解析、限制与 canonical 序列化。

use aether_core::{
    truncate_utf8, SubagentEvidenceRef, SubagentFinding, SubagentResult, SubagentSeverity,
    SubagentUsage, SubagentVerdict, SUBAGENT_EVIDENCE_MAX, SUBAGENT_FINDINGS_MAX,
    SUBAGENT_RESULT_MAX_BYTES, SUBAGENT_SUMMARY_MAX_BYTES,
};
use serde_json::{json, Value};

/// 解析后的结果 + 持久化元数据。
#[derive(Debug, Clone)]
pub struct ParsedResult {
    pub result: SubagentResult,
    pub canonical: String,
    pub total_bytes: usize,
    pub truncated: bool,
    pub summary_512: String,
    pub summary_truncated: bool,
}

/// 从子 Agent 最终助手消息解析结构；不可解析时合成保守 `info` 结果。
pub fn parse_child_result(text: &str) -> ParsedResult {
    let candidate = extract_json(text);
    let parsed = candidate
        .as_deref()
        .and_then(|raw| serde_json::from_str::<SubagentResult>(raw).ok())
        .or_else(|| candidate.as_deref().and_then(parse_legacy))
        .unwrap_or_else(|| SubagentResult::from_text(text));
    normalize(parsed)
}

fn extract_json(text: &str) -> Option<String> {
    // 1) 显式标签（推荐子 Agent 输出格式）。
    if let Some(start) = text.find("<aether:subagent-result>") {
        let start = start + "<aether:subagent-result>".len();
        if let Some(end) = text[start..].find("</aether:subagent-result>") {
            return Some(text[start..start + end].trim().to_owned());
        }
    }
    // 2) fenced code block（```json / ```）。
    let mut rest = text;
    while let Some(index) = rest.find("```") {
        let after = &rest[index + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let after = after.strip_prefix('\n').unwrap_or(after);
        if let Some(end) = after.find("```") {
            let block = after[..end].trim();
            if block.starts_with('{') {
                return Some(block.to_owned());
            }
            rest = &after[end + 3..];
            continue;
        }
        break;
    }
    // 3) 整段 JSON。
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_owned());
    }
    // 4) 第一个平衡的 JSON 对象（宽容：模型常在前后加说明）。
    balanced_object(text)
}

fn balanced_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + offset + 1].to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

/// 兼容 `{ "verdict": "...", "findings": [...] }` 的宽松形态（缺失字段补默认）。
fn parse_legacy(raw: &str) -> Option<SubagentResult> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let object = value.as_object()?;
    let verdict = object
        .get("verdict")
        .and_then(Value::as_str)
        .and_then(SubagentVerdict::from_code)
        .unwrap_or(SubagentVerdict::Info);
    let summary = object
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let findings = object
        .get("findings")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    let severity = match object
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("info")
                    {
                        "blocker" => SubagentSeverity::Blocker,
                        "major" => SubagentSeverity::Major,
                        "minor" => SubagentSeverity::Minor,
                        _ => SubagentSeverity::Info,
                    };
                    let evidence_refs = object
                        .get("evidence_refs")
                        .and_then(Value::as_array)
                        .map(|refs| {
                            refs.iter()
                                .take(SUBAGENT_EVIDENCE_MAX)
                                .filter_map(|reference| {
                                    let reference = reference.as_object()?;
                                    Some(SubagentEvidenceRef {
                                        path: reference
                                            .get("path")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_owned(),
                                        line_start: reference
                                            .get("line_start")
                                            .and_then(Value::as_i64),
                                        line_end: reference.get("line_end").and_then(Value::as_i64),
                                        artifact_id: reference
                                            .get("artifact_id")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(SubagentFinding {
                        severity,
                        title: object
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        detail: object
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        evidence_refs,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let confidence = object
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let usage = object
        .get("usage")
        .and_then(|value| serde_json::from_value::<SubagentUsage>(value.clone()).ok())
        .unwrap_or_default();
    Some(SubagentResult {
        verdict,
        summary,
        findings,
        confidence,
        usage,
    })
}

/// 施加 schema 上限并生成 canonical JSON（≤64KiB；超限截断并置 `truncated`）。
pub fn normalize(mut result: SubagentResult) -> ParsedResult {
    result.summary = truncate_utf8(&result.summary, 8_192).0;
    result.findings.truncate(SUBAGENT_FINDINGS_MAX);
    for finding in &mut result.findings {
        finding.title = truncate_utf8(&finding.title, 256).0;
        finding.detail = truncate_utf8(&finding.detail, 4_096).0;
        finding.evidence_refs.truncate(SUBAGENT_EVIDENCE_MAX);
        for evidence in &mut finding.evidence_refs {
            evidence.path = truncate_utf8(&evidence.path, 1_024).0;
        }
    }
    let summary_512_raw = result.summary.clone();
    let (summary_512, summary_truncated) =
        truncate_utf8(&summary_512_raw, SUBAGENT_SUMMARY_MAX_BYTES);

    let mut canonical = serde_json::to_string(&result).unwrap_or_else(|_| {
        "{\"verdict\":\"info\",\"summary\":\"\",\"findings\":[],\"confidence\":0.0,\"usage\":{\"tokens_in\":0,\"tokens_out\":0}}"
            .to_owned()
    });
    let mut truncated = false;
    if canonical.len() > SUBAGENT_RESULT_MAX_BYTES {
        truncated = true;
        // 先裁剪 findings 数，再裁剪 summary；仍超限则保留 verdict + 截断 summary。
        while canonical.len() > SUBAGENT_RESULT_MAX_BYTES && !result.findings.is_empty() {
            result.findings.pop();
            canonical = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned());
        }
        if canonical.len() > SUBAGENT_RESULT_MAX_BYTES {
            result.summary = truncate_utf8(&result.summary, 4_096).0;
            canonical = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_owned());
        }
        if canonical.len() > SUBAGENT_RESULT_MAX_BYTES {
            result.summary.clear();
            canonical = serde_json::to_string(&result).unwrap_or_else(|_| {
                json!({"verdict": result.verdict.as_str(), "summary": ""}).to_string()
            });
        }
    }
    let total_bytes = canonical.len();
    ParsedResult {
        result,
        canonical,
        total_bytes,
        truncated,
        summary_512,
        summary_truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_and_fenced_json() {
        let tagged = parse_child_result(
            "说明\n<aether:subagent-result>{\"verdict\":\"approve\",\"summary\":\"ok\"}</aether:subagent-result>",
        );
        assert_eq!(tagged.result.verdict, SubagentVerdict::Approve);
        let fenced = parse_child_result(
            "结果：\n```json\n{\"verdict\":\"needs_changes\",\"summary\":\"fix\"}\n```\n",
        );
        assert_eq!(fenced.result.verdict, SubagentVerdict::NeedsChanges);
    }

    #[test]
    fn falls_back_to_info_text() {
        let parsed = parse_child_result("没有结构化输出，只是普通文本");
        assert_eq!(parsed.result.verdict, SubagentVerdict::Info);
        assert!(parsed.result.findings.is_empty());
    }

    #[test]
    fn summary_is_utf8_byte_truncated() {
        let result = SubagentResult {
            verdict: SubagentVerdict::Info,
            summary: "子".repeat(300),
            findings: Vec::new(),
            confidence: 0.0,
            usage: SubagentUsage::default(),
        };
        let parsed = normalize(result);
        assert!(parsed.summary_512.len() <= SUBAGENT_SUMMARY_MAX_BYTES);
        assert!(parsed.summary_truncated);
    }
}
