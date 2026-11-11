use anyhow::{Context, Result, bail};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// User settings from ~/.config/lanlink/config.toml. Every field is optional.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// Name other devices see. Defaults to the hostname.
    pub name: String,
    /// desktop, laptop, tv, phone or tablet.
    pub device_type: String,
    /// Where received files go.
    pub download_dir: PathBuf,
    /// Phones get mounted at <mount_dir>/<device name>.
    pub mount_dir: PathBuf,
    /// TCP port to listen on (KDE Connect uses 1716-1764).
    pub tcp_port: u16,
    /// Broadcast and listen for other devices on UDP 1716.
    pub discovery: bool,
    /// Multiplier for pointer movement (both directions).
    pub pointer_speed: f64,
    /// Multiplier for scrolling (both directions).
    pub scroll_speed: f64,
    /// Flip scroll direction for received scroll events.
    pub invert_scroll: bool,
    /// Natural (touchscreen-style) two-finger scrolling when controlling another device.
    pub natural_scroll: bool,
    /// Colour of the "input captured" screen border, #rrggbb.
    pub border_color: String,
    /// Width of that border in pixels.
    pub border_width: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Config {
            name: hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "lanlink".into()),
            device_type: "desktop".into(),
            download_dir: dirs::download_dir().unwrap_or_else(|| home.join("Downloads")),
            mount_dir: dirs::runtime_dir().map(|d| d.join("lanlink")).unwrap_or_else(|| home.join("Phone")),
            tcp_port: 1716,
            discovery: true,
            pointer_speed: 1.0,
            scroll_speed: 1.0,
            invert_scroll: false,
            natural_scroll: true,
            border_color: "#ff8800".into(),
            border_width: 4,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("lanlink")
}

pub fn trusted_dir() -> PathBuf {
    config_dir().join("trusted")
}

pub fn socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lanlink.sock")
}

fn expand(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// KDE Connect rejects names with these characters, and longer than 32 chars.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !"\"',;:.!?()[]<>".contains(*c))
        .take(32)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() { "lanlink".into() } else { s }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("config.toml");
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        cfg.download_dir = expand(&cfg.download_dir);
        cfg.mount_dir = expand(&cfg.mount_dir);
        cfg.name = sanitize_name(&cfg.name);
        if !["desktop", "laptop", "tv", "phone", "tablet"].contains(&cfg.device_type.as_str()) {
            bail!("device_type must be desktop, laptop, tv, phone or tablet");
        }
        Ok(cfg)
    }

    pub fn border_rgb(&self) -> (u8, u8, u8) {
        let h = self.border_color.trim_start_matches('#');
        let v = u32::from_str_radix(h, 16).unwrap_or(0xff8800);
        ((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

/// This device's identity: an id plus the self-signed cert other devices pin.
pub struct Identity {
    pub device_id: String,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn load_or_create() -> Result<Self> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let (id_p, cert_p, key_p) = (dir.join("device_id"), dir.join("cert.pem"), dir.join("key.pem"));
        if !(id_p.exists() && cert_p.exists() && key_p.exists()) {
            create(&id_p, &cert_p, &key_p)?;
        }
        let device_id = std::fs::read_to_string(&id_p)?.trim().to_string();
        let cert = CertificateDer::from_pem_file(&cert_p).context("reading cert.pem")?;
        let key = PrivateKeyDer::from_pem_file(&key_p).context("reading key.pem")?;
        Ok(Identity { device_id, cert, key })
    }
}

fn create(id_p: &Path, cert_p: &Path, key_p: &Path) -> Result<()> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
    // Protocol v8 wants 32-38 chars of [a-zA-Z0-9_], and the cert CN must match it.
    let device_id = uuid::Uuid::new_v4().simple().to_string();
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, device_id.as_str());
    dn.push(DnType::OrganizationName, "KDE");
    dn.push(DnType::OrganizationalUnitName, "KDE Connect");
    params.distinguished_name = dn;
    let year = 1970 + (crate::protocol::now_ms() / 1000 / 31_556_952) as i32;
    params.not_before = rcgen::date_time_ymd(year - 1, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + 10, 1, 1);
    let cert = params.self_signed(&key)?;
    std::fs::write(id_p, &device_id)?;
    std::fs::write(cert_p, cert.pem())?;
    write_private(key_p, key.serialize_pem().as_bytes())?;
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}
