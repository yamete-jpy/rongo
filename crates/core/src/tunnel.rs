//! SSH local port forwarding: listens on 127.0.0.1 and forwards each accepted
//! connection through the SSH server to the MongoDB host.
//!
//! The server's host key must already be in `~/.ssh/known_hosts` (connect once
//! with `ssh` to add it); unknown or changed keys are refused.
//!
//! Like `ssh -L`, the local port accepts any process on this machine, not just
//! the driver: the MongoDB driver cannot use a custom stream, and macOS gives no
//! cheap way to identify the connecting process. MongoDB authentication still
//! applies to whoever connects, the port is random, and the listener closes when
//! the connection is closed (dropping `Tunnel` aborts the accept loop).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::error::{CoreError, CoreResult};
use crate::profile::{SshAuth, SshConfig};

pub struct Tunnel {
    local_port: u16,
    task: JoinHandle<()>,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub async fn start(ssh: &SshConfig, secret: Option<String>, target_host: String, target_port: u16) -> CoreResult<Tunnel> {
        let err = |e: &dyn std::fmt::Display| CoreError::Tunnel(e.to_string());
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            inactivity_timeout: None,
            ..Default::default()
        });
        let rejection = Arc::new(Mutex::new(None));
        let handler = HostKeyCheck { host: ssh.host.clone(), port: ssh.port, rejection: rejection.clone() };
        let connect = client::connect(config, (ssh.host.as_str(), ssh.port), handler);
        let mut session = match tokio::time::timeout(Duration::from_secs(15), connect).await {
            Err(_) => return Err(CoreError::Tunnel(format!("timed out connecting to {}:{}", ssh.host, ssh.port))),
            Ok(Err(e)) => {
                let why = rejection.lock().ok().and_then(|r| r.clone());
                return Err(CoreError::Tunnel(why.unwrap_or_else(|| e.to_string())));
            }
            Ok(Ok(s)) => s,
        };

        let auth = match &ssh.auth {
            SshAuth::Password => {
                let pw = secret.ok_or_else(|| CoreError::Tunnel("no SSH password saved".into()))?;
                session.authenticate_password(&ssh.user, pw).await.map_err(|e| err(&e))?
            }
            SshAuth::Key { path } => {
                let path = expand_home(path);
                let key = russh::keys::load_secret_key(&path, secret.as_deref())
                    .map_err(|e| CoreError::Tunnel(format!("{}: {e}", path.display())))?;
                let hash = session.best_supported_rsa_hash().await.map_err(|e| err(&e))?.flatten();
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
                session.authenticate_publickey(&ssh.user, key).await.map_err(|e| err(&e))?
            }
        };
        if !auth.success() {
            return Err(CoreError::Tunnel(format!("SSH authentication failed for {}@{}", ssh.user, ssh.host)));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| err(&e))?;
        let local_port = listener.local_addr().map_err(|e| err(&e))?.port();
        let session = Arc::new(session);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, peer)) = listener.accept().await {
                let session = session.clone();
                let host = target_host.clone();
                tokio::spawn(async move {
                    let channel = match session
                        .channel_open_direct_tcpip(host, u32::from(target_port), peer.ip().to_string(), u32::from(peer.port()))
                        .await
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                });
            }
        });
        Ok(Tunnel { local_port, task })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn expand_home(p: &std::path::Path) -> std::path::PathBuf {
    match (p.strip_prefix("~"), directories::BaseDirs::new()) {
        (Ok(rest), Some(dirs)) => dirs.home_dir().join(rest),
        _ => p.to_path_buf(),
    }
}

struct HostKeyCheck {
    host: String,
    port: u16,
    rejection: Arc<Mutex<Option<String>>>,
}

impl HostKeyCheck {
    fn reject(&self, why: String) -> Result<bool, russh::Error> {
        if let Ok(mut r) = self.rejection.lock() {
            *r = Some(why);
        }
        Ok(false)
    }
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return self.reject("SSH host certificates are not supported".into());
        };
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => self.reject(format!(
                "{} is not in ~/.ssh/known_hosts. Run `ssh -p {} {}` once to verify and add its key.",
                self.host, self.port, self.host
            )),
            Err(russh::keys::Error::KeyChanged { line }) => self.reject(format!(
                "the host key of {} does not match ~/.ssh/known_hosts line {line}. This could be an attack; refusing to connect.",
                self.host
            )),
            Err(e) => self.reject(format!("could not check ~/.ssh/known_hosts: {e}")),
        }
    }
}
