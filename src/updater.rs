use std::env;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use flate2::read::GzDecoder;
use sha2::Digest;
use sha2::Sha256;

const REPO: &str = "lilong7676/codex-pool";
const BIN_NAME: &str = "codex-pool";

#[derive(Debug, Clone)]
pub struct InstalledBinary {
    pub install_path: PathBuf,
    pub requested_version: String,
    pub reported_version: Option<String>,
}

pub fn normalize_requested_version(version: Option<&str>) -> Option<String> {
    version.map(|value| {
        let trimmed = value.trim();
        if trimmed.starts_with('v') {
            trimmed.to_string()
        } else {
            format!("v{trimmed}")
        }
    })
}

pub fn resolve_install_path(bin_name: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os("CODEX_POOL_BIN_PATH") {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = find_in_path(bin_name) {
        return Ok(path);
    }

    if let Ok(current_exe) = env::current_exe() {
        if current_exe
            .file_name()
            .and_then(|value| value.to_str())
            .map(|value| value == bin_name)
            .unwrap_or(false)
        {
            return Ok(current_exe);
        }
    }

    let home = dirs::home_dir().context("failed to resolve HOME for install path")?;
    Ok(home.join(".local").join("bin").join(bin_name))
}

pub fn current_installed_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_version_output(&String::from_utf8_lossy(&output.stdout))
}

pub async fn download_and_install(
    version: Option<&str>,
    install_path: &Path,
) -> Result<InstalledBinary> {
    let requested_version = normalize_requested_version(version).unwrap_or_else(|| "latest".into());
    let target = detect_target()?;
    let archive_name = format!("{BIN_NAME}-{target}.tar.gz");
    let archive_url = resolve_archive_url(&archive_name, version);
    let checksum_url = format!("{archive_url}.sha256");

    let client = reqwest::Client::builder()
        .user_agent(format!("codex-pool/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")?;

    let archive_bytes = download_bytes(&client, &archive_url).await?;
    let checksum_text = download_text(&client, &checksum_url).await?;
    let expected_checksum = parse_checksum(&checksum_text, &archive_name)?;
    verify_checksum(&archive_bytes, &expected_checksum)?;

    let temp_dir = tempfile::tempdir().context("failed to create temp dir for update")?;
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(temp_dir.path())
        .context("failed to unpack release archive")?;

    let extracted_binary = temp_dir.path().join(BIN_NAME);
    if !extracted_binary.is_file() {
        anyhow::bail!("release archive did not contain `{BIN_NAME}`");
    }

    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let temp_install_path = install_path.with_extension(format!("download-{}", std::process::id()));
    if temp_install_path.exists() {
        let _ = fs::remove_file(&temp_install_path);
    }

    fs::copy(&extracted_binary, &temp_install_path).with_context(|| {
        format!(
            "failed to stage updated binary from {} to {}",
            extracted_binary.display(),
            temp_install_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&temp_install_path, permissions).with_context(|| {
            format!(
                "failed to set executable permissions on {}",
                temp_install_path.display()
            )
        })?;
    }

    match fs::rename(&temp_install_path, install_path) {
        Ok(()) => {}
        Err(error) => {
            if install_path.exists() {
                fs::remove_file(install_path)
                    .with_context(|| format!("failed to replace {}", install_path.display()))?;
                fs::rename(&temp_install_path, install_path).with_context(|| {
                    format!(
                        "failed to move updated binary into {}",
                        install_path.display()
                    )
                })?;
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "failed to move updated binary into {}",
                        install_path.display()
                    )
                });
            }
        }
    }

    Ok(InstalledBinary {
        install_path: install_path.to_path_buf(),
        requested_version,
        reported_version: current_installed_version(install_path),
    })
}

fn detect_target() -> Result<String> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;

    let os_part = match os {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => anyhow::bail!("unsupported OS: {other}"),
    };
    let arch_part = match arch {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => anyhow::bail!("unsupported architecture: {other}"),
    };

    if os_part == "unknown-linux-gnu" && arch_part != "x86_64" {
        anyhow::bail!("only x86_64 Linux builds are published right now");
    }

    Ok(format!("{arch_part}-{os_part}"))
}

fn resolve_archive_url(archive_name: &str, version: Option<&str>) -> String {
    if let Some(tag) = normalize_requested_version(version) {
        format!("https://github.com/{REPO}/releases/download/{tag}/{archive_name}")
    } else {
        format!("https://github.com/{REPO}/releases/latest/download/{archive_name}")
    }
}

async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("download failed for {url} -> {status}");
    }
    Ok(response
        .bytes()
        .await
        .with_context(|| format!("failed to read response body for {url}"))?
        .to_vec())
}

async fn download_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("download failed for {url} -> {status}");
    }
    response
        .text()
        .await
        .with_context(|| format!("failed to read response body for {url}"))
}

fn verify_checksum(bytes: &[u8], expected_checksum: &str) -> Result<()> {
    let actual_checksum = format!("{:x}", Sha256::digest(bytes));
    if actual_checksum != expected_checksum.to_ascii_lowercase() {
        anyhow::bail!(
            "checksum mismatch: expected {}, got {}",
            expected_checksum,
            actual_checksum
        );
    }
    Ok(())
}

fn parse_checksum(contents: &str, archive_name: &str) -> Result<String> {
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(file_name) = parts.next() else {
            continue;
        };
        if file_name.trim_start_matches('*') == archive_name {
            return Ok(hash.to_ascii_lowercase());
        }
    }

    anyhow::bail!("checksum file did not contain an entry for {archive_name}")
}

fn parse_version_output(output: &str) -> Option<String> {
    output
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().last())
        .map(|value| value.trim_start_matches('v').to_string())
}

fn find_in_path(bin_name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(bin_name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_requested_version;
    use super::parse_checksum;
    use super::parse_version_output;

    #[test]
    fn normalizes_requested_versions() {
        assert_eq!(
            normalize_requested_version(Some("0.1.1")).as_deref(),
            Some("v0.1.1")
        );
        assert_eq!(
            normalize_requested_version(Some("v0.1.1")).as_deref(),
            Some("v0.1.1")
        );
        assert_eq!(normalize_requested_version(None), None);
    }

    #[test]
    fn parses_checksum_entries() {
        let checksum = "abc123  codex-pool-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            parse_checksum(checksum, "codex-pool-aarch64-apple-darwin.tar.gz")
                .expect("checksum should parse"),
            "abc123"
        );
    }

    #[test]
    fn parses_version_output() {
        assert_eq!(
            parse_version_output("codex-pool 0.1.1\n").as_deref(),
            Some("0.1.1")
        );
    }
}
