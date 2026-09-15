//! A3 降级路径：加密文件 `secrets.enc`（Argon2id m=64MB,t=3,p=1 + XChaCha20-Poly1305）。
//!
//! 仅当 OS 凭据库不可用时启用（[`crate::SecurityManager`] 统一判定）；
//! 口令启动时输入、仅驻留内存；派生密钥在会话内缓存，文件每次写入都换新 nonce。
//!
//! 文件信封（JSON，可读，密文与盐/nonce 为 base64）：
//! `{ v, kdf, m_cost_kib, t_cost, p_cost, salt, nonce, ciphertext }`；
//! KDF 参数、盐与 nonce 全部作为 AEAD 附加数据参与认证（信封篡改直接解密失败）。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::SecretError;
use crate::reference::KeychainRef;
use crate::store::{random_bytes, SecretStore, SecretValue, SecurityLevel};

/// A3 冻结的 Argon2id 内存成本（KiB，即 64MB）。
pub const A3_M_COST_KIB: u32 = 64 * 1024;
/// A3 冻结的 Argon2id 迭代次数。
pub const A3_T_COST: u32 = 3;
/// A3 冻结的 Argon2id 并行度。
pub const A3_P_COST: u32 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const FILE_VERSION: u32 = 1;
const KDF_NAME: &str = "argon2id";
// 读取他方文件时的参数上限（防止被构造文件拖入超大内存开销）。
const MAX_M_COST_KIB: u32 = 1024 * 1024;
const MAX_T_COST: u32 = 16;
const MAX_P_COST: u32 = 16;
const MIN_M_COST_KIB: u32 = 1024;

/// 加密文件 KDF 参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileCryptoParams {
    /// Argon2id 内存成本（KiB）。
    pub m_cost_kib: u32,
    /// Argon2id 迭代次数。
    pub t_cost: u32,
    /// Argon2id 并行度。
    pub p_cost: u32,
}

impl FileCryptoParams {
    /// A3 冻结参数（生产降级路径唯一允许的取值）。
    #[must_use]
    pub const fn a3() -> Self {
        Self {
            m_cost_kib: A3_M_COST_KIB,
            t_cost: A3_T_COST,
            p_cost: A3_P_COST,
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

impl Default for FileCryptoParams {
    fn default() -> Self {
        Self::a3()
    }
}

#[derive(Serialize, Deserialize)]
struct FileEnvelope {
    v: u32,
    kdf: String,
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct PlaintextFile {
    v: u32,
    entries: BTreeMap<String, SecretValue>,
}

/// `secrets.enc` 存储后端。
pub struct EncryptedFileStore {
    path: PathBuf,
    params: FileCryptoParams,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    entries: Mutex<BTreeMap<String, SecretValue>>,
}

impl fmt::Debug for EncryptedFileStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self
            .entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0);
        f.debug_struct("EncryptedFileStore")
            .field("path", &self.path)
            .field("params", &self.params)
            .field("entries", &count)
            .finish_non_exhaustive()
    }
}

impl EncryptedFileStore {
    /// 文件存在则打开，否则以给定参数创建。
    pub fn open_or_create(
        path: &Path,
        passphrase: &SecretValue,
        params: FileCryptoParams,
    ) -> Result<Self, SecretError> {
        if path.exists() {
            Self::open(path, passphrase)
        } else {
            Self::create(path, passphrase, params)
        }
    }

    /// 打开既有文件（缺失/损坏/口令错误均返回错误，不静默重建）。
    pub fn open(path: &Path, passphrase: &SecretValue) -> Result<Self, SecretError> {
        let bytes = fs::read(path)?;
        let envelope: FileEnvelope = serde_json::from_slice(&bytes)?;
        if envelope.v != FILE_VERSION {
            return Err(SecretError::InvalidFormat("不支持的密钥文件版本".into()));
        }
        if envelope.kdf != KDF_NAME {
            return Err(SecretError::InvalidFormat("不支持的 KDF".into()));
        }
        let params = FileCryptoParams {
            m_cost_kib: envelope.m_cost_kib,
            t_cost: envelope.t_cost,
            p_cost: envelope.p_cost,
        };
        validate_params(params)?;

        let salt = decode_fixed::<SALT_LEN>(&envelope.salt, "salt")?;
        let nonce = decode_fixed::<NONCE_LEN>(&envelope.nonce, "nonce")?;
        let ciphertext = BASE64
            .decode(&envelope.ciphertext)
            .map_err(|_| SecretError::InvalidFormat("ciphertext 不是合法 base64".into()))?;
        if ciphertext.len() < 16 {
            return Err(SecretError::InvalidFormat("ciphertext 长度非法".into()));
        }

        let key = derive_key(params, &salt, passphrase)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(key.as_ref()));
        let aad = associated_data(params, &salt, &nonce);
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| SecretError::Decrypt)?,
        );
        let parsed: PlaintextFile = serde_json::from_slice(&plaintext)?;
        if parsed.v != FILE_VERSION {
            return Err(SecretError::InvalidFormat("明文载荷版本非法".into()));
        }

        Ok(Self {
            path: path.to_path_buf(),
            params,
            salt,
            key,
            entries: Mutex::new(parsed.entries),
        })
    }

    /// 新建空文件（参数非法或写盘失败则返回错误）。
    pub fn create(
        path: &Path,
        passphrase: &SecretValue,
        params: FileCryptoParams,
    ) -> Result<Self, SecretError> {
        validate_params(params)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut salt = [0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        let key = derive_key(params, &salt, passphrase)?;
        let store = Self {
            path: path.to_path_buf(),
            params,
            salt,
            key,
            entries: Mutex::new(BTreeMap::new()),
        };
        {
            let entries = store.lock_entries()?;
            store.persist(&entries)?;
        }
        Ok(store)
    }

    /// 文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前生效的 KDF 参数。
    #[must_use]
    pub fn params(&self) -> FileCryptoParams {
        self.params
    }

    fn lock_entries(&self) -> Result<MutexGuard<'_, BTreeMap<String, SecretValue>>, SecretError> {
        self.entries
            .lock()
            .map_err(|_| SecretError::InvalidFormat("密钥文件状态锁已中毒".into()))
    }

    fn persist(&self, entries: &BTreeMap<String, SecretValue>) -> Result<(), SecretError> {
        let plaintext = Zeroizing::new(serde_json::to_vec(&PlaintextFile {
            v: FILE_VERSION,
            entries: entries.clone(),
        })?);

        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(self.key.as_ref()));
        let aad = associated_data(self.params, &self.salt, &nonce);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SecretError::InvalidFormat("加密失败".into()))?;

        let envelope = FileEnvelope {
            v: FILE_VERSION,
            kdf: KDF_NAME.to_string(),
            m_cost_kib: self.params.m_cost_kib,
            t_cost: self.params.t_cost,
            p_cost: self.params.p_cost,
            salt: BASE64.encode(self.salt),
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(&ciphertext),
        };
        let body = Zeroizing::new(serde_json::to_vec(&envelope)?);
        write_atomic(&self.path, &body)
    }
}

impl SecretStore for EncryptedFileStore {
    fn level(&self) -> SecurityLevel {
        SecurityLevel::Degraded
    }

    fn get(&self, reference: &KeychainRef) -> Result<SecretValue, SecretError> {
        let entries = self.lock_entries()?;
        entries
            .get(&reference.to_uri())
            .cloned()
            .ok_or_else(|| SecretError::NotFound {
                reference: reference.clone(),
            })
    }

    fn set(&self, reference: &KeychainRef, value: &SecretValue) -> Result<(), SecretError> {
        // persist 失败时回滚内存态，保持「内存 == 磁盘」。
        let mut entries = self.lock_entries()?;
        let uri = reference.to_uri();
        let previous = entries.insert(uri.clone(), value.clone());
        if let Err(err) = self.persist(&entries) {
            match previous {
                Some(previous) => {
                    entries.insert(uri, previous);
                }
                None => {
                    entries.remove(&uri);
                }
            }
            return Err(err);
        }
        Ok(())
    }

    fn delete(&self, reference: &KeychainRef) -> Result<(), SecretError> {
        let mut entries = self.lock_entries()?;
        let uri = reference.to_uri();
        let previous = entries.remove(&uri).ok_or_else(|| SecretError::NotFound {
            reference: reference.clone(),
        })?;
        if let Err(err) = self.persist(&entries) {
            entries.insert(uri, previous);
            return Err(err);
        }
        Ok(())
    }
}

fn derive_key(
    params: FileCryptoParams,
    salt: &[u8; SALT_LEN],
    passphrase: &SecretValue,
) -> Result<Zeroizing<[u8; KEY_LEN]>, SecretError> {
    let argon_params = Params::new(
        params.m_cost_kib,
        params.t_cost,
        params.p_cost,
        Some(KEY_LEN),
    )
    .map_err(|err| SecretError::InvalidFormat(format!("Argon2 参数非法：{err}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(passphrase.expose().as_bytes(), salt, key.as_mut())
        .map_err(|err| SecretError::InvalidFormat(format!("Argon2 派生失败：{err}")))?;
    Ok(key)
}

fn associated_data(
    params: FileCryptoParams,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Vec<u8> {
    format!(
        "aether-secrets|v={FILE_VERSION}|kdf={KDF_NAME}|m={}|t={}|p={}|salt={}|nonce={}",
        params.m_cost_kib,
        params.t_cost,
        params.p_cost,
        BASE64.encode(salt),
        BASE64.encode(nonce)
    )
    .into_bytes()
}

fn validate_params(params: FileCryptoParams) -> Result<(), SecretError> {
    let in_bounds = (MIN_M_COST_KIB..=MAX_M_COST_KIB).contains(&params.m_cost_kib)
        && (1..=MAX_T_COST).contains(&params.t_cost)
        && (1..=MAX_P_COST).contains(&params.p_cost);
    if in_bounds {
        Ok(())
    } else {
        Err(SecretError::InvalidFormat("KDF 参数超出允许范围".into()))
    }
}

fn decode_fixed<const N: usize>(value: &str, field: &str) -> Result<[u8; N], SecretError> {
    let raw = BASE64
        .decode(value)
        .map_err(|_| SecretError::InvalidFormat(format!("{field} 不是合法 base64")))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| SecretError::InvalidFormat(format!("{field} 长度非法")))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    let written = write_private(&tmp, bytes);
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}
