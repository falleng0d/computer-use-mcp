//! Helpers for the Docker tests in the parent module.

use bollard::{Docker, query_parameters::RemoveContainerOptionsBuilder};

pub(super) struct Cleanup {
    pub(super) docker: Docker,
    pub(super) name: String,
    pub(super) volume: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let docker = self.docker.clone();
        let (name, volume) = (self.name.clone(), self.volume.clone());
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime can always be built");
            runtime.block_on(async {
                let options = RemoveContainerOptionsBuilder::new().force(true).build();
                let _ = docker.remove_container(&name, Some(options)).await;
                let _ = docker
                    .remove_volume(
                        &volume,
                        None::<bollard::query_parameters::RemoveVolumeOptions>,
                    )
                    .await;
            });
        })
        .join();
    }
}

/// A VNC connection after the version and security type exchange.
pub(super) struct Rfb {
    pub(super) stream: tokio::net::TcpStream,
    pub(super) security_types: Vec<u8>,
}

/// DES response to a VNC authentication challenge. VNC bit-reverses every key byte.
pub(super) fn vnc_response(password: &str, challenge: [u8; 16]) -> [u8; 16] {
    use des::{
        Des,
        cipher::{BlockCipherEncrypt, KeyInit},
    };
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new(&key.into());
    let mut out = [0u8; 16];
    for (index, chunk) in challenge.chunks(8).enumerate() {
        let mut block = <[u8; 8]>::try_from(chunk).unwrap().into();
        cipher.encrypt_block(&mut block);
        out[index * 8..index * 8 + 8].copy_from_slice(&block);
    }
    out
}

pub(super) async fn rfb_connect(port: u16) -> Rfb {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await.unwrap();
    assert!(version.starts_with(b"RFB 003."));
    stream
        .write_all(
            b"RFB 003.008
",
        )
        .await
        .unwrap();
    let mut count = [0u8; 1];
    stream.read_exact(&mut count).await.unwrap();
    let mut security_types = vec![0u8; usize::from(count[0])];
    stream.read_exact(&mut security_types).await.unwrap();
    Rfb {
        stream,
        security_types,
    }
}

/// Authenticates with VNC password auth and returns the security result (0 means accepted).
pub(super) async fn rfb_authenticate(rfb: &mut Rfb, password: &str) -> u32 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    rfb.stream.write_all(&[2]).await.unwrap();
    let mut challenge = [0u8; 16];
    rfb.stream.read_exact(&mut challenge).await.unwrap();
    let response = vnc_response(password, challenge);
    rfb.stream.write_all(&response).await.unwrap();
    let mut result = [0u8; 4];
    rfb.stream.read_exact(&mut result).await.unwrap();
    u32::from_be_bytes(result)
}

/// Sends `ClientInit` and returns the desktop size from `ServerInit`.
pub(super) async fn rfb_size(rfb: &mut Rfb) -> (u16, u16) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    rfb.stream.write_all(&[1]).await.unwrap();
    let mut init = [0u8; 4];
    rfb.stream.read_exact(&mut init).await.unwrap();
    (
        u16::from_be_bytes([init[0], init[1]]),
        u16::from_be_bytes([init[2], init[3]]),
    )
}

/// Viewers the page lists for the first session with a screen, read from its event stream.
pub(super) async fn viewers_on_page(port_base: u16, key: &str) -> usize {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port_base))
        .await
        .unwrap();
    let request = format!(
        "GET /events HTTP/1.1
Host: 127.0.0.1:{port_base}
X-Viewer-Key: {key}

"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    let mut chunk = [0u8; 2048];
    let data = loop {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "the event stream ended");
        seen.extend_from_slice(&chunk[..count]);
        let text = String::from_utf8_lossy(&seen).into_owned();
        if let Some(line) = text.lines().find(|line| line.starts_with("data: ")) {
            break line["data: ".len()..].to_owned();
        }
    };
    let sessions: serde_json::Value = serde_json::from_str(&data).unwrap();
    usize::try_from(sessions[0]["viewers"].as_u64().unwrap()).unwrap()
}

pub(super) fn docker_cli(args: &[&str]) -> String {
    let output = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("the docker CLI runs");
    assert!(
        output.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Removes the extra image tags a test made.
pub(super) struct Tags(pub(super) Vec<String>);

impl Drop for Tags {
    fn drop(&mut self) {
        for tag in &self.0 {
            let _ = std::process::Command::new("docker")
                .args(["rmi", "--force", tag])
                .output();
        }
    }
}
