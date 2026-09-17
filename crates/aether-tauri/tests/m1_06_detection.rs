//! M1-06 DoD1/DoD2/DoD4：A4 实现级检测样本集。
//!
//! - Windows 三类样本（DoD1）：环境变量前缀、父目录重解析点、注册表 `UserFolder`；
//!   另含网络盘粗筛与真实 Junction / 真实注册表沙箱键（Win runner 执行）；
//! - macOS 两类样本（DoD2）：iCloud 容器、File Provider 路径（注入 home 上下文，
//!   可在任意宿主断言；macOS 宿主走 `macos_native_samples_are_detected_and_local_is_released`
//!   原生上下文 + 真实样本目录，由 macos-14 CI 执行）；
//! - 本地目录对照样本 20 个（DoD4）：全部必须放行（防误杀）。
//!
//! 降级口径（M1-06 风险条款）：macOS 不绑定 `NSURLIsUbiquitousItemKey`，iCloud 采用
//! 路径前缀近似 + 用户手动确认；精度限制文案为 [`MAC_PRECISION_NOTE`]，UI（启动门
//! `startup-precision-note`）与测试均逐字断言。
//!
//! 样本报告为 stdout 机器可读行（`cargo test -- --nocapture` 捕获），由
//! `scripts/test/m1-06/verify-m1-06.mjs`（全平台）与
//! `scripts/test/m1-06/verify-m1-06-macos.mjs`（macOS 原生）断言并归档。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use aether_tauri::startup::detect::{
    detect_data_dir, DetectionContext, DetectionReport, NetworkDriveSource, PlatformKind,
    Precision, RegistrySource, ReparseSource, Verdict, MAC_PRECISION_NOTE,
};

const WIN_ENV_CHECK: &str = "win.one_drive_env_prefix";
const WIN_REPARSE_CHECK: &str = "win.parent_reparse_point";
const WIN_REGISTRY_CHECK: &str = "win.reg_user_folder";
const WIN_NETWORK_CHECK: &str = "win.network_drive_screen";
const MAC_ICLOUD_CHECK: &str = "mac.icloud_ubiquitous";
const MAC_PROVIDER_CHECK: &str = "mac.file_provider_path";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "aether-m1-06-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

fn context(platform: PlatformKind) -> DetectionContext {
    DetectionContext {
        platform,
        home: None,
        env: BTreeMap::new(),
        registry: RegistrySource::Unavailable,
        reparse: ReparseSource::Unavailable,
        network_drives: NetworkDriveSource::Unavailable,
    }
}

fn hit_ids(report: &DetectionReport) -> Vec<&'static str> {
    report
        .checks
        .iter()
        .filter(|check| check.hit)
        .map(|check| check.id)
        .collect()
}

fn assert_hit(report: &DetectionReport, id: &str, label: &str) {
    // 逐样本输出判定摘要（CI 失败时仅凭日志即可定位样本，不依赖断言展开）。
    println!(
        "[m1-06] sample-result {label}: expect={id} verdict={:?} hits={:?}",
        report.verdict,
        hit_ids(report)
    );
    if report.verdict != Verdict::Reject || !hit_ids(report).contains(&id) {
        for check in &report.checks {
            println!(
                "[m1-06] check id={} hit={} precision={:?} detail={}",
                check.id, check.hit, check.precision, check.detail
            );
        }
    }
    assert_eq!(
        report.verdict,
        Verdict::Reject,
        "样本 {label} 应命中拒绝：{:?}",
        report.reasons
    );
    assert!(
        hit_ids(report).contains(&id),
        "样本 {label} 应命中检查 {id}，实际：{:?}",
        hit_ids(report)
    );
    println!("[m1-06] sample {id}（{label}）: hit (PASS)");
}

#[test]
fn windows_samples_are_all_detected() {
    let root = temp_dir("win-samples");
    let sync = root.join("sync-root");
    std::fs::create_dir_all(&sync).expect("创建模拟 OneDrive 根");
    let mut total = 0usize;

    // ① OneDrive 环境变量前缀祖先。
    {
        let mut ctx = context(PlatformKind::Windows);
        ctx.env.insert("OneDrive".to_string(), sync.clone());
        let report = detect_data_dir(&sync.join("Aether"), &ctx);
        assert_hit(&report, WIN_ENV_CHECK, "环境变量前缀");
        total += 1;
    }

    // ② 父目录重解析点（注入样本；跨平台可执行）。
    {
        let link = root.join("junction-link");
        std::fs::create_dir_all(&link).expect("创建注入重解析点目录");
        let mut ctx = context(PlatformKind::Windows);
        ctx.reparse = ReparseSource::Paths(vec![link.clone()]);
        let report = detect_data_dir(&link.join("Aether"), &ctx);
        assert_hit(&report, WIN_REPARSE_CHECK, "重解析点（注入）");
        total += 1;
    }

    // ② 父目录重解析点（真实 Junction，Win runner 执行）。
    #[cfg(windows)]
    {
        let target = root.join("junction-real-target");
        let link = root.join("junction-real-link");
        std::fs::create_dir_all(&target).expect("创建 Junction 目标");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .expect("执行 mklink /J");
        assert!(status.success(), "Junction 创建失败（mklink /J）");
        {
            use std::os::windows::fs::{FileTypeExt, MetadataExt};
            let metadata = std::fs::symlink_metadata(&link).expect("Junction 元数据");
            println!(
                "[m1-06] junction-meta attrs=0x{:X} is_symlink={} is_symlink_dir={}",
                metadata.file_attributes(),
                metadata.file_type().is_symlink(),
                metadata.file_type().is_symlink_dir()
            );
        }
        let inside = link.join("Aether");
        std::fs::create_dir_all(&inside).expect("创建 Junction 内目录");
        let mut ctx = context(PlatformKind::Windows);
        ctx.reparse = ReparseSource::Native;
        let report = detect_data_dir(&inside, &ctx);
        assert_hit(&report, WIN_REPARSE_CHECK, "真实 Junction（原生元数据）");
        total += 1;
    }

    // ③ 注册表 UserFolder 比对（注入样本）。
    {
        let mut ctx = context(PlatformKind::Windows);
        ctx.registry = RegistrySource::Values(vec![(
            "Personal".to_string(),
            sync.join("RegistryPersonal"),
        )]);
        let report = detect_data_dir(&sync.join("RegistryPersonal").join("Aether"), &ctx);
        assert_hit(&report, WIN_REGISTRY_CHECK, "注册表 UserFolder（注入）");
        total += 1;
    }

    // ③ 注册表 UserFolder 比对（真实注册表沙箱键，Win runner 执行）。
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;

        let sandbox = format!("Software\\Aether\\M1-06-samples-{}", std::process::id());
        let registry_sync = root.join("registry-sync");
        std::fs::create_dir_all(&registry_sync).expect("创建注册表模拟同步目录");
        {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (accounts, _) = hkcu.create_subkey(&sandbox).expect("创建沙箱注册表键");
            let (account, _) = accounts.create_subkey("Personal").expect("创建账户子键");
            account
                .set_value("UserFolder", &registry_sync.to_string_lossy().to_string())
                .expect("写入 UserFolder");
        }
        let mut ctx = context(PlatformKind::Windows);
        ctx.registry = RegistrySource::NativeAt(sandbox.clone());
        let report = detect_data_dir(&registry_sync.join("Aether"), &ctx);
        assert_hit(&report, WIN_REGISTRY_CHECK, "真实注册表沙箱键");
        total += 1;
        let _ = RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(&sandbox);
    }

    // ④ 网络盘粗筛（UNC 全平台；映射盘符依赖 Windows 路径语义，仅 Win 宿主）。
    {
        let mut ctx = context(PlatformKind::Windows);
        ctx.network_drives = NetworkDriveSource::Letters(vec!["Z".to_string()]);
        // `Component::Prefix` 是 Windows 专属路径语义：非 Windows 宿主无法构造盘符样本。
        #[cfg(windows)]
        {
            let report = detect_data_dir(std::path::Path::new(r"Z:\Aether"), &ctx);
            assert_hit(&report, WIN_NETWORK_CHECK, "映射网络盘 Z:");
            total += 1;
            // 回归（M1-06 CI）：清单形态 `"Z:"` / `"z"` 也必须命中（规范化比较）。
            let mut colon_ctx = context(PlatformKind::Windows);
            colon_ctx.network_drives = NetworkDriveSource::Letters(vec!["z:".to_string()]);
            let report = detect_data_dir(std::path::Path::new(r"Z:\Aether"), &colon_ctx);
            assert_hit(
                &report,
                WIN_NETWORK_CHECK,
                "映射网络盘 Z:（冒号/大小写规范化）",
            );
            total += 1;
        }
        let report = detect_data_dir(std::path::Path::new(r"\\server\share\Aether"), &ctx);
        assert_hit(&report, WIN_NETWORK_CHECK, "UNC 路径");
        total += 1;
    }

    println!("[m1-06] windows summary: {total}/{total}");
}

#[test]
fn macos_samples_are_all_detected() {
    let root = temp_dir("mac-samples");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("创建模拟 home");
    let mut ctx = context(PlatformKind::MacOs);
    ctx.home = Some(home.clone());
    let mut total = 0usize;

    // ① iCloud 容器（风险条款降级：路径前缀近似 + UI 明示精度限制）。
    {
        let candidate = home
            .join("Library")
            .join("Mobile Documents")
            .join("com~apple~CloudDocs")
            .join("Aether");
        let report = detect_data_dir(&candidate, &ctx);
        assert_hit(&report, MAC_ICLOUD_CHECK, "iCloud Mobile Documents");
        let check = report
            .checks
            .iter()
            .find(|check| check.id == MAC_ICLOUD_CHECK)
            .expect("iCloud 检查存在");
        assert_eq!(
            check.precision,
            Precision::PathPrefix,
            "iCloud 检查必须以 PathPrefix 精度标注（降级实现）"
        );
        let note = report
            .note
            .as_deref()
            .expect("降级实现必须带精度说明（UI 明示）");
        assert_eq!(note, MAC_PRECISION_NOTE, "精度说明文案必须与 UI 常量一致");
        assert!(
            note.contains("检测精度受限"),
            "精度说明必须含「检测精度受限」"
        );
        assert!(
            note.contains("请确认目录不在 iCloud/CloudStorage 下"),
            "精度说明必须含手动确认指引"
        );
        println!("[m1-06] macos precision-note: {note}");
        total += 1;
    }

    // ② File Provider 挂载目录。
    {
        let candidate = home
            .join("Library")
            .join("CloudStorage")
            .join("Dropbox")
            .join("Aether");
        let report = detect_data_dir(&candidate, &ctx);
        assert_hit(&report, MAC_PROVIDER_CHECK, "CloudStorage/Dropbox");
        total += 1;
    }

    println!("[m1-06] macos summary: {total}/{total}");
}

/// DoD2（mac runner 原生执行）：真实样本目录 + `DetectionContext::native()`。
///
/// 样本目录由 `scripts/test/macos_sync_samples.sh` 构造（CI job 先跑脚本）；测试内
/// 兜底 `create_dir_all`，仅清理自建目录。断言：
/// ① `~/Library/CloudStorage/{Dropbox,GoogleDrive,OneDrive}` 全部命中；
/// ② `~/Library/Mobile Documents` 命中且报告含降级精度文案；
/// ③ `~/AetherTest/local` 放行（本地对照）。
#[cfg(target_os = "macos")]
#[test]
fn macos_native_samples_are_detected_and_local_is_released() {
    let home = dirs::home_dir().expect("macOS 用户主目录");
    let probe = format!("AetherM106-{}", std::process::id());
    let native = DetectionContext::native();
    assert_eq!(
        native.platform,
        PlatformKind::MacOs,
        "原生上下文平台必须是 macOS"
    );
    let mut total = 0usize;
    let mut cleanup: Vec<PathBuf> = Vec::new();

    // ① File Provider：~/Library/CloudStorage/{Dropbox,GoogleDrive,OneDrive}。
    for provider in ["Dropbox", "GoogleDrive", "OneDrive"] {
        let parent = home.join("Library").join("CloudStorage").join(provider);
        if ensure_sample_dir(&parent) {
            cleanup.push(parent.clone());
        }
        assert!(parent.is_dir(), "样本目录构造失败：{}", parent.display());
        let candidate = parent.join(&probe);
        let report = detect_data_dir(&candidate, &native);
        assert_hit(
            &report,
            MAC_PROVIDER_CHECK,
            &format!("~/Library/CloudStorage/{provider}"),
        );
        assert_eq!(
            report.note.as_deref(),
            Some(MAC_PRECISION_NOTE),
            "macOS 报告必须附带精度限制说明（UI 明示）"
        );
        total += 1;
    }

    // ② iCloud 容器：~/Library/Mobile Documents（路径前缀降级）。
    {
        let parent = home
            .join("Library")
            .join("Mobile Documents")
            .join("com~apple~CloudDocs");
        if ensure_sample_dir(&parent) {
            cleanup.push(parent.clone());
        }
        assert!(
            parent.is_dir(),
            "iCloud 样本目录构造失败：{}",
            parent.display()
        );
        let candidate = parent.join(&probe);
        let report = detect_data_dir(&candidate, &native);
        assert_hit(&report, MAC_ICLOUD_CHECK, "~/Library/Mobile Documents");
        let note = report.note.as_deref().expect("降级实现必须带精度说明");
        assert!(
            note.contains("检测精度受限"),
            "精度说明必须含「检测精度受限」"
        );
        assert!(
            note.contains("请确认目录不在 iCloud/CloudStorage 下"),
            "精度说明必须含手动确认指引"
        );
        println!("[m1-06] macos-native precision-note: {note}");
        total += 1;
    }

    // ③ 本地对照目录放行（~/AetherTest/local）。
    {
        let local = home.join("AetherTest").join("local");
        if ensure_sample_dir(&local) {
            cleanup.push(local.clone());
        }
        assert!(local.is_dir(), "本地对照目录构造失败：{}", local.display());
        let report = detect_data_dir(&local, &native);
        assert_eq!(
            report.verdict,
            Verdict::Allow,
            "本地目录必须放行：{:?}",
            report.reasons
        );
        assert_eq!(
            report.note.as_deref(),
            Some(MAC_PRECISION_NOTE),
            "macOS 上下文必须携带精度说明（UI 明示）"
        );
        println!("[m1-06] macos-native control ~/AetherTest/local: allow (PASS)");
        total += 1;
    }

    for path in cleanup {
        let _ = std::fs::remove_dir_all(&path);
    }
    println!("[m1-06] macos-native summary: {total}/{total}");
}

/// macOS 样本准备：存在即复用；否则创建并返回 `true`（仅清理自建目录）。
#[cfg(target_os = "macos")]
fn ensure_sample_dir(path: &std::path::Path) -> bool {
    if path.exists() {
        return false;
    }
    match std::fs::create_dir_all(path) {
        Ok(()) => true,
        Err(error) => panic!("创建样本目录 {} 失败：{error}", path.display()),
    }
}

#[test]
fn local_control_samples_are_all_released() {
    let root = temp_dir("controls");
    let sync = root.join("sync-root");
    std::fs::create_dir_all(&sync).expect("创建模拟同步根");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("创建模拟 home");

    let mut win = context(PlatformKind::Windows);
    win.env.insert("OneDrive".to_string(), sync.clone());
    win.env
        .insert("OneDriveConsumer".to_string(), root.join("sync-consumer"));
    win.registry = RegistrySource::Values(vec![(
        "Personal".to_string(),
        sync.join("RegistryPersonal"),
    )]);
    win.reparse = ReparseSource::Paths(vec![root.join("junction-link")]);
    win.network_drives = NetworkDriveSource::Letters(vec!["Z".to_string()]);

    let mut mac = context(PlatformKind::MacOs);
    mac.home = Some(home.clone());

    let mac_without_home = context(PlatformKind::MacOs);

    let samples: Vec<(&str, PathBuf, DetectionContext)> = vec![
        (
            "win.plain-local",
            root.join("local-plain").join("Aether"),
            win.clone(),
        ),
        (
            "win.segment-named-onedrive",
            root.join("OneDrive").join("Aether"),
            win.clone(),
        ),
        (
            "win.segment-onedrive-backup",
            root.join("OneDriveBackup").join("Aether"),
            win.clone(),
        ),
        (
            "win.segment-cloudstorage",
            root.join("CloudStorage").join("Aether"),
            win.clone(),
        ),
        (
            "win.sibling-of-env-root",
            root.join("sync-root-2").join("Aether"),
            win.clone(),
        ),
        (
            "win.other-root",
            root.join("elsewhere").join("Aether"),
            win.clone(),
        ),
        (
            "win.drive-c-not-mapped",
            root.join("drive-c").join("Aether"),
            win.clone(),
        ),
        (
            "win.drive-d-not-mapped",
            PathBuf::from(r"D:\AetherLocal"),
            win.clone(),
        ),
        (
            "win.fabricated-local",
            PathBuf::from(r"C:\AetherControlSample"),
            win.clone(),
        ),
        (
            "win.reparse-elsewhere",
            root.join("not-under-junction").join("Aether"),
            win.clone(),
        ),
        (
            "win.registry-elsewhere",
            root.join("registry-near-miss").join("Aether"),
            win.clone(),
        ),
        ("win.env-empty", root.join("empty-env").join("Aether"), {
            let mut ctx = context(PlatformKind::Windows);
            ctx.env.insert("OneDrive".to_string(), PathBuf::new());
            ctx
        }),
        (
            "mac.documents",
            home.join("Documents").join("Aether"),
            mac.clone(),
        ),
        (
            "mac.application-support",
            home.join("Library")
                .join("Application Support")
                .join("Aether"),
            mac.clone(),
        ),
        (
            "mac.cloudstorage-near-miss",
            home.join("Library")
                .join("CloudStorageX")
                .join("Dropbox")
                .join("Aether"),
            mac.clone(),
        ),
        (
            "mac.onedrive-name-only",
            home.join("OneDrive").join("Aether"),
            mac.clone(),
        ),
        (
            "mac.mobile-documents-near-miss",
            home.join("Library")
                .join("Mobile Documents Backup")
                .join("Aether"),
            mac.clone(),
        ),
        (
            "mac.desktop",
            home.join("Desktop").join("Aether"),
            mac.clone(),
        ),
        (
            "mac.home-unknown",
            root.join("iCloud Drive").join("Aether"),
            mac_without_home,
        ),
        (
            "mac.native-temp",
            root.join("mac-native-temp").join("Aether"),
            DetectionContext::native(),
        ),
    ];

    assert_eq!(samples.len(), 20, "本地对照样本必须为 20 个");
    let mut released = 0usize;
    for (label, candidate, ctx) in &samples {
        let report = detect_data_dir(candidate, ctx);
        assert_eq!(
            report.verdict,
            Verdict::Allow,
            "对照样本 {label} 被误杀：{:?}",
            report.reasons
        );
        println!("[m1-06] control {label}: allow (PASS)");
        released += 1;
    }
    println!("[m1-06] controls summary: {released}/20");
}
