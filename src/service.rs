use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "com.bsync.daemon";

#[derive(Debug)]
pub enum ServiceError {
    Io(std::io::Error),
    HomeNotFound,
    CommandFailed(String),
    PidNotFound,
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::Io(e) => write!(f, "IO error: {e}"),
            ServiceError::HomeNotFound => write!(f, "Could not determine home directory"),
            ServiceError::CommandFailed(msg) => write!(f, "Command failed: {msg}"),
            ServiceError::PidNotFound => write!(f, "Could not find running daemon PID"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        ServiceError::Io(e)
    }
}

fn home_dir() -> Result<PathBuf, ServiceError> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| ServiceError::HomeNotFound)
}

pub fn install(binary_path: &Path, config_path: &Path) -> Result<(), ServiceError> {
    if cfg!(target_os = "macos") {
        install_launchd(binary_path, config_path)
    } else {
        install_systemd(binary_path, config_path)
    }
}

pub fn uninstall() -> Result<(), ServiceError> {
    if cfg!(target_os = "macos") {
        uninstall_launchd()
    } else {
        uninstall_systemd()
    }
}

pub fn restart() -> Result<(), ServiceError> {
    if cfg!(target_os = "macos") {
        restart_launchd()
    } else {
        restart_systemd()
    }
}

pub fn reload() -> Result<(), ServiceError> {
    let service_result = if cfg!(target_os = "macos") {
        reload_launchd()
    } else {
        reload_systemd()
    };

    match service_result {
        Ok(()) => Ok(()),
        Err(ServiceError::PidNotFound) | Err(ServiceError::CommandFailed(_)) => {
            reload_from_pid_file()
        }
        Err(e) => Err(e),
    }
}

fn pid_file_path() -> Result<PathBuf, ServiceError> {
    let home = home_dir()?;
    Ok(home.join(".config/bsync/bsync.pid"))
}

pub fn write_pid_file() -> Result<(), ServiceError> {
    let path = pid_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

pub fn remove_pid_file() {
    if let Ok(path) = pid_file_path() {
        let _ = fs::remove_file(path);
    }
}

fn reload_from_pid_file() -> Result<(), ServiceError> {
    let path = pid_file_path()?;
    let contents = fs::read_to_string(&path).map_err(|_| ServiceError::PidNotFound)?;
    let pid: i32 = contents
        .trim()
        .parse()
        .map_err(|_| ServiceError::PidNotFound)?;

    let ret = unsafe { libc::kill(pid, 0) };
    if ret != 0 {
        let _ = fs::remove_file(&path);
        return Err(ServiceError::PidNotFound);
    }

    let ret = unsafe { libc::kill(pid, libc::SIGHUP) };
    if ret != 0 {
        return Err(ServiceError::CommandFailed(format!(
            "kill({pid}, SIGHUP) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn launchd_plist_path() -> Result<PathBuf, ServiceError> {
    let home = home_dir()?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn generate_plist_content(binary_path: &Path, config_path: &Path) -> String {
    let binary = escape_xml(&binary_path.display().to_string());
    let config = escape_xml(&config_path.display().to_string());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>run</string>
        <string>--log-to-file</string>
        <string>--config</string>
        <string>{config}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#
    )
}

fn install_launchd(binary_path: &Path, config_path: &Path) -> Result<(), ServiceError> {
    let plist_path = launchd_plist_path()?;

    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let plist = generate_plist_content(binary_path, config_path);

    fs::write(&plist_path, plist)?;

    // Try to unload first (ignore errors if not loaded)
    let _ = Command::new("launchctl")
        .args(["unload", &plist_path.to_string_lossy()])
        .output();

    let output = Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .output()?;

    if !output.status.success() {
        return Err(ServiceError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    tracing::info!("Installed launchd service at {}", plist_path.display());
    Ok(())
}

fn uninstall_launchd() -> Result<(), ServiceError> {
    let plist_path = launchd_plist_path()?;

    if plist_path.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .output();
        fs::remove_file(&plist_path)?;
        tracing::info!("Removed launchd service");
    }

    Ok(())
}

fn reload_launchd() -> Result<(), ServiceError> {
    let output = Command::new("launchctl").args(["list", LABEL]).output()?;

    if !output.status.success() {
        return Err(ServiceError::PidNotFound);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse PID from launchctl list output (first line after header: "PID\tStatus\tLabel")
    // Or from `launchctl list <label>` which shows "\"PID\" = <num>;"
    for line in stdout.lines() {
        if let Some(pid_part) = line.strip_prefix("\"PID\" = ")
            && let Some(pid_str) = pid_part.strip_suffix(';')
            && let Ok(pid) = pid_str.trim().parse::<i32>()
        {
            let ret = unsafe { libc::kill(pid, libc::SIGHUP) };
            if ret != 0 {
                return Err(ServiceError::CommandFailed(format!(
                    "kill({pid}, SIGHUP) failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            return Ok(());
        }
    }

    Err(ServiceError::PidNotFound)
}

fn restart_launchd() -> Result<(), ServiceError> {
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{LABEL}");

    let output = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()?;

    if !output.status.success() {
        return Err(ServiceError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    Ok(())
}

fn systemd_unit_path() -> Result<PathBuf, ServiceError> {
    let home = home_dir()?;
    Ok(home.join(".config/systemd/user").join("bsync.service"))
}

fn generate_systemd_unit(binary_path: &Path, config_path: &Path) -> String {
    format!(
        r#"[Unit]
Description=Bidirectional file sync daemon
After=default.target

[Service]
Type=simple
ExecStart={binary} run --config {config}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        binary = binary_path.display(),
        config = config_path.display(),
    )
}

fn install_systemd(binary_path: &Path, config_path: &Path) -> Result<(), ServiceError> {
    let unit_path = systemd_unit_path()?;

    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let unit = generate_systemd_unit(binary_path, config_path);

    fs::write(&unit_path, unit)?;

    let output = Command::new("systemctl")
        .args(["--user", "enable", "--now", "bsync"])
        .output()?;

    if !output.status.success() {
        return Err(ServiceError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    tracing::info!("Installed systemd user service at {}", unit_path.display());
    Ok(())
}

fn uninstall_systemd() -> Result<(), ServiceError> {
    let unit_path = systemd_unit_path()?;

    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "bsync"])
        .output();

    if unit_path.exists() {
        fs::remove_file(&unit_path)?;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    tracing::info!("Removed systemd user service");
    Ok(())
}

fn restart_systemd() -> Result<(), ServiceError> {
    let output = Command::new("systemctl")
        .args(["--user", "restart", "bsync"])
        .output()?;

    if !output.status.success() {
        return Err(ServiceError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    Ok(())
}

fn reload_systemd() -> Result<(), ServiceError> {
    let output = Command::new("systemctl")
        .args(["--user", "kill", "-s", "SIGHUP", "bsync"])
        .output()?;

    if !output.status.success() {
        return Err(ServiceError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_xml() {
        assert_eq!(
            escape_xml("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
        assert_eq!(escape_xml("no special chars"), "no special chars");
        assert_eq!(escape_xml(""), "");
    }

    #[test]
    fn test_generate_plist_content() {
        let binary = Path::new("/usr/local/bin/bsync");
        let config = Path::new("/home/user/.config/bsync/config.toml");
        let plist = generate_plist_content(binary, config);

        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains("<string>/usr/local/bin/bsync</string>"));
        assert!(plist.contains("<string>/home/user/.config/bsync/config.toml</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<true/>"));
    }

    #[test]
    fn test_generate_systemd_unit() {
        let binary = Path::new("/usr/local/bin/bsync");
        let config = Path::new("/home/user/.config/bsync/config.toml");
        let unit = generate_systemd_unit(binary, config);

        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("Type=simple"));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/bsync run --config /home/user/.config/bsync/config.toml"
        ));
        assert!(unit.contains("Restart=always"));
    }
}
