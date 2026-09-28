/**
 * M1-08 CSP / 导航拦截 E2E 探针页脚本（本地同源，CSP script-src 'self' 允许）。
 *
 * 流程：
 *   1. 收集观测：内联脚本是否执行、远程脚本是否执行、wind→ 由页面在 1.5s 后回报；
 *   2. 通过 Tauri 内部 IPC 调用 e2e_probe_report（自定义命令不受 capabilities 限制）；
 *   3. IPC 成功后延迟 300ms 尝试一次外链导航（应被 on_navigation 拦截并记录）；
 *   4. IPC 失败时回退为带 payload 的外链导航，宿主侧解析 report-nav 兜底。
 */
(function () {
  "use strict";

  var violations = [];
  document.addEventListener("securitypolicyviolation", function (event) {
    violations.push({
      directive: event.violatedDirective,
      blockedURI: event.blockedURI,
    });
  });

  function collect() {
    return {
      inlineExecuted: window.__inlineExecuted === true,
      remoteExecuted: window.__remoteExecuted === true,
      globalTauri: typeof window.__TAURI__ !== "undefined",
      internalsAvailable: !!(window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke),
      violations: violations,
    };
  }

  function attemptExternalNavigation() {
    window.location.href = "https://aether-csp-probe.invalid/";
  }

  function fallbackNavigation(results) {
    window.location.href =
      "https://aether-csp-probe.invalid/report-nav?payload=" +
      encodeURIComponent(JSON.stringify(results));
  }

  function report(results) {
    var internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== "function") {
      fallbackNavigation(results);
      return;
    }
    internals.invoke("e2e_probe_report", { payload: results }).then(
      function () {
        setTimeout(attemptExternalNavigation, 300);
      },
      function () {
        fallbackNavigation(results);
      },
    );
  }

  // 等待预期四类指令的违规事件齐备再回报（或 5s 上限）：
  // `frame-src` 违规依赖 iframe 加载被阻断的事件时序，CI 冷启动/高负载下可能晚于
  // 固定 1.5s；固定等待会漏报（断言不变，仅消除报告时序竞态）。
  var REQUIRED_PREFIXES = ["script-src", "style-src", "img-src", "frame-src"];
  function directivesReady() {
    return REQUIRED_PREFIXES.every(function (prefix) {
      return violations.some(function (violation) {
        return (violation.directive || "").indexOf(prefix) === 0;
      });
    });
  }
  var deadline = Date.now() + 5000;
  var timer = setInterval(function () {
    if (directivesReady() || Date.now() > deadline) {
      clearInterval(timer);
      report(collect());
    }
  }, 100);
})();
