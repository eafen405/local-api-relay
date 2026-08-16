//! Opt-in HKCU Run autostart for the desktop shell.
//!
//! Default is off. The shell only writes or deletes its own Run value when the
//! user explicitly toggles the tray item; it never registers a service and
//! never creates a background process on its own.

pub const AUTOSTART_VALUE_NAME: &str = "LocalApiRelay";
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
fn run_key() -> anyhow::Result<winreg::RegKey> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    Ok(hkcu.create_subkey(RUN_KEY_PATH)?.0)
}

#[cfg(not(windows))]
fn run_key() -> anyhow::Result<()> {
    Ok(())
}

/// Returns true when the current user's Run key contains this app's value.
pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        if let Ok(key) = run_key() {
            key.get_value::<String, _>(AUTOSTART_VALUE_NAME).is_ok()
        } else {
            false
        }
    }
    #[cfg(not(windows))]
    {
        let _ = run_key();
        false
    }
}

/// Writes or removes the current user's Run value.
pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let key = run_key()?;
        if enabled {
            let exe = std::env::current_exe()?;
            let command = format!("\"{}\"", exe.display());
            key.set_value(AUTOSTART_VALUE_NAME, &command)?;
        } else {
            match key.delete_value(AUTOSTART_VALUE_NAME) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = run_key();
        anyhow::bail!("autostart is only supported on Windows")
    }
}
