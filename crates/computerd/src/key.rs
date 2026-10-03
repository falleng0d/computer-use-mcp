//! The viewer password: one value for the page link and for VNC.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use computer_protocol::vnc_password_file;
use tracing::warn;
use uuid::Uuid;

/// VNC password authentication only looks at the first 8 characters.
const KEY_LEN: usize = 8;
const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

const CONFIG_DIR: &str = ".config/computerd";
const KEY_FILE: &str = "viewer-key";
const VNC_FILE: &str = "vncpasswd";

/// Builds a key from 8 random bytes, 5 bits each, so every character is equally likely.
fn from_random(bytes: [u8; KEY_LEN]) -> String {
    bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(byte & 31)]))
        .collect()
}

fn fresh() -> String {
    let random = *Uuid::new_v4().as_bytes();
    from_random([
        random[0], random[1], random[2], random[3], random[4], random[5], random[7], random[9],
    ])
}

fn is_valid(key: &str) -> bool {
    key.len() == KEY_LEN && key.bytes().all(|byte| ALPHABET.contains(&byte))
}

fn config_dir(home: &Path) -> PathBuf {
    home.join(CONFIG_DIR)
}

/// Where `Xvnc` reads the obfuscated password.
pub fn vnc_password_path(home: &Path) -> PathBuf {
    config_dir(home).join(VNC_FILE)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Loads the key from home, or makes and stores one on a fresh home, then writes the `Xvnc` password file.
pub fn ensure(home: &Path) -> Result<String> {
    let dir = config_dir(home);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let key_path = dir.join(KEY_FILE);
    let stored = match std::fs::read_to_string(&key_path) {
        Ok(text) if is_valid(text.trim()) => Some(text.trim().to_owned()),
        Ok(_) => {
            warn!(path = %key_path.display(), "the viewer key file is damaged, making a new key");
            None
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            warn!(path = %key_path.display(), %error, "the viewer key file is unreadable, making a new key");
            None
        }
    };
    let key = if let Some(key) = stored {
        key
    } else {
        let key = fresh();
        write_private(&key_path, key.as_bytes())?;
        key
    };
    write_private(&vnc_password_path(home), &vnc_password_file(&key))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_8_characters_from_the_safe_alphabet() {
        for _ in 0..50 {
            assert!(is_valid(&fresh()));
        }
        assert_ne!(fresh(), fresh());
        assert_eq!(from_random([0, 1, 31, 32, 33, 255, 128, 7]), "ab9ab9ah");
    }

    #[test]
    fn a_stored_key_is_reused_and_a_damaged_one_is_replaced() {
        let home = std::env::temp_dir().join(format!("computerd-key-{}", Uuid::new_v4().simple()));
        let first = ensure(&home).unwrap();
        assert_eq!(ensure(&home).unwrap(), first);
        let key_file = config_dir(&home).join(KEY_FILE);
        assert_eq!(std::fs::read_to_string(&key_file).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&key_file), 0o600);
            assert_eq!(mode(&vnc_password_path(&home)), 0o600);
        }
        std::fs::write(&key_file, "short").unwrap();
        let replaced = ensure(&home).unwrap();
        assert!(is_valid(&replaced));
        assert_eq!(
            std::fs::read(vnc_password_path(&home)).unwrap(),
            vnc_password_file(&replaced)
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}
