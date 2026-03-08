use anyhow::{anyhow, Context, Result};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::ffi::c_void;

#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub public_key: Vec<u8>,
    pkcs8: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredDeviceIdentity {
    device_id: String,
    pkcs8: Vec<u8>,
}

impl DeviceIdentity {
    pub fn load_or_create() -> Result<Self> {
        let path = identity_path();
        if let Ok(content) = std::fs::read(&path) {
            if let Ok(stored) = serde_json::from_slice::<StoredDeviceIdentity>(&content) {
                #[cfg(windows)]
                let (decrypted_pkcs8, needs_migration) = match dpapi_unprotect(&stored.pkcs8) {
                    Ok(pkcs8) => (pkcs8, false),
                    Err(_) => (stored.pkcs8.clone(), true),
                };
                #[cfg(not(windows))]
                let decrypted_pkcs8 = stored.pkcs8.clone();

                let key = Ed25519KeyPair::from_pkcs8(&decrypted_pkcs8)
                    .map_err(|_| anyhow!("invalid stored device key"))?;

                #[cfg(windows)]
                if needs_migration {
                    let migrated = StoredDeviceIdentity {
                        device_id: stored.device_id.clone(),
                        pkcs8: protect_pkcs8_for_storage(&decrypted_pkcs8)
                            .context("encrypt legacy plaintext device key with DPAPI")?,
                    };
                    write_identity_file_atomically(&path, &migrated)
                        .context("migrate legacy plaintext device identity to protected storage")?;
                }

                return Ok(Self {
                    device_id: stored.device_id,
                    public_key: key.public_key().as_ref().to_vec(),
                    pkcs8: decrypted_pkcs8,
                });
            }
        }

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| anyhow!("failed to generate device keypair"))?;
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| anyhow!("failed to decode generated keypair"))?;
        let stored = StoredDeviceIdentity {
            device_id: uuid::Uuid::new_v4().to_string(),
            pkcs8: protect_pkcs8_for_storage(pkcs8.as_ref())?,
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create identity dir {}", parent.display()))?;
        }
        write_identity_file_atomically(&path, &stored)?;

        Ok(Self {
            device_id: stored.device_id,
            public_key: key.public_key().as_ref().to_vec(),
            pkcs8: pkcs8.as_ref().to_vec(),
        })
    }

    pub fn sign_challenge(&self, challenge: &[u8], session_id: &str) -> Result<Vec<u8>> {
        let key = Ed25519KeyPair::from_pkcs8(&self.pkcs8)
            .map_err(|_| anyhow!("invalid device key material"))?;
        let mut payload = Vec::with_capacity(challenge.len() + session_id.len());
        payload.extend_from_slice(challenge);
        payload.extend_from_slice(session_id.as_bytes());
        Ok(key.sign(&payload).as_ref().to_vec())
    }
}

fn write_identity_file_atomically(path: &Path, stored: &StoredDeviceIdentity) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(stored)?;
    let pid = std::process::id();
    let suffix = uuid::Uuid::new_v4();
    let tmp_path = path.with_file_name(format!("device_identity.json.{pid}.{suffix}.tmp"));

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("open temp identity file {}", tmp_path.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write temp identity file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp identity file {}", tmp_path.display()))?;
    }

    #[cfg(windows)]
    {
        write_windows_protected_file(&tmp_path, &bytes)?;
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        std::fs::write(&tmp_path, &bytes)
            .with_context(|| format!("write temp identity file {}", tmp_path.display()))?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("rename identity file {}", path.display()))?;

    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent)
            .with_context(|| format!("open identity dir {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("sync identity dir {}", parent.display()))?;
    }
    Ok(())
}

fn protect_pkcs8_for_storage(pkcs8: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi_protect(pkcs8).context("encrypt device key with DPAPI")
    }
    #[cfg(not(windows))]
    {
        Ok(pkcs8.to_vec())
    }
}

#[cfg(windows)]
fn write_windows_protected_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::System::Memory::LocalFree;

    let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    path_wide.push(0);

    let mut sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;OW)".encode_utf16().collect();
    sddl.push(0);

    let mut security_descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1 as u32,
            &mut security_descriptor,
            None,
        )
        .ok()
        .context("build protected ACL security descriptor")?;
    }

    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.0,
        bInheritHandle: false.into(),
    };

    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
            Some(&mut security_attributes),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            HANDLE(null_mut()),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        unsafe {
            let _ = LocalFree(HLOCAL(security_descriptor.0 as *mut c_void));
        }
        return Err(anyhow!("failed to create protected identity file"));
    }

    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0 as *mut c_void) };
    let result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("write temp identity file {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp identity file {}", path.display()))?;
        Ok(())
    })();

    drop(file);
    unsafe {
        let _ = LocalFree(HLOCAL(security_descriptor.0 as *mut c_void));
    }

    result
}

#[cfg(windows)]
fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, DATA_BLOB,
    };
    use windows::Win32::System::Memory::LocalFree;

    let mut input = DATA_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = DATA_BLOB::default();

    unsafe {
        CryptProtectData(
            &mut input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()
        .context("CryptProtectData failed")?;

        let encrypted = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut c_void));
        Ok(encrypted)
    }
}

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, DATA_BLOB,
    };
    use windows::Win32::System::Memory::LocalFree;

    let mut input = DATA_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = DATA_BLOB::default();

    unsafe {
        CryptUnprotectData(
            &mut input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .ok()
        .context("CryptUnprotectData failed")?;

        let decrypted = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData as *mut c_void));
        Ok(decrypted)
    }
}

fn identity_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    } else if cfg!(target_os = "macos") {
        dirs_fallback_home().join("Library/Application Support")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_fallback_home().join(".config"))
    };
    base.join("tsod").join("device_identity.json")
}

fn dirs_fallback_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
