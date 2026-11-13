//! Client certificate, extra trusted CAs and host verification from the env files'
//! `SSLConfiguration` (JetBrains' fields plus our `caCertificate`), handed over as `$ssl` with
//! absolute paths. rustls reads PEM; DER is wrapped into PEM here.
// ponytail: P12/PFX and passphrase-protected keys need native-tls or a PKCS#12 crate; until
// someone needs them, the error says how to convert with openssl.

use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;
use http_env::Variable;
use serde_json::Value;

use crate::Error;

#[derive(Default)]
pub(crate) struct Tls {
    pub identity: Option<reqwest::Identity>,
    pub roots: Vec<reqwest::Certificate>,
    /// `verifyHostCertificate: false` turns checks off for the whole environment.
    pub verify: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    Pem,
    Der,
    P12,
}

fn bad(m: String) -> Error {
    Error::BadRequest(format!("SSLConfiguration: {m}"))
}

/// `"a.pem"` or `{"path": "a.crt", "format": "PEM"}`; the format from the extension as in
/// JetBrains when not given.
fn file(field: &str, v: &Value) -> Result<(PathBuf, Format), Error> {
    let (path, format) = match v {
        Value::String(p) => (p.as_str(), None),
        Value::Object(o) => (o.get("path").and_then(Value::as_str).unwrap_or(""), o.get("format").and_then(Value::as_str)),
        _ => return Err(bad(format!("\"{field}\" must be a path or {{\"path\", \"format\"}}"))),
    };
    if path.is_empty() {
        return Err(bad(format!("\"{field}\" has no path")));
    }
    let path = PathBuf::from(path);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let format = match (format.map(str::to_ascii_uppercase).as_deref(), ext.as_str()) {
        (Some("PEM"), _) | (None, "pem" | "crt" | "key") => Format::Pem,
        (Some("DER"), _) | (None, "der" | "cer") => Format::Der,
        (Some("P12"), _) | (None, "p12" | "pkcs12" | "pfx") => Format::P12,
        (Some(f), _) => return Err(bad(format!("\"{field}\": unknown format \"{f}\" (PEM, DER, P12)"))),
        (None, _) => return Err(bad(format!("\"{field}\": can't tell the format of {}; add \"format\": \"PEM\" or \"DER\"", path.display()))),
    };
    Ok((path, format))
}

fn read(path: &PathBuf) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::File(path.clone(), e))
}

fn pem(label: &str, der: &[u8]) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).expect("base64 is ASCII")).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n")).into_bytes()
}

/// Certificate or key file as PEM.
fn as_pem(field: &str, v: &Value, label: &str) -> Result<Vec<u8>, Error> {
    let (path, format) = file(field, v)?;
    match format {
        Format::Pem => read(&path),
        Format::Der => Ok(pem(label, &read(&path)?)),
        Format::P12 => Err(bad(format!(
            "{} is PKCS#12, which isn't supported yet; convert it to PEM: \
             openssl pkcs12 -in {0} -out client.pem -nodes",
            path.display()
        ))),
    }
}

pub(crate) fn load(env: &HashMap<String, Variable>) -> Result<Tls, Error> {
    let Some(var) = env.get("$ssl") else { return Ok(Tls { verify: true, ..Default::default() }) };
    let cfg: Value = serde_json::from_str(&var.value).unwrap_or_default();
    let mut tls = Tls { verify: cfg["verifyHostCertificate"].as_bool().unwrap_or(true), ..Default::default() };

    if let Some(cert) = cfg.get("clientCertificate") {
        if cfg["hasCertificatePassphrase"].as_bool() == Some(true) {
            return Err(bad("keys with a passphrase aren't supported yet; decrypt it once: \
                 openssl pkey -in key.pem -out key-plain.pem (and keep that file private)"
                .into()));
        }
        // One PEM buffer: the certificate (chain), then the key when it's in its own file.
        let mut buf = as_pem("clientCertificate", cert, "CERTIFICATE")?;
        if let Some(key) = cfg.get("clientCertificateKey") {
            buf.push(b'\n');
            buf.extend(as_pem("clientCertificateKey", key, "PRIVATE KEY")?);
        }
        if String::from_utf8_lossy(&buf).contains("ENCRYPTED") {
            return Err(bad("the private key is encrypted; decrypt it once: openssl pkey -in key.pem -out key-plain.pem".into()));
        }
        let id = reqwest::Identity::from_pem(&buf).map_err(|e| bad(format!("client certificate or key not usable: {e}")))?;
        tls.identity = Some(id);
    } else if cfg.get("clientCertificateKey").is_some() {
        return Err(bad("\"clientCertificateKey\" without \"clientCertificate\"".into()));
    }

    let cas = match cfg.get("caCertificate") {
        None => vec![],
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
    };
    for ca in &cas {
        let (path, format) = file("caCertificate", ca)?;
        let bytes = read(&path)?;
        let parsed = match format {
            Format::Pem => reqwest::Certificate::from_pem_bundle(&bytes),
            Format::Der => reqwest::Certificate::from_der(&bytes).map(|c| vec![c]),
            Format::P12 => return Err(bad(format!("{}: a CA must be PEM or DER", path.display()))),
        };
        let certs = parsed.map_err(|e| bad(format!("{}: {e}", path.display())))?;
        if certs.is_empty() {
            return Err(bad(format!("{}: no certificates in the file", path.display())));
        }
        tls.roots.extend(certs);
    }
    Ok(tls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_from_extension_or_field() {
        let f = |v: Value| file("x", &v).map(|(_, f)| f).map_err(|e| e.to_string());
        assert_eq!(f("a.crt".into()), Ok(Format::Pem));
        assert_eq!(f("a.CER".into()), Ok(Format::Der));
        assert_eq!(f("a.pfx".into()), Ok(Format::P12));
        assert_eq!(f(serde_json::json!({"path": "a.bin", "format": "der"})), Ok(Format::Der));
        assert!(f("a.bin".into()).unwrap_err().contains("add \"format\""));
        assert!(pem("CERTIFICATE", &[0u8; 100]).starts_with(b"-----BEGIN CERTIFICATE-----\n"));
    }

    /// Fixtures in tests/certs (openssl; a test CA, a server and a client signed by it).
    #[test]
    fn load_from_ssl_configuration() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/certs");
        let p = |f: &str| dir.join(f).to_string_lossy().into_owned();
        let load_with = |cfg: Value| {
            let var = Variable { value: cfg.to_string(), source: dir.clone(), secret: false };
            load(&HashMap::from([("$ssl".to_string(), var)])).map_err(|e| e.to_string())
        };
        let pem = load_with(serde_json::json!({"clientCertificate": p("client.pem"), "clientCertificateKey": p("client.key"), "caCertificate": p("ca.pem")})).unwrap();
        assert!(pem.identity.is_some() && pem.verify);
        assert_eq!(pem.roots.len(), 1);
        let der = load_with(serde_json::json!({"clientCertificate": p("client.der"), "clientCertificateKey": {"path": p("client-key.der"), "format": "DER"}}));
        assert!(der.unwrap().identity.is_some());
        assert!(!load_with(serde_json::json!({"verifyHostCertificate": false})).unwrap().verify);

        let err = |cfg: Value| load_with(cfg).err().unwrap_or_default();
        assert!(err(serde_json::json!({"clientCertificate": p("client.pem"), "clientCertificateKey": p("client-encrypted.key")})).contains("encrypted"));
        assert!(err(serde_json::json!({"clientCertificate": p("c.p12")})).contains("openssl pkcs12"));
        assert!(err(serde_json::json!({"clientCertificate": p("client.pem"), "hasCertificatePassphrase": true})).contains("passphrase"));
        assert!(err(serde_json::json!({"caCertificate": p("missing.pem")})).contains("missing.pem"));
        assert!(load(&HashMap::new()).unwrap().verify);
    }
}
