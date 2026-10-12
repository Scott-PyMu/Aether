//! 备份产物加密（M5-03；设计 D10/D13、A3；ADR-020 §2.1-3/4、附录 A-2）。
//!
//! 产物格式 v1（ADR-020 附录 A-2 冻结）：
//!
//! ```text
//! magic   : "AEBK1" (5B)
//! version : u8 = 1
//! flags   : u8  bit0 = passphrase-mode, bit1 = keychain-mode（恰置其一）
//! salt    : 16B（仅 passphrase-mode）
//! nonce   : 24B（随机）
//! source  : u16 LE 长度 + source_backup_id（≤64B UTF-8）+ i64 LE 生成时间
//! body    : XChaCha20-Poly1305 密文（末 16B 为 AEAD tag；AAD = 完整头字节）
//! ```
//!
//! 单一 AEAD tag 同时认证头与密文（头作为 AAD 输入），**不另设独立 header_auth**；
//! 任何头字节篡改都会在解密时失败并明确拒绝（[`SecretError::Decrypt`]）。
//!
//! 实现说明（常量级实现细节，记入 M5-03 证据）：
//! - 加密/解密为**流式**（1 MiB 分块），内存占用与文件大小无关；密文与
//!   `chacha20poly1305::XChaCha20Poly1305` 单次加密逐字节一致（单测对拍）；
//! - 默认密钥 = OS Keychain 主密钥（`keychain://aether/backup/master`，32B 随机，
//!   首次使用生成；A3 降级时同一引用落在 `secrets.enc`）；
//! - 口令模式为显式可选：Argon2id（m=64MiB, t=3, p=1，与 A3 同参）+ 16B 随机盐。
//!
//! 安全边界（AGENTS §2.9）：密钥/口令只经 [`SecretValue`]/[`Zeroizing`] 传递，
//! 错误信息不含任何密钥或口令值；盐/nonce 非机密，可出现在产物头。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::{Key, XChaCha20, XNonce};
use poly1305::universal_hash::{KeyInit as _, UniversalHash};
use poly1305::Poly1305;
use zeroize::{Zeroize, Zeroizing};

use crate::error::SecretError;
use crate::reference::KeychainRef;
use crate::store::{random_bytes, SecretStore, SecretValue};

/// 产物 magic（ADR-020 附录 A-2）。
pub const ARTIFACT_MAGIC: [u8; 5] = *b"AEBK1";
/// 产物格式版本（v1）。
pub const ARTIFACT_VERSION: u8 = 1;
/// flags：口令模式（含 salt）。
pub const FLAG_PASSPHRASE_MODE: u8 = 0x01;
/// flags：Keychain 主密钥模式。
pub const FLAG_KEYCHAIN_MODE: u8 = 0x02;
/// 口令盐长度（字节）。
pub const SALT_LEN: usize = 16;
/// nonce 长度（字节）。
pub const NONCE_LEN: usize = 24;
/// 密钥长度（字节）。
pub const KEY_LEN: usize = 32;
/// AEAD tag 长度（字节；单一 tag，头为 AAD）。
pub const TAG_LEN: usize = 16;
/// 主密钥引用 service 段。
pub const BACKUP_MASTER_KEY_SERVICE: &str = "backup";
/// 主密钥引用 key 段。
pub const BACKUP_MASTER_KEY_NAME: &str = "master";
/// source_backup_id 字段最大长度（ULID 为 26；留裕量）。
pub const MAX_SOURCE_ID_LEN: usize = 64;
/// 流式分块大小（1 MiB；16 的整数倍，保证 Poly1305 分块更新与单次一致）。
const CHUNK_SIZE: usize = 1024 * 1024;
/// ChaCha20 块大小（AEAD 构造：Poly1305 key 取块 0，正文从块 1 开始）。
const BLOCK_SIZE: usize = 64;
/// 单 key/nonce 可加密的密文上限（`u32::MAX` 个块，与 chacha20poly1305 一致）。
const MAX_CIPHERTEXT_LEN: u64 = (u32::MAX as u64) * (BLOCK_SIZE as u64);

/// 产物密钥模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupArtifactMode {
    /// Keychain 主密钥模式（默认）。
    Keychain,
    /// 口令模式（Argon2id 派生；产物头含随机盐）。
    Passphrase,
}

impl BackupArtifactMode {
    /// 模式对应 flags 位。
    #[must_use]
    pub const fn flag(self) -> u8 {
        match self {
            Self::Keychain => FLAG_KEYCHAIN_MODE,
            Self::Passphrase => FLAG_PASSPHRASE_MODE,
        }
    }
}

/// 加密产物信息（恢复校验/证据；salt 非机密，可回传）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupArtifactInfo {
    /// 产物密钥模式。
    pub mode: BackupArtifactMode,
    /// 来源备份 id（创建时台账 id；恢复校验）。
    pub source_backup_id: String,
    /// 产物生成时间（毫秒）。
    pub generated_at: i64,
    /// 口令模式盐（非机密；解密派生用）。
    pub salt: Option<[u8; SALT_LEN]>,
}

/// 产物加密密钥（由调用方解析：主密钥或口令派生）。
pub enum BackupArtifactKey {
    /// Keychain 主密钥模式。
    Keychain(Zeroizing<[u8; KEY_LEN]>),
    /// 口令模式（含派生用盐，写入产物头）。
    Passphrase {
        /// Argon2id 派生密钥。
        key: Zeroizing<[u8; KEY_LEN]>,
        /// 随机盐（16B）。
        salt: [u8; SALT_LEN],
    },
}

impl BackupArtifactKey {
    /// 模式。
    #[must_use]
    pub fn mode(&self) -> BackupArtifactMode {
        match self {
            Self::Keychain(_) => BackupArtifactMode::Keychain,
            Self::Passphrase { .. } => BackupArtifactMode::Passphrase,
        }
    }

    fn key(&self) -> &[u8; KEY_LEN] {
        match self {
            Self::Keychain(key) | Self::Passphrase { key, .. } => key,
        }
    }
}

/// 备份加密 KDF 参数（口令模式；生产固定 A3 冻结值，测试/演练可注入弱参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupCryptoParams {
    /// Argon2id 内存成本（KiB）。
    pub m_cost_kib: u32,
    /// Argon2id 迭代次数。
    pub t_cost: u32,
    /// Argon2id 并行度。
    pub p_cost: u32,
}

impl BackupCryptoParams {
    /// ADR-020 冻结参数（m=64MiB, t=3, p=1；与 A3 同参）。
    #[must_use]
    pub const fn a3() -> Self {
        Self {
            m_cost_kib: crate::encrypted_file::A3_M_COST_KIB,
            t_cost: crate::encrypted_file::A3_T_COST,
            p_cost: crate::encrypted_file::A3_P_COST,
        }
    }

    /// 测试/演练专用弱参数——禁止用于生产（生产路径固定 [`Self::a3`]）。
    #[must_use]
    pub const fn test_only_fast() -> Self {
        Self {
            m_cost_kib: 8 * 1024,
            t_cost: 1,
            p_cost: 1,
        }
    }
}

impl Default for BackupCryptoParams {
    fn default() -> Self {
        Self::a3()
    }
}

/// 主密钥引用（`keychain://aether/backup/master`，ADR-020 §2.1-3）。
pub fn backup_master_key_ref() -> Result<KeychainRef, SecretError> {
    KeychainRef::new(BACKUP_MASTER_KEY_SERVICE, BACKUP_MASTER_KEY_NAME)
}

/// 主密钥引用 URI（响应/证据呈现；引用非密钥）。
pub fn backup_master_key_uri() -> String {
    format!("keychain://aether/{BACKUP_MASTER_KEY_SERVICE}/{BACKUP_MASTER_KEY_NAME}")
}

/// 读取主密钥；不存在则生成 32B 随机主密钥并写入（首次备份）。
///
/// 存储值为 hex（64 字符）；非法存储值明确报错（不静默重建，避免旧产物不可恢复）。
pub fn load_or_create_master_key(
    store: &dyn SecretStore,
) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let reference = backup_master_key_ref()?;
    match store.get(&reference) {
        Ok(value) => decode_master_key(value.expose()),
        Err(error) if error.is_not_found() => {
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            random_bytes(key.as_mut())?;
            store.set(&reference, &SecretValue::new(encode_hex(key.as_ref())))?;
            Ok(key)
        }
        Err(error) => Err(error),
    }
}

/// 主密钥当前可用性探测（诊断/状态投影；不创建新密钥）。
///
/// `NotFound` 视为可用（首次备份时可生成）；存储不可用/值损坏 → `false`。
#[must_use]
pub fn master_key_available(store: &dyn SecretStore) -> bool {
    let Ok(reference) = backup_master_key_ref() else {
        return false;
    };
    match store.get(&reference) {
        Ok(value) => decode_master_key(value.expose()).is_ok(),
        Err(error) if error.is_not_found() => true,
        Err(_) => false,
    }
}

/// 口令 → 派生密钥 + 新随机盐（加密路径；Argon2id）。
pub fn derive_passphrase_key(
    params: BackupCryptoParams,
    passphrase: &SecretValue,
) -> Result<(Zeroizing<[u8; KEY_LEN]>, [u8; SALT_LEN]), SecretError> {
    let mut salt = [0u8; SALT_LEN];
    random_bytes(&mut salt)?;
    let key = derive_passphrase_key_with_salt(params, &salt, passphrase)?;
    Ok((key, salt))
}

/// 口令 + 既有盐 → 派生密钥（解密路径）。
pub fn derive_passphrase_key_with_salt(
    params: BackupCryptoParams,
    salt: &[u8; SALT_LEN],
    passphrase: &SecretValue,
) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let argon_params = Params::new(
        params.m_cost_kib,
        params.t_cost,
        params.p_cost,
        Some(KEY_LEN),
    )
    .map_err(|error| SecretError::InvalidFormat(format!("Argon2 参数非法：{error}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase.expose().as_bytes(), salt, key.as_mut())
        .map_err(|error| SecretError::InvalidFormat(format!("Argon2 派生失败：{error}")))?;
    Ok(key)
}

/// 文件是否为加密产物（读 magic；不存在/不可读/非产物 → `false`）。
#[must_use]
pub fn is_encrypted_backup_artifact(path: &Path) -> bool {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut magic = [0u8; ARTIFACT_MAGIC.len()];
    file.read_exact(&mut magic).is_ok() && magic == ARTIFACT_MAGIC
}

/// 解析产物头（不执行解密；flags 非法/格式非法 → [`SecretError::InvalidFormat`]）。
pub fn inspect_backup_artifact(path: &Path) -> Result<BackupArtifactInfo, SecretError> {
    let mut file = File::open(path)?;
    let (header, _len) = read_header(&mut file)?;
    Ok(header.info())
}

/// 加密备份文件（流式；`src` 为已校验的明文 DB，`dest` 必须不存在）。
///
/// 返回产物总字节数。头字节即 AAD；密文与 tag 顺序写出；失败不留下 `dest`
/// （调用方负责临时文件与原子 rename）。
pub fn encrypt_backup_file(
    src: &Path,
    dest: &Path,
    source_backup_id: &str,
    generated_at: i64,
    key: &BackupArtifactKey,
) -> Result<u64, SecretError> {
    if source_backup_id.len() > MAX_SOURCE_ID_LEN {
        return Err(SecretError::InvalidFormat(format!(
            "source_backup_id 超过 {MAX_SOURCE_ID_LEN} 字节上限"
        )));
    }
    let total = fs::metadata(src)?.len();
    if total >= MAX_CIPHERTEXT_LEN {
        return Err(SecretError::InvalidFormat(
            "源文件超出单 key/nonce 可加密上限".to_owned(),
        ));
    }
    if dest.exists() {
        return Err(SecretError::InvalidFormat(format!(
            "加密目标已存在（拒绝覆盖）：{}",
            dest.display()
        )));
    }

    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce)?;
    let header = ArtifactHeader {
        mode: key.mode(),
        salt: match key {
            BackupArtifactKey::Passphrase { salt, .. } => Some(*salt),
            BackupArtifactKey::Keychain(_) => None,
        },
        nonce,
        source_backup_id: source_backup_id.to_owned(),
        generated_at,
    };
    let aad = header.to_bytes();

    let outcome = seal_file(src, dest, &aad, key.key(), &nonce, total);
    if outcome.is_err() {
        let _ = fs::remove_file(dest);
    }
    outcome
}

/// 解密备份产物到 `dest`（流式；`dest` 必须不存在）。
///
/// tag 校验失败（口令/密钥错误、头/密文篡改）→ [`SecretError::Decrypt`]；
/// 失败不留下 `dest`。
pub fn decrypt_backup_file(
    src: &Path,
    dest: &Path,
    key: &[u8; KEY_LEN],
) -> Result<BackupArtifactInfo, SecretError> {
    if dest.exists() {
        return Err(SecretError::InvalidFormat(format!(
            "解密目标已存在（拒绝覆盖）：{}",
            dest.display()
        )));
    }
    let outcome = open_file(src, dest, key);
    match outcome {
        Ok(info) => Ok(info),
        Err(error) => {
            let _ = fs::remove_file(dest);
            Err(error)
        }
    }
}

// ===== 内部实现 =====

/// 产物头（序列化确定；头字节整体作为 AAD）。
struct ArtifactHeader {
    mode: BackupArtifactMode,
    salt: Option<[u8; SALT_LEN]>,
    nonce: [u8; NONCE_LEN],
    source_backup_id: String,
    generated_at: i64,
}

impl ArtifactHeader {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            ARTIFACT_MAGIC.len() + 2 + SALT_LEN + NONCE_LEN + 2 + self.source_backup_id.len() + 8,
        );
        out.extend_from_slice(&ARTIFACT_MAGIC);
        out.push(ARTIFACT_VERSION);
        out.push(self.mode.flag());
        if let Some(salt) = &self.salt {
            out.extend_from_slice(salt);
        }
        out.extend_from_slice(&self.nonce);
        let id_len = u16::try_from(self.source_backup_id.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&id_len.to_le_bytes());
        out.extend_from_slice(self.source_backup_id.as_bytes());
        out.extend_from_slice(&self.generated_at.to_le_bytes());
        out
    }

    fn info(&self) -> BackupArtifactInfo {
        BackupArtifactInfo {
            mode: self.mode,
            source_backup_id: self.source_backup_id.clone(),
            generated_at: self.generated_at,
            salt: self.salt,
        }
    }
}

fn read_exact_or_format(
    reader: &mut impl Read,
    buffer: &mut [u8],
    what: &str,
) -> Result<(), SecretError> {
    reader
        .read_exact(buffer)
        .map_err(|_| SecretError::InvalidFormat(format!("产物头不完整（{what}）")))
}

/// 读取并解析产物头；返回（头，头字节长度）。`aad = 文件前 `len` 字节`。
fn read_header(reader: &mut impl Read) -> Result<(ArtifactHeader, usize), SecretError> {
    let mut magic = [0u8; ARTIFACT_MAGIC.len()];
    read_exact_or_format(reader, &mut magic, "magic")?;
    if magic != ARTIFACT_MAGIC {
        return Err(SecretError::InvalidFormat("产物 magic 非法".to_owned()));
    }
    let mut version_flags = [0u8; 2];
    read_exact_or_format(reader, &mut version_flags, "version/flags")?;
    if version_flags[0] != ARTIFACT_VERSION {
        return Err(SecretError::InvalidFormat(format!(
            "不支持的产物版本：{}",
            version_flags[0]
        )));
    }
    let flags = version_flags[1];
    let mode = match flags {
        FLAG_PASSPHRASE_MODE => BackupArtifactMode::Passphrase,
        FLAG_KEYCHAIN_MODE => BackupArtifactMode::Keychain,
        other => {
            return Err(SecretError::InvalidFormat(format!(
                "产物 flags 非法（须恰置其一）：0x{other:02x}"
            )))
        }
    };

    let mut salt = None;
    if mode == BackupArtifactMode::Passphrase {
        let mut value = [0u8; SALT_LEN];
        read_exact_or_format(reader, &mut value, "salt")?;
        salt = Some(value);
    }

    let mut nonce = [0u8; NONCE_LEN];
    read_exact_or_format(reader, &mut nonce, "nonce")?;

    let mut id_len_bytes = [0u8; 2];
    read_exact_or_format(reader, &mut id_len_bytes, "source_id 长度")?;
    let id_len = usize::from(u16::from_le_bytes(id_len_bytes));
    if id_len > MAX_SOURCE_ID_LEN {
        return Err(SecretError::InvalidFormat(format!(
            "source_backup_id 长度非法：{id_len}"
        )));
    }
    let mut id_bytes = vec![0u8; id_len];
    read_exact_or_format(reader, &mut id_bytes, "source_id")?;
    let source_backup_id = String::from_utf8(id_bytes)
        .map_err(|_| SecretError::InvalidFormat("source_backup_id 非 UTF-8".to_owned()))?;

    let mut generated = [0u8; 8];
    read_exact_or_format(reader, &mut generated, "生成时间")?;
    let generated_at = i64::from_le_bytes(generated);

    let header_len = ARTIFACT_MAGIC.len()
        + 2
        + if salt.is_some() { SALT_LEN } else { 0 }
        + NONCE_LEN
        + 2
        + id_len
        + 8;
    let header = ArtifactHeader {
        mode,
        salt,
        nonce,
        source_backup_id,
        generated_at,
    };
    Ok((header, header_len))
}

/// AEAD 构造（与 chacha20poly1305::cipher.rs 一致）：
/// Poly1305 key = 块 0 前 32B；正文密钥流自块 1（偏移 64B）开始。
struct StreamSealer {
    cipher: XChaCha20,
    mac: Poly1305,
}

impl StreamSealer {
    fn new(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN]) -> Self {
        let mut cipher = XChaCha20::new(Key::from_slice(key), XNonce::from_slice(nonce));
        let mut mac_key = poly1305::Key::default();
        cipher.apply_keystream(&mut mac_key);
        let mac = Poly1305::new(&mac_key);
        mac_key.as_mut_slice().zeroize();
        cipher.seek(BLOCK_SIZE as u64);
        Self { cipher, mac }
    }

    /// 认证头（AAD，零填充）。
    fn authenticate_aad(&mut self, aad: &[u8]) {
        self.mac.update_padded(aad);
    }

    /// 认证一段密文（[`CHUNK_SIZE`] 为 16 字节整数倍，仅末段可能带零填充）。
    fn authenticate_ciphertext(&mut self, chunk: &[u8]) {
        self.mac.update_padded(chunk);
    }

    /// 长度块 + tag（aad_len LE || ct_len LE）。
    fn finish(mut self, aad_len: u64, ct_len: u64) -> [u8; TAG_LEN] {
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&aad_len.to_le_bytes());
        block[8..].copy_from_slice(&ct_len.to_le_bytes());
        self.mac
            .update(&[poly1305::Block::clone_from_slice(&block)]);
        let tag = self.mac.finalize();
        let mut out = [0u8; TAG_LEN];
        out.copy_from_slice(&tag);
        out
    }

    /// tag 校验（常量时间；失败 → [`SecretError::Decrypt`]）。
    fn verify(mut self, aad_len: u64, ct_len: u64, tag: &[u8; TAG_LEN]) -> Result<(), SecretError> {
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&aad_len.to_le_bytes());
        block[8..].copy_from_slice(&ct_len.to_le_bytes());
        self.mac
            .update(&[poly1305::Block::clone_from_slice(&block)]);
        self.mac
            .verify(&poly1305::Block::clone_from_slice(tag))
            .map_err(|_| SecretError::Decrypt)
    }
}

fn seal_file(
    src: &Path,
    dest: &Path,
    aad: &[u8],
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    total: u64,
) -> Result<u64, SecretError> {
    let mut reader = File::open(src)?;
    let mut writer = OpenOptions::new().write(true).create_new(true).open(dest)?;
    writer.write_all(aad)?;

    let mut sealer = StreamSealer::new(key, nonce);
    sealer.authenticate_aad(aad);
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut remaining = total;
    while remaining > 0 {
        let take = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        reader.read_exact(&mut buffer[..take])?;
        sealer.cipher.apply_keystream(&mut buffer[..take]);
        sealer.authenticate_ciphertext(&buffer[..take]);
        writer.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    let tag = sealer.finish(aad.len() as u64, total);
    writer.write_all(&tag)?;
    writer.sync_all()?;
    drop(writer);
    Ok(fs::metadata(dest)?.len())
}

fn open_file(
    src: &Path,
    dest: &Path,
    key: &[u8; KEY_LEN],
) -> Result<BackupArtifactInfo, SecretError> {
    let mut reader = File::open(src)?;
    let (header, header_len) = read_header(&mut reader)?;
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len < (header_len as u64) + (TAG_LEN as u64) {
        return Err(SecretError::InvalidFormat("产物长度非法".to_owned()));
    }
    let ct_len = file_len - header_len as u64 - TAG_LEN as u64;

    // 重新读取头字节作为 AAD（确定性序列化，与写入端一致）。
    reader.seek(SeekFrom::Start(0))?;
    let mut aad = vec![0u8; header_len];
    reader.read_exact(&mut aad)?;

    let mut sealer = StreamSealer::new(key, &header.nonce);
    sealer.authenticate_aad(&aad);

    // 第一遍：认证密文 + 读取 tag。
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut remaining = ct_len;
    while remaining > 0 {
        let take = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        reader.read_exact(&mut buffer[..take])?;
        sealer.authenticate_ciphertext(&buffer[..take]);
        remaining -= take as u64;
    }
    let mut tag = [0u8; TAG_LEN];
    reader.read_exact(&mut tag)?;
    sealer.verify(aad.len() as u64, ct_len, &tag)?;

    // 第二遍：解密写出（tag 已验证，篡改产物不会写出明文）。
    reader.seek(SeekFrom::Start(header_len as u64))?;
    let mut writer = OpenOptions::new().write(true).create_new(true).open(dest)?;
    let mut cipher = XChaCha20::new(Key::from_slice(key), XNonce::from_slice(&header.nonce));
    cipher.seek(BLOCK_SIZE as u64);
    let mut remaining = ct_len;
    while remaining > 0 {
        let take = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        reader.read_exact(&mut buffer[..take])?;
        cipher.apply_keystream(&mut buffer[..take]);
        writer.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    writer.sync_all()?;
    drop(writer);
    Ok(header.info())
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        // String 的 fmt::Write 不会失败。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn decode_master_key(value: &str) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let invalid =
        || SecretError::InvalidFormat("备份主密钥存储值非法（期望 64 位 hex）".to_owned());
    let raw = value.trim();
    if raw.len() != KEY_LEN * 2 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    for (index, byte) in key.iter_mut().enumerate() {
        let pair = raw.get(index * 2..index * 2 + 2).ok_or_else(invalid)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| invalid())?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use chacha20poly1305::aead::{Aead, KeyInit as AeadKeyInit, Payload};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce as AeadNonce};

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn sample_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn keychain_key(bytes: u8) -> BackupArtifactKey {
        BackupArtifactKey::Keychain(Zeroizing::new([bytes; KEY_LEN]))
    }

    #[test]
    fn artifact_roundtrip_keychain_and_passphrase_modes() {
        let dir = temp_dir();
        for (name, key) in [
            (
                "keychain",
                BackupArtifactKey::Keychain(Zeroizing::new([7u8; KEY_LEN])),
            ),
            (
                "passphrase",
                BackupArtifactKey::Passphrase {
                    key: Zeroizing::new([9u8; KEY_LEN]),
                    salt: [3u8; SALT_LEN],
                },
            ),
        ] {
            let src = dir.path().join(format!("{name}.src"));
            let dest = dir.path().join(format!("{name}.aebk"));
            let out = dir.path().join(format!("{name}.out"));
            let plaintext = sample_bytes(1024 * 1024 + 333);
            fs::write(&src, &plaintext).unwrap();

            let size =
                encrypt_backup_file(&src, &dest, "01JTEST0000000000000000AA", 42, &key).unwrap();
            assert_eq!(size, fs::metadata(&dest).unwrap().len());
            assert!(is_encrypted_backup_artifact(&dest));

            let info = inspect_backup_artifact(&dest).unwrap();
            assert_eq!(info.mode, key.mode());
            assert_eq!(info.source_backup_id, "01JTEST0000000000000000AA");
            assert_eq!(info.generated_at, 42);

            let decrypted = decrypt_backup_file(&dest, &out, key.key()).unwrap();
            assert_eq!(decrypted, info);
            assert_eq!(fs::read(&out).unwrap(), plaintext);
        }
    }

    /// 与 `chacha20poly1305::XChaCha20Poly1305` 单次加密逐字节对拍（同一构造）。
    #[test]
    fn streaming_output_matches_reference_aead() {
        let dir = temp_dir();
        let key_bytes = [0x42u8; KEY_LEN];
        let src = dir.path().join("big.src");
        let dest = dir.path().join("big.aebk");
        // 3 MiB + 1：跨多块（含尾块非 16 倍数）验证分块认证。
        let plaintext = sample_bytes(3 * 1024 * 1024 + 1);
        fs::write(&src, &plaintext).unwrap();

        encrypt_backup_file(
            &src,
            &dest,
            "01JTEST0000000000000000BB",
            7,
            &keychain_key(0x42),
        )
        .unwrap();

        let artifact = fs::read(&dest).unwrap();
        let (header, header_len) = read_header(&mut artifact.as_slice()).unwrap();
        let aad = &artifact[..header_len];
        let body = &artifact[header_len..];

        let cipher = XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key_bytes));
        let expected = cipher
            .encrypt(
                AeadNonce::from_slice(&header.nonce),
                Payload {
                    msg: &plaintext,
                    aad,
                },
            )
            .unwrap();
        assert_eq!(
            body,
            expected.as_slice(),
            "流式密文必须与参考实现逐字节一致"
        );

        // 反向：参考实现加密 → 本实现解密。
        let reference_artifact = {
            let mut bytes = header.to_bytes();
            bytes.extend_from_slice(&expected);
            bytes
        };
        let reference_path = dir.path().join("reference.aebk");
        fs::write(&reference_path, &reference_artifact).unwrap();
        let out = dir.path().join("reference.out");
        decrypt_backup_file(&reference_path, &out, &key_bytes).unwrap();
        assert_eq!(fs::read(&out).unwrap(), plaintext);
    }

    #[test]
    fn header_tamper_and_wrong_key_are_rejected_without_output() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest.aebk");
        fs::write(&src, sample_bytes(4096)).unwrap();
        encrypt_backup_file(
            &src,
            &dest,
            "01JTEST0000000000000000CC",
            1,
            &keychain_key(1),
        )
        .unwrap();

        // 头篡改（source id 字节翻转）→ Decrypt（头为 AAD）。
        let mut bytes = fs::read(&dest).unwrap();
        let id_offset = ARTIFACT_MAGIC.len() + 2 + NONCE_LEN + 2;
        bytes[id_offset] ^= 0x01;
        let tampered = dir.path().join("tampered.aebk");
        fs::write(&tampered, &bytes).unwrap();
        let out = dir.path().join("tampered.out");
        let error = decrypt_backup_file(&tampered, &out, &[1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(error, SecretError::Decrypt), "{error}");
        assert!(!out.exists(), "校验失败不得写出明文");

        // 密钥错误 → Decrypt。
        let wrong = dir.path().join("wrong.out");
        let error = decrypt_backup_file(&dest, &wrong, &[2u8; KEY_LEN]).unwrap_err();
        assert!(matches!(error, SecretError::Decrypt), "{error}");
        assert!(!wrong.exists());
    }

    #[test]
    fn invalid_flags_and_short_header_are_rejected() {
        let dir = temp_dir();
        // flags 非法（两位同置）。
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ARTIFACT_MAGIC);
        bytes.push(ARTIFACT_VERSION);
        bytes.push(FLAG_PASSPHRASE_MODE | FLAG_KEYCHAIN_MODE);
        bytes.extend_from_slice(&[0u8; NONCE_LEN]);
        let path = dir.path().join("bad-flags.aebk");
        fs::write(&path, &bytes).unwrap();
        let error = inspect_backup_artifact(&path).unwrap_err();
        assert!(matches!(error, SecretError::InvalidFormat(_)), "{error}");

        // 头截断。
        let short = dir.path().join("short.aebk");
        fs::write(&short, b"AEBK1\x01").unwrap();
        assert!(inspect_backup_artifact(&short).is_err());
    }

    #[test]
    fn passphrase_derivation_matches_salt_and_params() {
        let passphrase = SecretValue::new("unit-passphrase-m5-03");
        let params = BackupCryptoParams::test_only_fast();
        let (key, salt) = derive_passphrase_key(params, &passphrase).unwrap();
        let again = derive_passphrase_key_with_salt(params, &salt, &passphrase).unwrap();
        assert_eq!(key.as_ref(), again.as_ref());

        let other =
            derive_passphrase_key_with_salt(params, &salt, &SecretValue::new("other")).unwrap();
        assert_ne!(key.as_ref(), other.as_ref());

        // 生产参数与 ADR-020 冻结值一致（m=64MiB, t=3, p=1）。
        let a3 = BackupCryptoParams::a3();
        assert_eq!(a3.m_cost_kib, 64 * 1024);
        assert_eq!(a3.t_cost, 3);
        assert_eq!(a3.p_cost, 1);
    }

    #[test]
    fn master_key_create_load_and_availability() {
        use crate::store::{SecretStore, SecurityLevel};

        #[derive(Default)]
        struct MemoryStore(std::sync::Mutex<std::collections::HashMap<String, String>>);

        impl SecretStore for MemoryStore {
            fn level(&self) -> SecurityLevel {
                SecurityLevel::OsKeychain
            }
            fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
                self.0
                    .lock()
                    .unwrap()
                    .get(&reference.to_uri())
                    .map(|value| SecretValue::new(value.clone()))
                    .ok_or_else(|| SecretError::NotFound {
                        reference: reference.clone(),
                    })
            }
            fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
                self.0
                    .lock()
                    .unwrap()
                    .insert(reference.to_uri(), value.expose().to_owned());
                Ok(())
            }
            fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
                self.0.lock().unwrap().remove(&reference.to_uri());
                Ok(())
            }
        }

        let store = MemoryStore::default();
        assert_eq!(backup_master_key_uri(), "keychain://aether/backup/master");
        assert!(master_key_available(&store), "NotFound 视为可生成");
        let first = load_or_create_master_key(&store).unwrap();
        let second = load_or_create_master_key(&store).unwrap();
        assert_eq!(first.as_ref(), second.as_ref(), "主密钥须稳定复用");
        assert!(master_key_available(&store));

        // 存储值损坏 → 明确报错（不静默重建）。
        let reference = backup_master_key_ref().unwrap();
        store.set(&reference, &SecretValue::new("not-hex")).unwrap();
        assert!(load_or_create_master_key(&store).is_err());
        assert!(!master_key_available(&store));
    }

    #[test]
    fn encrypt_refuses_existing_destination() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        let dest = dir.path().join("dest.aebk");
        fs::write(&src, b"data").unwrap();
        fs::write(&dest, b"existing").unwrap();
        let error = encrypt_backup_file(
            &src,
            &dest,
            "01JTEST0000000000000000DD",
            1,
            &keychain_key(1),
        )
        .unwrap_err();
        assert!(matches!(error, SecretError::InvalidFormat(_)), "{error}");
        assert_eq!(fs::read(&dest).unwrap(), b"existing", "不得覆盖既有产物");
    }
}
