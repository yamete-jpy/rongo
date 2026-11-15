//! Connection strings: the password taken out of a typed URI (it goes to
//! the vault), a URI fit to show or print, and the string Export Connection
//! gives away.

use crate::profile::{Auth, ConnectionProfile, Target};

/// Replace the password in a URI with `****`.
pub fn redact_uri(uri: &str) -> String {
    match uri_userinfo(uri) {
        Some((scheme, user, Some(_), rest)) => format!("{scheme}://{user}:****{rest}"),
        _ => uri.to_string(),
    }
}

/// A URI fit to print (a `Debug`): its password and the secrets Export
/// Connection leaves out shown as `****` (the TLS file options, a key
/// file's password among them, and an AWS session token).
pub(crate) fn redact_uri_secrets(uri: &str) -> String {
    let uri = redact_uri(uri);
    let Some((head, query)) = uri.split_once('?') else { return uri };
    let mut u = UriParts { scheme: String::new(), user: None, hosts: String::new(), db: String::new(), options: query.split('&').filter(|o| !o.is_empty()).map(str::to_string).collect() };
    for key in TLS_FILE_OPTIONS {
        if u.options.iter().any(|o| option_is(o, key)) {
            u.set(key, Some("****".into()));
        }
    }
    let before = u.options.clone();
    u.drop_session_token();
    if u.options != before {
        u.options.push("authMechanismProperties=AWS_SESSION_TOKEN:****".into());
    }
    format!("{head}?{}", u.options.join("&"))
}

/// Split `scheme://user:pass@rest` into (`scheme://user@rest`, `pass`).
/// The password comes back decoded (`s3cr%40t` → `s3cr@t`): it leaves the
/// URI, and is given to the driver as is. Returns `None` when the URI
/// carries no password.
pub fn split_uri_password(uri: &str) -> Option<(String, String)> {
    match uri_userinfo(uri)? {
        (scheme, user, Some(pass), rest) => Some((format!("{scheme}://{user}{rest}"), percent_decode(pass))),
        _ => None,
    }
}

/// Decode `%XX` escapes. Text that isn't valid escaping is kept as typed.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// `(scheme, user, password, "@hosts/…")` of a URI with userinfo. The
/// authority ends at the first `/` or `?`: an `@` in the options is not one.
pub(crate) fn uri_userinfo(uri: &str) -> Option<(&str, &str, Option<&str>, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let at = rest[..authority_end].rfind('@')?;
    let userinfo = &rest[..at];
    Some(match userinfo.split_once(':') {
        Some((user, pass)) => (scheme, user, Some(pass), &rest[at..]),
        None => (scheme, userinfo, None, &rest[at..]),
    })
}

/// A connection string for this profile to give to someone else (Export
/// Connection): hosts, replica set, direct connection, default database,
/// auth database and mechanism, TLS on and invalid certificates allowed,
/// with the Authentication and TLS tabs applied as when connecting. With
/// `user` the username goes in and, when given, `password`; without it no
/// credentials or auth options do (a driver refuses authSource without a
/// username). SSH and TLS file paths stay out: they belong to this computer.
pub fn export_uri(p: &ConnectionProfile, user: bool, password: Option<&str>) -> String {
    let mut u = match &p.target {
        Target::Uri { uri } => UriParts::parse(uri),
        Target::Manual { hosts, replica_set, direct } => {
            let mut u = UriParts { scheme: "mongodb".into(), user: None, hosts: hosts.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).collect::<Vec<_>>().join(","), db: String::new(), options: Vec::new() };
            if let Some(rs) = replica_set.as_deref().filter(|rs| !rs.is_empty()) {
                u.set("replicaSet", Some(uri_encode(rs)));
            }
            if *direct {
                u.set("directConnection", Some("true".into()));
            }
            u
        }
    };
    if u.db.is_empty()
        && let Some(db) = &p.default_db
    {
        u.db = uri_encode(db);
    }
    if let Auth::Password { user, auth_db, mechanism } = &p.auth {
        u.user = Some(uri_encode(user));
        u.set("authSource", Some(uri_encode(if auth_db.is_empty() { "admin" } else { auth_db })));
        u.set("authMechanism", mechanism.as_deref().filter(|m| !m.is_empty()).map(uri_encode));
    }
    if p.tls.enabled {
        u.set("tls", Some("true".into()));
        if p.tls.allow_invalid_certs {
            u.set("tlsAllowInvalidCertificates", Some("true".into()));
        }
    }
    // Files on this computer, and secrets other than the password given, never travel.
    for key in TLS_FILE_OPTIONS {
        u.set(key, None);
    }
    u.drop_session_token();
    if !user {
        u.user = None;
        for key in ["authSource", "authMechanism", "authMechanismProperties"] {
            u.set(key, None);
        }
    }
    let creds = match (&u.user, password) {
        (Some(name), Some(pw)) => format!("{name}:{}@", uri_encode(pw)),
        (Some(name), None) => format!("{name}@"),
        (None, _) => String::new(),
    };
    let mut out = format!("{}://{creds}{}", u.scheme, u.hosts);
    if !u.db.is_empty() || !u.options.is_empty() {
        out = format!("{out}/{}", u.db);
    }
    if !u.options.is_empty() {
        out = format!("{out}?{}", u.options.join("&"));
    }
    out
}

/// `scheme://user[:password]@hosts/db?options`, split as written (still
/// percent-encoded); the password is dropped.
struct UriParts {
    scheme: String,
    user: Option<String>,
    hosts: String,
    db: String,
    /// `key=value`, in order.
    options: Vec<String>,
}

impl UriParts {
    fn parse(uri: &str) -> Self {
        let (scheme, rest) = uri.split_once("://").unwrap_or(("mongodb", uri));
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, db) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, hosts) = match authority.rfind('@') {
            Some(at) => (authority[..at].split(':').next().map(str::to_string), &authority[at + 1..]),
            None => (None, authority),
        };
        let options = query.split('&').filter(|o| !o.is_empty()).map(str::to_string).collect();
        Self { scheme: scheme.into(), user, hosts: hosts.into(), db: db.into(), options }
    }

    /// Drop `key` (option names ignore case), then add `key=value` if given.
    fn set(&mut self, key: &str, value: Option<String>) {
        self.options.retain(|o| !option_is(o, key));
        if let Some(v) = value {
            self.options.push(format!("{key}={v}"));
        }
    }

    /// Take `AWS_SESSION_TOKEN:…` out of authMechanismProperties, and the
    /// option itself once nothing is left in it.
    fn drop_session_token(&mut self) {
        self.options.retain_mut(|o| {
            let Some(value) = o.split_once('=').map(|(_, v)| percent_decode(v)).filter(|_| option_is(o, "authMechanismProperties")) else { return true };
            let token = |p: &&str| p.split(':').next().unwrap_or("").eq_ignore_ascii_case("AWS_SESSION_TOKEN");
            if !value.split(',').any(|p| token(&p)) {
                return true;
            }
            let kept: Vec<String> = value
                .split(',')
                .filter(|p| !token(p))
                .map(|p| match p.split_once(':') {
                    Some((k, v)) => format!("{}:{}", uri_encode(k), uri_encode(v)),
                    None => uri_encode(p),
                })
                .collect();
            *o = format!("authMechanismProperties={}", kept.join(","));
            !kept.is_empty()
        });
    }
}

/// URI options naming TLS files on this computer (and a key file's password),
/// current names and the older `ssl` ones.
const TLS_FILE_OPTIONS: [&str; 6] = ["tlsCAFile", "tlsCertificateKeyFile", "tlsCertificateKeyFilePassword", "sslCAFile", "sslPEMKeyFile", "sslPEMKeyPassword"];

/// Whether `option` (`key=value`) is `key`; option names ignore case.
fn option_is(option: &str, key: &str) -> bool {
    option.split('=').next().unwrap_or("").eq_ignore_ascii_case(key)
}

/// Whether the profile uses TLS certificate files (the TLS tab, or options
/// in its URI), which an export leaves out.
pub fn has_tls_files(p: &ConnectionProfile) -> bool {
    let in_uri = match &p.target {
        Target::Uri { uri } => UriParts::parse(uri).options.iter().any(|o| TLS_FILE_OPTIONS.iter().any(|k| option_is(o, k))),
        Target::Manual { .. } => false,
    };
    in_uri || p.tls.ca_file.is_some() || p.tls.cert_key_file.is_some()
}

/// Percent-encode everything but RFC 3986's unreserved characters.
fn uri_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{SshAuth, SshConfig, TlsConfig};

    #[test]
    fn splits_password_out_of_uri() {
        let (clean, pass) = split_uri_password("mongodb://alice:s3cr%40t@db1:27017/?replicaSet=rs0").unwrap();
        assert_eq!(clean, "mongodb://alice@db1:27017/?replicaSet=rs0");
        assert_eq!(pass, "s3cr@t", "decoded, as the driver would read it");
        let (_, pass) = split_uri_password("mongodb://u:a%3Ab%2Fc%25d%E0%B8%81@h/").unwrap();
        assert_eq!(pass, "a:b/c%dก");
        let (_, pass) = split_uri_password("mongodb://u:100%sure@h/").unwrap();
        assert_eq!(pass, "100%sure", "not an escape: kept as typed");
        assert!(split_uri_password("mongodb://db1:27017").is_none());
        assert!(split_uri_password("mongodb://alice@db1:27017").is_none());
    }

    /// The userinfo ends where the hosts do: at the first `/` or `?`. An
    /// `@` in the options of a URI with no `/` before them is not a password.
    #[test]
    fn an_at_in_the_options_is_not_userinfo() {
        for uri in ["mongodb://h?x=a:b@c", "mongodb://h:27017?appName=a:b@c&w=1", "mongodb+srv://c.example.net?x=@"] {
            assert!(split_uri_password(uri).is_none(), "{uri}");
            assert_eq!(redact_uri(uri), uri);
            assert!(!uri_profile(uri).uses_password(), "{uri}");
        }
        let (clean, pass) = split_uri_password("mongodb://u:p@h?x=a:b@c").unwrap();
        assert_eq!((clean.as_str(), pass.as_str()), ("mongodb://u@h?x=a:b@c", "p"));
        assert_eq!(redact_uri("mongodb://u:p@h/db?x=a:b@c"), "mongodb://u:****@h/db?x=a:b@c");
    }

    #[test]
    fn redacts_uri() {
        assert_eq!(redact_uri("mongodb+srv://bob:pw@c.example.net/x"), "mongodb+srv://bob:****@c.example.net/x");
        assert_eq!(redact_uri("mongodb://h:1"), "mongodb://h:1");
    }

    fn manual(hosts: &[&str], replica_set: Option<&str>, direct: bool) -> ConnectionProfile {
        let mut p = ConnectionProfile::new("x");
        p.target = Target::Manual { hosts: hosts.iter().map(|h| h.to_string()).collect(), replica_set: replica_set.map(str::to_string), direct };
        p
    }

    fn password_auth(user: &str, auth_db: &str, mechanism: Option<&str>) -> Auth {
        Auth::Password { user: user.into(), auth_db: auth_db.into(), mechanism: mechanism.map(str::to_string) }
    }

    fn uri_profile(uri: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::new("x");
        p.target = Target::Uri { uri: uri.into() };
        p
    }

    /// Export Connection: hosts, replica set and direct connection go into
    /// the string, the username and auth options only when asked for.
    #[test]
    fn exports_a_manual_profile() {
        let mut p = manual(&["a.example.net:27017", "b.example.net:27018"], Some("rs0"), false);
        p.auth = password_auth("app", "admin", Some("SCRAM-SHA-256"));
        assert_eq!(export_uri(&p, true, None), "mongodb://app@a.example.net:27017,b.example.net:27018/?replicaSet=rs0&authSource=admin&authMechanism=SCRAM-SHA-256");
        assert_eq!(export_uri(&p, false, Some("pw")), "mongodb://a.example.net:27017,b.example.net:27018/?replicaSet=rs0", "no username: no password and no auth options (the driver refuses authSource without a user)");
        assert_eq!(export_uri(&manual(&["h:27017"], None, true), true, None), "mongodb://h:27017/?directConnection=true");
        assert_eq!(export_uri(&manual(&[" h:27017 ", ""], Some(""), false), true, Some("pw")), "mongodb://h:27017", "no user to go with the password");
    }

    /// The password goes in only with the username; both are percent-encoded
    /// so every character, Thai included, reads back exactly.
    #[tokio::test]
    async fn exported_credentials_read_back_exactly() {
        let mut p = manual(&["h:27017"], None, false);
        p.auth = password_auth("a@b:c", "admin", None);
        let uri = export_uri(&p, true, Some("p@ss:w/rd%?#[]"));
        assert_eq!(uri, "mongodb://a%40b%3Ac:p%40ss%3Aw%2Frd%25%3F%23%5B%5D@h:27017/?authSource=admin");
        for pw in ["p@ss:w/rd%?#[]", "รหัส ผ่าน+&="] {
            let opts = mongodb::options::ClientOptions::parse(export_uri(&p, true, Some(pw))).await.unwrap();
            let cred = opts.credential.unwrap();
            assert_eq!((cred.username.as_deref(), cred.password.as_deref(), cred.source.as_deref()), (Some("a@b:c"), Some(pw), Some("admin")));
        }
    }

    /// Whatever is ticked, the string is one the driver accepts.
    #[tokio::test]
    async fn every_export_parses() {
        let mut auth = manual(&["a:27017", "b:27017"], Some("rs0"), false);
        auth.auth = password_auth("app", "", Some("SCRAM-SHA-1"));
        auth.tls = TlsConfig { enabled: true, allow_invalid_certs: true, ..Default::default() };
        auth.default_db = Some("shop".into());
        let mut on_uri = uri_profile("mongodb://old@h:27017/?authSource=x&w=majority");
        on_uri.auth = password_auth("app", "admin", None);
        for p in [auth, on_uri, uri_profile("mongodb://app@h:27017/?authSource=admin&authMechanism=SCRAM-SHA-256"), manual(&["h:27017"], None, true)] {
            for (user, pw) in [(true, Some("pw")), (true, None), (false, None)] {
                let uri = export_uri(&p, user, pw);
                assert!(mongodb::options::ClientOptions::parse(&uri).await.is_ok(), "{uri}");
            }
        }
    }

    /// TLS on and invalid certificates allowed travel; file paths on this computer
    /// don't, and neither does SSH (the dialog says so).
    #[test]
    fn exports_tls_and_the_default_database_but_not_files_or_ssh() {
        let mut p = manual(&["h:27017"], None, false);
        p.tls = TlsConfig { enabled: true, allow_invalid_certs: true, ca_file: Some("/Users/me/ca.pem".into()), cert_key_file: Some("/Users/me/client.pem".into()) };
        p.ssh = Some(SshConfig { host: "bastion".into(), port: 22, user: "ec2".into(), auth: SshAuth::Password });
        p.default_db = Some("shop".into());
        assert_eq!(export_uri(&p, true, None), "mongodb://h:27017/shop?tls=true&tlsAllowInvalidCertificates=true");
        p.auth = password_auth("app", "", None);
        assert_eq!(export_uri(&p, true, None), "mongodb://app@h:27017/shop?authSource=admin&tls=true&tlsAllowInvalidCertificates=true", "an empty auth database means admin, as when connecting");
    }

    /// A profile saved from a URI exports that URI: its username only when
    /// asked for, the password (from the vault) added back when given.
    #[test]
    fn exports_a_uri_profile() {
        let p = uri_profile("mongodb+srv://app@cluster0.example.net/shop?retryWrites=true&authSource=admin");
        assert_eq!(export_uri(&p, true, None), "mongodb+srv://app@cluster0.example.net/shop?retryWrites=true&authSource=admin");
        assert_eq!(export_uri(&p, true, Some("p@ss")), "mongodb+srv://app:p%40ss@cluster0.example.net/shop?retryWrites=true&authSource=admin");
        assert_eq!(export_uri(&p, false, Some("p@ss")), "mongodb+srv://cluster0.example.net/shop?retryWrites=true");
        let typed = uri_profile("mongodb://app:old@h:27017");
        assert_eq!(export_uri(&typed, true, None), "mongodb://app@h:27017", "a password left in a typed URI goes only when asked for");
        assert_eq!(export_uri(&typed, true, Some("new")), "mongodb://app:new@h:27017");
    }

    /// The Authentication and TLS tabs apply to a URI too, as when
    /// connecting: their user, auth database and mechanism replace the URI's.
    #[test]
    fn a_uri_profile_takes_the_auth_and_tls_tabs() {
        let mut p = uri_profile("mongodb://old@h:27017/?authSource=x&AUTHMECHANISM=SCRAM-SHA-1&w=majority");
        p.auth = password_auth("app", "admin", Some("SCRAM-SHA-256"));
        p.tls.enabled = true;
        assert_eq!(export_uri(&p, true, Some("pw")), "mongodb://app:pw@h:27017/?w=majority&authSource=admin&authMechanism=SCRAM-SHA-256&tls=true");
        assert_eq!(export_uri(&p, false, None), "mongodb://h:27017/?w=majority&tls=true");
        p.auth = password_auth("app", "admin", None);
        assert_eq!(export_uri(&p, true, None), "mongodb://app@h:27017/?w=majority&authSource=admin&tls=true", "no mechanism in the tab: the server chooses, as when connecting");
    }

    /// Whatever is ticked, the only secret an export can carry is the
    /// password it is given: options typed into a URI that name files on
    /// this computer, the key file's password and an AWS session token stay out.
    #[test]
    fn exports_leave_out_files_and_other_secrets() {
        let p = uri_profile("mongodb://app@h:27017/?tls=true&tlsCAFile=%2FUsers%2Fme%2Fca.pem&tlsCertificateKeyFile=/Users/me/c.pem&TLSCERTIFICATEKEYFILEPASSWORD=k3y&sslPEMKeyPassword=old&authMechanism=MONGODB-AWS&authMechanismProperties=AWS_SESSION_TOKEN:t0ken,SERVICE_NAME:x&w=1");
        for pw in [None, Some("pw")] {
            let uri = export_uri(&p, true, pw);
            assert!(["ca.pem", "c.pem", "k3y", "old", "t0ken"].iter().all(|s| !uri.contains(s)), "{uri}");
        }
        assert_eq!(export_uri(&p, true, Some("pw")), "mongodb://app:pw@h:27017/?tls=true&authMechanism=MONGODB-AWS&authMechanismProperties=SERVICE_NAME:x&w=1");
        let only_token = uri_profile("mongodb://app@h:27017/?authMechanism=MONGODB-AWS&authMechanismProperties=aws_session_token%3At0ken");
        assert_eq!(export_uri(&only_token, true, None), "mongodb://app@h:27017/?authMechanism=MONGODB-AWS", "an escaped token goes too, and the option with it once empty");

        assert!(has_tls_files(&p), "TLS files typed into the URI");
        let mut form = manual(&["h:27017"], None, false);
        assert!(!has_tls_files(&form));
        form.tls.cert_key_file = Some("/Users/me/c.pem".into());
        assert!(has_tls_files(&form), "TLS files from the TLS tab");
    }
}
