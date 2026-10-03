//! The viewer password: one value for the page link and for VNC.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use des::{
    Des,
    cipher::{BlockCipherEncrypt, KeyInit},
};
use uuid::Uuid;

/// VNC password authentication only looks at the first 8 characters.
const KEY_LEN: usize = 8;
const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
/// Fixed key `Xvnc` uses to obfuscate the password in its password file.
const FILE_KEY: [u8; 8] = [23, 82, 107, 6, 35, 78, 88, 7];

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

/// Contents of an `Xvnc` password file for `key`: the key padded to 8 bytes and DES-encrypted with the fixed key.
///
/// VNC bit-reverses every byte of a DES key.
fn vnc_password_file(key: &str) -> [u8; 8] {
    let mut block = [0u8; 8];
    for (slot, byte) in block.iter_mut().zip(key.bytes()) {
        *slot = byte;
    }
    let cipher = Des::new(&FILE_KEY.map(u8::reverse_bits).into());
    let mut out = block.into();
    cipher.encrypt_block(&mut out);
    out.into()
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
    let stored = std::fs::read_to_string(&key_path)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| is_valid(text));
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
    fn the_password_file_is_the_des_obfuscation_xvnc_reads() {
        assert_eq!(
            vnc_password_file("abcd2345"),
            [255, 232, 190, 74, 23, 18, 52, 125],
            "bytes from the same encoding that Xvnc accepted in the Docker test"
        );
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
