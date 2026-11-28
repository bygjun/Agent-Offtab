use base64::{engine::general_purpose::STANDARD, Engine};
use core_foundation::url::CFURL;
use security_framework::os::macos::code_signing::{
    Flags, GuestAttributes, SecCode, SecRequirement, SecStaticCode,
};
use serde::Deserialize;
use std::{
    fs, io,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const NAME: &str = "OfftabPairingKeychain";
const ID: &str = "local.sessiondesk.app.pairing-keychain";
const SIGNER: Option<&str> = option_env!("OFFTAB_PAIRING_SIGNER");
const MAX_BYTES: usize = 1_048_576;

pub fn enabled() -> bool {
    SIGNER.is_some()
}

fn invalid_helper() -> io::Error {
    io::Error::other("Offtab pairing helper could not be verified")
}

fn verify_file(path: &Path, requirement: &SecRequirement) -> io::Result<()> {
    if !path.symlink_metadata()?.is_file() {
        return Err(invalid_helper());
    }
    let url = CFURL::from_path(path, false).ok_or_else(invalid_helper)?;
    SecStaticCode::from_path(&url, Flags::NONE)
        .and_then(|code| code.check_validity(Flags::STRICT_VALIDATE, requirement))
        .map_err(|_| invalid_helper())
}

fn installed_helper(requirement: &SecRequirement) -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(invalid_helper)?;
    let parent = home.join("Library/Application Support/local.sessiondesk.app");
    let directory = parent.join("PairingKeychain");
    let executable = directory.join(NAME);
    if executable.symlink_metadata().is_ok() {
        return Ok(executable);
    }
    let app = std::env::current_exe()?;
    let source = app
        .ancestors()
        .nth(2)
        .ok_or_else(invalid_helper)?
        .join("Resources")
        .join(NAME);
    verify_file(&source, requirement)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    io::copy(&mut fs::File::open(source)?, temporary.as_file_mut())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&executable) {
        Ok(_) => Ok(executable),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(executable),
        Err(error) => Err(error.error),
    }
}

fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().filter(|directory| Path::is_absolute(directory))
}

#[derive(Deserialize)]
struct Response {
    status: i32,
    data: Option<String>,
}

pub fn request(account: &str, data: Option<&[u8]>) -> io::Result<(i32, Option<Vec<u8>>)> {
    let signer = SIGNER.ok_or_else(invalid_helper)?;
    let requirement: SecRequirement =
        format!("identifier \"{ID}\" and certificate leaf = H\"{signer}\"")
            .parse()
            .map_err(|_| invalid_helper())?;
    let executable = installed_helper(&requirement)?;
    verify_file(&executable, &requirement)?;
    let payload = serde_json::to_vec(
        &serde_json::json!({ "account": account, "data": data.map(|bytes| STANDARD.encode(bytes)) }),
    )?;
    if payload.len() > MAX_BYTES {
        return Err(io::Error::other("Offtab pairing data exceeds helper limit"));
    }
    let mut child = Command::new(executable)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut guest = GuestAttributes::new();
        guest.set_pid(child.id() as i32);
        // Verify the running child before sending keys or accepting its response.
        SecCode::copy_guest_with_attribues(None, &guest, Flags::NONE)
            .and_then(|code| code.check_validity(Flags::NONE, &requirement))
            .map_err(|_| invalid_helper())?;
        child
            .stdin
            .take()
            .ok_or_else(invalid_helper)?
            .write_all(&payload)?;
        let mut response = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(invalid_helper)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut response)?;
        if response.len() > MAX_BYTES || !child.wait()?.success() {
            return Err(invalid_helper());
        }
        let response: Response = serde_json::from_slice(&response)?;
        let data = response
            .data
            .map(|text| STANDARD.decode(text))
            .transpose()
            .map_err(io::Error::other)?;
        Ok((response.status, data))
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
