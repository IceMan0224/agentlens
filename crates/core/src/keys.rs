//! SSH keys and known hosts under `~/.ssh`, for display. Only public material is read: a
//! private key file is recognised by its first line and never read further.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use sha2::{Digest, Sha256};

use crate::net::SshHost;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// File name, or the line number in `authorized_keys`.
    pub name: String,
    /// `ssh-ed25519`, `ssh-rsa` …; empty for a private key without its `.pub`.
    pub kind: String,
    pub comment: String,
    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fingerprint: Option<String>,
    pub has_private: bool,
    /// `~/.ssh/config` hosts whose `IdentityFile` names this key.
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
    /// As written in `known_hosts`; hashed entries cannot be read back.
    pub hosts: String,
    pub kinds: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshInventory {
    pub dir: Option<PathBuf>,
    /// Key pairs in the directory: what this machine can log in with.
    pub keys: Vec<KeyInfo>,
    /// `authorized_keys`: who can log in to this machine.
    pub authorized: Vec<KeyInfo>,
    pub known: Vec<KnownHost>,
}

pub fn ssh_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| PathBuf::from(home).join(".ssh"))
}

/// `SHA256:<unpadded base64>` of a base64 public key blob.
pub fn fingerprint(blob: &str) -> Option<String> {
    let bytes = STANDARD.decode(blob).ok()?;
    Some(format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(bytes))))
}

/// (type, blob, comment) of a public key line, skipping `authorized_keys` options before it.
fn parse_key_line(line: &str) -> Option<(String, String, String)> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words
        .iter()
        .position(|w| w.starts_with("ssh-") || w.starts_with("ecdsa-") || w.starts_with("sk-"))?;
    let blob = words.get(i + 1)?;
    Some((words[i].to_string(), blob.to_string(), words[i + 2..].join(" ")))
}

fn is_private_key(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    let _ = std::io::BufReader::new(file).read_line(&mut first);
    first.starts_with("-----BEGIN") && first.contains("PRIVATE KEY")
}

fn identity_users(config: &[SshHost], file: &str) -> Vec<String> {
    config
        .iter()
        .filter(|h| {
            h.identity_files
                .iter()
                .any(|f| f.rsplit(['/', '\\']).next() == Some(file))
        })
        .flat_map(|h| h.patterns.iter().cloned())
        .collect()
}

pub fn load(dir: &Path, config: &[SshHost]) -> SshInventory {
    let mut out = SshInventory {
        dir: Some(dir.to_path_buf()),
        ..SshInventory::default()
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in &names {
        let path = dir.join(name);
        if let Some(stem) = name.strip_suffix(".pub") {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let Some((kind, blob, comment)) = text.lines().find_map(parse_key_line) else {
                continue;
            };
            out.keys.push(KeyInfo {
                name: stem.to_string(),
                kind,
                comment,
                fingerprint: fingerprint(&blob),
                has_private: names.iter().any(|n| n == stem),
                used_by: identity_users(config, stem),
            });
        } else if !names.contains(&format!("{name}.pub")) && is_private_key(&path) {
            out.keys.push(KeyInfo {
                name: name.clone(),
                kind: String::new(),
                comment: String::new(),
                fingerprint: None,
                has_private: true,
                used_by: identity_users(config, name),
            });
        }
    }

    let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap_or_default();
    for (i, line) in read("authorized_keys").lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((kind, blob, comment)) = parse_key_line(line) {
            out.authorized.push(KeyInfo {
                name: format!("#{}", i + 1),
                kind,
                comment,
                fingerprint: fingerprint(&blob),
                has_private: false,
                used_by: Vec::new(),
            });
        }
    }

    for line in read("known_hosts").lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `@cert-authority` / `@revoked` markers come first.
        let mut words = line.split_whitespace().skip_while(|w| w.starts_with('@'));
        let (Some(hosts), Some(kind)) = (words.next(), words.next()) else {
            continue;
        };
        let hosts = if hosts.starts_with("|1|") {
            "(hashed)".to_string()
        } else {
            hosts.to_string()
        };
        match out.known.iter_mut().find(|k| k.hosts == hosts && hosts != "(hashed)") {
            Some(k) => k.kinds.push(kind.to_string()),
            None => out.known.push(KnownHost {
                hosts,
                kinds: vec![kind.to_string()],
            }),
        }
    }
    out
}

/// The current user's `~/.ssh`, or nothing when there is no home directory.
pub fn load_default(config: &[SshHost]) -> SshInventory {
    ssh_dir().map(|dir| load(&dir, config)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_ssh_keygen() {
        // `ssh-keygen -lf` of this key prints the fingerprint below.
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIPXGXb4cZNjy58NTVnU7BOMOJ7DxpMIPciui/loj1N2n";
        assert_eq!(
            fingerprint(blob).as_deref(),
            Some("SHA256:1UD3SlGMoOhWpSXQRO0E+iaf9IjsPeOvomi/BzR5AG0")
        );
        assert_eq!(fingerprint("not base64!"), None);
    }

    #[test]
    fn lists_keys_known_hosts_and_authorized_keys_without_reading_private_keys() {
        let dir = std::env::temp_dir().join(format!("agentlens-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIPXGXb4cZNjy58NTVnU7BOMOJ7DxpMIPciui/loj1N2n";
        std::fs::write(dir.join("id_mac"), "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n").unwrap();
        std::fs::write(dir.join("id_mac.pub"), format!("ssh-ed25519 {blob} me@pc\n")).unwrap();
        std::fs::write(dir.join("old_key"), "-----BEGIN RSA PRIVATE KEY-----\nsecret\n").unwrap();
        std::fs::write(dir.join("config"), "Host mac\n  IdentityFile ~/.ssh/id_mac\n").unwrap();
        std::fs::write(
            dir.join("known_hosts"),
            format!("10.0.0.2 ssh-ed25519 {blob}\n10.0.0.2 ssh-rsa AAAA\n|1|abc= ssh-rsa AAAA\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("authorized_keys"),
            format!("# comment\nno-pty,from=\"10.*\" ssh-ed25519 {blob} laptop\n"),
        )
        .unwrap();
        let config = crate::net::parse_ssh_config(&std::fs::read_to_string(dir.join("config")).unwrap());
        let inv = load(&dir, &config);
        std::fs::remove_dir_all(&dir).unwrap();

        let names: Vec<&str> = inv.keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["id_mac", "old_key"], "config and known_hosts are not keys");
        let mac = &inv.keys[0];
        assert_eq!((mac.kind.as_str(), mac.comment.as_str()), ("ssh-ed25519", "me@pc"));
        assert!(mac.has_private && mac.fingerprint.is_some());
        assert_eq!(mac.used_by, ["mac"]);
        assert!(inv.keys[1].has_private && inv.keys[1].kind.is_empty());
        assert_eq!(inv.known.len(), 2);
        assert_eq!(inv.known[0].kinds, ["ssh-ed25519", "ssh-rsa"]);
        assert_eq!(inv.known[1].hosts, "(hashed)");
        assert_eq!(inv.authorized.len(), 1);
        assert_eq!(inv.authorized[0].comment, "laptop");
        assert_eq!(inv.authorized[0].name, "#2");
    }
}
