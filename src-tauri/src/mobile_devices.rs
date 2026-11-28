use crate::mobile_reply::ResultCode;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct SavedDevice {
    pub id: String,
    pub label: String,
    pub host: String,
    pub keys: [[u8; 32]; 3],
    pub accepted_request: String,
    pub permit: Option<String>,
    pub revoking: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_send: Option<(String, i64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_sends: Vec<SavedSend>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct SavedSend {
    pub id: String,
    pub until: i64,
    pub outcome: ResultCode,
}

#[derive(Clone)]
pub(super) struct DeviceStore {
    account: Option<String>,
    devices: Arc<Mutex<Vec<SavedDevice>>>,
}

#[cfg(target_os = "macos")]
const SERVICE: &str = "Offtab mobile pairing";

#[cfg(target_os = "macos")]
fn read_keychain(account: &str) -> Result<Option<Vec<u8>>, ()> {
    if crate::pairing_keychain::enabled() {
        return match crate::pairing_keychain::request(account, None).map_err(|_| ())? {
            (0, Some(bytes)) => Ok(Some(bytes)),
            (-25300, _) => Ok(None), // errSecItemNotFound
            _ => Err(()),
        };
    }
    match security_framework::passwords::get_generic_password(SERVICE, account) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.code() == -25300 => Ok(None),
        Err(_) => Err(()),
    }
}

#[cfg(target_os = "macos")]
fn write_keychain(account: &str, data: &[u8]) -> Result<(), ()> {
    if crate::pairing_keychain::enabled() {
        return (crate::pairing_keychain::request(account, Some(data)).map_err(|_| ())?.0 == 0)
            .then_some(())
            .ok_or(());
    }
    security_framework::passwords::set_generic_password(SERVICE, account, data).map_err(|_| ())
}

impl DeviceStore {
    pub fn load(account: String) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            let devices = match read_keychain(&account).map_err(|_| "macOS 키체인에 접근할 수 없습니다.")? {
                Some(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|_| "키체인에 저장된 휴대폰 정보를 읽을 수 없습니다.")?,
                None => Vec::new(),
            };
            Ok(Self {
                account: Some(account),
                devices: Arc::new(Mutex::new(devices)),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = account;
            Err("이 환경에서는 휴대폰 연결을 안전하게 저장할 수 없습니다.".into())
        }
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Self {
            account: None,
            devices: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn list(&self) -> Result<Vec<SavedDevice>, ()> {
        self.devices
            .lock()
            .map(|devices| devices.clone())
            .map_err(|_| ())
    }

    pub fn update(
        &self,
        apply: impl FnOnce(&mut Vec<SavedDevice>) -> Result<(), ()>,
    ) -> Result<(), ()> {
        let mut devices = self.devices.lock().map_err(|_| ())?;
        let mut updated = devices.clone();
        apply(&mut updated)?;
        if updated == *devices {
            return Ok(());
        }
        if let Some(account) = &self.account {
            #[cfg(target_os = "macos")]
            write_keychain(account, &serde_json::to_vec(&updated).map_err(|_| ())?)?;
            #[cfg(not(target_os = "macos"))]
            {
                let _ = account;
                return Err(());
            }
        }
        *devices = updated;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), ()> {
        self.update(|devices| {
            devices.retain(|device| device.id != id);
            Ok(())
        })
    }
}
