-- ADR-028：供应商协议端点（D10/D7；M7-10）
--
-- `providers.endpoints` 保存协议 → Base URL 的 JSON 映射，例如：
--   {"anthropic":"https://api.deepseek.com/anthropic","openai":"https://api.deepseek.com"}
--
-- 说明：
-- - 既有行默认 `{}`；读取端按 `type` + `base_url` 派生默认端点（向后兼容）。
-- - 新行由 provider 表单/预设写入；不把密钥写入该列。
ALTER TABLE providers ADD COLUMN endpoints TEXT NOT NULL DEFAULT '{}';
