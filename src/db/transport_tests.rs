//! Connection set-up against fake servers that record what the client sends,
//! as an attacker on the network path or a spoofed host would (audit M1, M2
//! and L1). No real database server is needed.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::config::ConnectionProfile;

fn read_exact(s: &mut TcpStream, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Body of a PostgreSQL message whose length (including itself) comes first.
fn read_pg_body(s: &mut TcpStream) -> Option<Vec<u8>> {
    let len = u32::from_be_bytes(read_exact(s, 4)?.try_into().ok()?) as usize;
    read_exact(s, len.checked_sub(4)?)
}

/// What a fake PostgreSQL server received on one connection.
#[derive(Debug, Default)]
pub(crate) struct PgCapture {
    pub ssl_request: bool,
    pub params: Vec<(String, String)>,
    pub password: Option<String>,
}

impl PgCapture {
    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A PostgreSQL server that declines TLS, as a downgrading attacker does,
/// then asks for the password in cleartext. With `tls`, it accepts TLS
/// instead and hangs up, so that the client sets up its TLS session.
pub(crate) fn fake_pg_server(tls: bool) -> (u16, mpsc::Receiver<PgCapture>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut cap = PgCapture::default();
            let _ = (|| -> Option<()> {
                let mut body = read_pg_body(&mut s)?;
                if body == [0x04, 0xd2, 0x16, 0x2f] {
                    cap.ssl_request = true;
                    if tls {
                        s.write_all(b"S").ok()?;
                        return None;
                    }
                    s.write_all(b"N").ok()?;
                    body = read_pg_body(&mut s)?;
                }
                // Startup message: protocol version, then key\0value\0 pairs.
                let parts: Vec<String> =
                    body[4..].split(|b| *b == 0).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
                for kv in parts.chunks(2) {
                    if let [k, v] = kv
                        && !k.is_empty()
                    {
                        cap.params.push((k.clone(), v.clone()));
                    }
                }
                // AuthenticationCleartextPassword.
                s.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 3]).ok()?;
                if read_exact(&mut s, 1)? != b"p" {
                    return None;
                }
                let pw = read_pg_body(&mut s)?;
                cap.password = Some(String::from_utf8_lossy(&pw[..pw.len() - 1]).into_owned());
                let mut fields = Vec::new();
                for (k, v) in [(b'S', "FATAL"), (b'C', "28P01"), (b'M', "password authentication failed")] {
                    fields.push(k);
                    fields.extend_from_slice(v.as_bytes());
                    fields.push(0);
                }
                fields.push(0);
                let mut msg = vec![b'E'];
                msg.extend_from_slice(&(fields.len() as u32 + 4).to_be_bytes());
                msg.extend_from_slice(&fields);
                s.write_all(&msg).ok()
            })();
            let _ = tx.send(cap);
        }
    });
    (port, rx)
}

/// A MySQL server that does not offer TLS (as after an attacker strips the
/// capability) and names `mysql_clear_password` as its auth plugin, which
/// makes sqlx 0.9 send the password in clear. Returns the client's reply.
/// With `tls`, it offers TLS and hangs up after the client's TLS request.
fn fake_mysql_server(tls: bool) -> (u16, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            // LONG_PASSWORD | CONNECT_WITH_DB | PROTOCOL_41 | TRANSACTIONS | SECURE_CONNECTION | PLUGIN_AUTH
            let caps: u32 = 1 | 8 | 512 | 8192 | (1 << 15) | (1 << 19) | if tls { 2048 } else { 0 };
            let mut p = vec![0x0a];
            p.extend_from_slice(b"8.0.36\0");
            p.extend_from_slice(&1u32.to_le_bytes());
            p.extend_from_slice(b"abcdefgh\0");
            p.extend_from_slice(&(caps as u16).to_le_bytes());
            p.push(255);
            p.extend_from_slice(&2u16.to_le_bytes());
            p.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
            p.push(21);
            p.extend_from_slice(&[0u8; 10]);
            p.extend_from_slice(b"ijklmnopqrst\0mysql_clear_password\0");
            let mut packet = (p.len() as u32).to_le_bytes()[..3].to_vec();
            packet.push(0);
            packet.extend_from_slice(&p);
            if s.write_all(&packet).is_err() {
                continue;
            }
            let Some(header) = read_exact(&mut s, 4) else { continue };
            let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            let Some(body) = read_exact(&mut s, len) else { continue };
            let mut err = vec![0xff];
            err.extend_from_slice(&1045u16.to_le_bytes());
            err.extend_from_slice(b"#28000denied");
            let mut packet = (err.len() as u32).to_le_bytes()[..3].to_vec();
            packet.push(header[3].wrapping_add(1));
            packet.extend_from_slice(&err);
            let _ = s.write_all(&packet);
            let _ = tx.send(body);
        }
    });
    (port, rx)
}

fn profile(driver: Driver, port: u16) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(driver);
    p.host = "127.0.0.1".into();
    p.port = Some(port);
    p.user = "app".into();
    p.database = "app".into();
    p
}

#[tokio::test]
async fn pg_modes_from_require_refuse_plaintext() {
    let (port, rx) = fake_pg_server(false);
    for mode in [SslMode::Require, SslMode::VerifyFull] {
        let mut p = profile(Driver::Postgres, port);
        p.ssl_mode = mode;
        assert!(Database::connect(&p, Some("s3cret")).await.is_err());
        let cap = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(cap.ssl_request);
        assert_eq!(cap.password, None, "{mode:?}");
    }
}

/// The fallback for `prefer` that the default for remote hosts avoids:
/// a downgraded connection hands the password over. Local servers keep it.
#[tokio::test]
async fn pg_prefer_is_only_the_default_for_local_servers() {
    let (port, rx) = fake_pg_server(false);
    let p = profile(Driver::Postgres, port);
    assert_eq!(p.ssl_mode, SslMode::Prefer);
    assert!(Database::connect(&p, Some("s3cret")).await.is_err());
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().password.as_deref(), Some("s3cret"));
    assert_eq!(SslMode::default_for_host("db.example.com"), SslMode::VerifyFull);
}

/// Audit M2: verify-ca without the profile's own CA would trust any public
/// certificate for any name, so it is refused before anything is sent.
#[tokio::test]
async fn verify_ca_needs_an_existing_ca_file() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for driver in [Driver::Postgres, Driver::MySql] {
        let mut p = profile(driver, port);
        p.ssl_mode = SslMode::VerifyCa;
        let err = Database::connect(&p, Some("s3cret")).await.err().unwrap().to_string();
        assert!(err.contains("verify-ca needs a CA file"), "{err}");
        p.ssl_ca = dir.path().join("missing.pem").to_string_lossy().into_owned();
        let err = Database::connect(&p, Some("s3cret")).await.err().unwrap().to_string();
        assert!(err.contains("not found"), "{err}");
    }
    assert!(listener.accept().is_err(), "nothing may reach the server");
}

/// The CA file is handed to the driver, which reads it when the server
/// accepts TLS: a broken certificate in it then fails the TLS set-up.
#[tokio::test]
async fn ca_file_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n").unwrap();
    let (pg, pg_rx) = fake_pg_server(true);
    let (my, my_rx) = fake_mysql_server(true);
    for (driver, port) in [(Driver::Postgres, pg), (Driver::MySql, my)] {
        let mut p = profile(driver, port);
        p.ssl_mode = SslMode::VerifyFull;
        let without = format!("{:#}", Database::connect(&p, Some("s3cret")).await.err().unwrap());
        assert!(!without.contains("BadEncoding"), "{without}");
        p.ssl_ca = ca.to_string_lossy().into_owned();
        let with = format!("{:#}", Database::connect(&p, Some("s3cret")).await.err().unwrap());
        assert!(with.contains("BadEncoding"), "{with}");
    }
    for _ in 0..2 {
        assert_eq!(pg_rx.recv_timeout(Duration::from_secs(5)).unwrap().password, None);
    }
    // Only the TLS request: no password.
    assert!(my_rx.try_iter().all(|reply| !reply.windows(6).any(|w| w == b"s3cret")));
}

/// sqlx 0.9 sends a MySQL password in clear when the server's first packet
/// names `mysql_clear_password`. verify-full (the default for remote
/// hosts) refuses a server without TLS before that happens.
#[tokio::test]
async fn mysql_verify_full_refuses_plaintext() {
    let (port, rx) = fake_mysql_server(false);
    let mut p = profile(Driver::MySql, port);
    p.ssl_mode = SslMode::VerifyFull;
    assert!(Database::connect(&p, Some("s3cret")).await.is_err());
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "no reply may be sent");

    // The upstream behaviour this guards against, with `prefer`.
    p.ssl_mode = SslMode::Prefer;
    assert!(Database::connect(&p, Some("s3cret")).await.is_err());
    let reply = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(reply.windows(7).any(|w| w == b"s3cret\0"));
}

/// What the status bar shows: only verify-full over TLS counts as verified;
/// anything else to a remote host is flagged.
#[test]
fn transport_summaries() {
    let tls = Transport::Tls { version: "TLSv1.3".into(), cipher: "TLS_AES_256_GCM_SHA384".into() };
    let mut remote = ConnectionProfile::new(Driver::Postgres);
    remote.host = "db.example.com".into();
    let local = ConnectionProfile::new(Driver::MySql);
    let summary = |p: &ConnectionProfile, mode, t: &Transport| {
        let p = ConnectionProfile { ssl_mode: mode, ..p.clone() };
        t.summary(&p).map(|(label, trust)| (trust, label))
    };
    use TransportTrust::*;
    let cases = [
        (&remote, SslMode::VerifyFull, &tls, Verified, "TLSv1.3 verified"),
        (&remote, SslMode::VerifyCa, &tls, Unverified, "TLSv1.3 host unchecked"),
        (&remote, SslMode::Prefer, &tls, Unverified, "TLSv1.3 unverified"),
        (&remote, SslMode::Prefer, &Transport::Plain, Exposed, "NOT ENCRYPTED"),
        (&remote, SslMode::Disable, &Transport::Unknown, Exposed, "NOT ENCRYPTED"),
        (&remote, SslMode::Require, &Transport::Unknown, Unverified, "TLS unverified"),
        (&remote, SslMode::Prefer, &Transport::Unknown, Unverified, "TLS unknown"),
        (&local, SslMode::Prefer, &Transport::Plain, Local, "no TLS"),
        (&local, SslMode::Require, &tls, Local, "TLSv1.3 unverified"),
        (&local, SslMode::VerifyFull, &tls, Verified, "TLSv1.3 verified"),
        (&local, SslMode::Prefer, &Transport::Socket, Local, "socket"),
    ];
    for (p, mode, t, trust, label) in cases {
        assert_eq!(summary(p, mode, t), Some((trust, label.to_string())), "{} {mode:?} {t:?}", p.host);
    }
    assert_eq!(Transport::File.summary(&ConnectionProfile::new(Driver::Sqlite)), None);
}

#[test]
fn client_variables() {
    for name in ["PGPASSWORD", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT", "PGOPTIONS", "PGUSER", "pgpassword"] {
        assert!(is_client_variable(OsStr::new(name)), "{name}");
    }
    for name in ["PATH", "P", "", "HOME", "XPGPASSWORD"] {
        assert!(!is_client_variable(OsStr::new(name)), "{name}");
    }
}

/// Audit L1: PG* variables exported for psql (a password, a client
/// certificate, options) must not reach the server a profile names. The
/// environment of a running test binary cannot be changed safely, so this
/// test runs itself again in a child process with those variables set.
#[test]
fn pg_environment_is_never_used() {
    const CHILD: &str = "DBTUI_TEST_PG_ENV";
    let name = format!("{}::pg_environment_is_never_used", module_path!().split_once("::").unwrap().1);
    match std::env::var(CHILD).as_deref() {
        // As `main` starts: the variables are removed, nothing is used.
        Ok("cleared") => {
            // SAFETY: the only test running in this process, before any
            // other thread is started.
            unsafe { clear_client_environment() };
            for var in ["PGPASSWORD", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT", "PGOPTIONS", "PGUSER", "pgappname"] {
                assert!(std::env::var_os(var).is_none(), "{var} still set");
            }
            assert_eq!(std::env::var("UNRELATED").as_deref(), Ok("kept"));
            // No user or database in the profile: the server's defaults.
            let cap = pg_connect_without_password("");
            assert_eq!(cap.password.as_deref(), Some(""));
            assert_ne!(cap.param("user"), Some("env-user"));
            assert_eq!(cap.param("database"), None);
            assert_eq!(cap.param("options"), None);
            assert_eq!(cap.param("application_name"), Some("dbtui"));
        }
        // Even if the variables were still there, the password is explicit.
        Ok("set") => assert_eq!(pg_connect_without_password("app").password.as_deref(), Some("")),
        _ => {
            for mode in ["set", "cleared"] {
                let out = Command::new(std::env::current_exe().unwrap())
                    .args([&name, "--exact", "--test-threads=1", "--nocapture"])
                    .env(CHILD, mode)
                    .env("PGPASSWORD", "exported-for-psql")
                    .env("PGSSLCERT", "/nonexistent/client.crt")
                    .env("PGSSLKEY", "/nonexistent/client.key")
                    .env("PGSSLROOTCERT", "/nonexistent/root.crt")
                    .env("PGOPTIONS", "-c default_transaction_read_only=off")
                    .env("PGUSER", "env-user")
                    .env("PGDATABASE", "env-db")
                    .env("pgappname", "lowercase")
                    .env("UNRELATED", "kept")
                    .output()
                    .unwrap();
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                assert!(out.status.success() && text.contains("1 passed"), "{mode}: {text}");
            }
        }
    }
}

fn pg_connect_without_password(user: &str) -> PgCapture {
    let (port, rx) = fake_pg_server(false);
    let mut p = profile(Driver::Postgres, port);
    p.user = user.into();
    p.database = user.into();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    assert!(rt.block_on(Database::connect(&p, None)).is_err());
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}
