//! Access to the OS secret store an agent CLI may use instead of a plain file.
//!
//! Only macOS needs this: Claude Code keeps its OAuth credential in the login
//! Keychain there and treats `.credentials.json` as a fallback. Windows and
//! Linux builds of both CLIs use plain files, so this module reports
//! [`Backend::File`] and the callers write files.

/// Where a CLI's credential actually lives on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// A plain file inside the configuration home.
    File,
    /// The macOS login Keychain.
    Keychain,
}

#[cfg(not(target_os = "macos"))]
pub use stub::*;

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::Result;

    /// Keychain storage is unavailable off macOS.
    pub fn is_available() -> bool {
        false
    }

    pub fn read(_service: &str) -> Result<Option<String>> {
        Ok(None)
    }

    pub fn write(_service: &str, _secret: &str) -> Result<()> {
        unreachable!("the Keychain is only used on macOS")
    }

    pub fn delete(_service: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "macos")]
mod macos {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    use anyhow::{Context, Result, bail};

    /// `security` exit code for "the item does not exist".
    const ERR_ITEM_NOT_FOUND: i32 = 44;

    /// The Keychain account name Claude Code stores items under: the login name.
    fn account_name() -> &'static str {
        static NAME: OnceLock<String> = OnceLock::new();
        NAME.get_or_init(|| {
            std::env::var("USER")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "claude-code-user".to_string())
        })
    }

    /// Whether the `security` tool is usable. A Keychain that cannot be reached
    /// (an SSH session with no login Keychain unlocked, say) must not be
    /// mistaken for a Keychain holding no account.
    pub fn is_available() -> bool {
        std::path::Path::new("/usr/bin/security").exists()
    }

    /// Reads a generic password. `Ok(None)` means the item does not exist.
    pub fn read(service: &str) -> Result<Option<String>> {
        let output = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-a", account_name(), "-s", service, "-w"])
            .output()
            .context("running /usr/bin/security")?;
        if output.status.code() == Some(ERR_ITEM_NOT_FOUND) {
            return Ok(None);
        }
        if !output.status.success() {
            bail!(
                "reading Keychain item {service:?}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let secret = String::from_utf8(output.stdout).context("the Keychain item is not valid UTF-8")?;
        // `security` appends exactly one newline to the secret it prints.
        Ok(Some(secret.strip_suffix('\n').unwrap_or(&secret).to_string()))
    }

    /// Creates or replaces a generic password.
    ///
    /// The secret is passed as hex on `security`'s own stdin command stream, so
    /// it never appears in this process's argument list where any user on the
    /// machine could read it from `ps`.
    pub fn write(service: &str, secret: &str) -> Result<()> {
        let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
        let command = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            shell_quote(account_name()),
            shell_quote(service),
        );
        let mut child = Command::new("/usr/bin/security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running /usr/bin/security")?;
        child
            .stdin
            .take()
            .context("security did not accept input")?
            .write_all(command.as_bytes())
            .context("sending the command to security")?;
        let output = child.wait_with_output().context("waiting for security")?;
        if !output.status.success() {
            bail!(
                "writing Keychain item {service:?}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Deletes a generic password; a missing item is success.
    pub fn delete(service: &str) -> Result<()> {
        let output = Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-a", account_name(), "-s", service])
            .output()
            .context("running /usr/bin/security")?;
        if output.status.success() || output.status.code() == Some(ERR_ITEM_NOT_FOUND) {
            return Ok(());
        }
        bail!(
            "deleting Keychain item {service:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    /// Quotes a value for `security`'s interactive command parser, which splits
    /// on whitespace and honours double quotes.
    fn shell_quote(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', r"\\").replace('"', "\\\""))
    }
}
