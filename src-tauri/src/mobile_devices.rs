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

impl DeviceStore {
    pub fn load(account: String) -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            let devices =
                match security_framework::passwords::get_generic_password(SERVICE, &account) {
                    Ok(bytes) => serde_json::from_slice(&bytes)
                        .map_err(|_| "키체인에 저장된 휴대폰 정보를 읽을 수 없습니다.")?,
                    Err(error) if error.code() == -25300 => Vec::new(), // errSecItemNotFound
                    Err(_) => return Err("macOS 키체인에 접근할 수 없습니다.".into()),
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
            security_framework::passwords::set_generic_password(
                SERVICE,
                account,
                &serde_json::to_vec(&updated).map_err(|_| ())?,
            )
            .map_err(|_| ())?;
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
