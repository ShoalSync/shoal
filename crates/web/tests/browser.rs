//! The web peer end to end: accounts, sessions, folder access, downloads, restore and admin.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;
use tidefell_web::{AccountStore, WebConfig};

const FOLDER: FolderId = FolderId([5; 32]);

/// A minimal browser: one server, one session cookie.
struct Browser {
    addr: SocketAddr,
    cookie: Option<String>,
}

struct Page {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Page {
    fn location(&self) -> &str {
        self.headers
            .get(hyper::header::LOCATION)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
    }

    fn csrf(&self) -> String {
        let marker = "name=\"csrf\" value=\"";
        let start = self.body.find(marker).expect("page has a CSRF token") + marker.len();
        self.body[start..start + 64].to_string()
    }
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            let v: String = v
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

impl Browser {
    fn new(addr: SocketAddr) -> Self {
        Self { addr, cookie: None }
    }

    async fn request(&mut self, method: Method, path: &str, body: Option<String>) -> Page {
        let stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(hyper::header::HOST, "localhost");
        if let Some(cookie) = &self.cookie {
            request = request.header(hyper::header::COOKIE, cookie.as_str());
        }
        if body.is_some() {
            request = request.header(
                hyper::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            );
        }
        let response = sender
            .send_request(
                request
                    .body(Full::new(Bytes::from(body.unwrap_or_default())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        if let Some(set) = parts.headers.get(hyper::header::SET_COOKIE) {
            let set = set.to_str().unwrap();
            let pair = set.split(';').next().unwrap().to_string();
            self.cookie = if pair.ends_with('=') {
                None
            } else {
                Some(pair)
            };
        }
        let body = body.collect().await.unwrap().to_bytes();
        Page {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn get(&mut self, path: &str) -> Page {
        self.request(Method::GET, path, None).await
    }

    async fn post(&mut self, path: &str, pairs: &[(&str, &str)]) -> Page {
        self.request(Method::POST, path, Some(form(pairs))).await
    }

    async fn login(&mut self, username: &str, password: &str) -> Page {
        self.post("/login", &[("username", username), ("password", password)])
            .await
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn web_peer_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let bob = UserKey::from_seed([2; 32]);
    let key = DeviceKey::from_seed([3; 32]);
    let data_dir = tmp.path().join("data");
    let root: PathBuf = tmp.path().join("team-docs");
    let engine = Arc::new(
        Engine::start(
            EngineConfig {
                data_dir: data_dir.clone(),
                relay: RelayConfig::Disabled,
                address_lookup: false,
                bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                peers: Vec::new(),
                folders: Vec::new(),
            },
            key.clone(),
            alice.certify(key.id(), "web", 0),
        )
        .await
        .unwrap(),
    );
    engine
        .add_folder(FolderConfig {
            id: FOLDER,
            path: root.clone(),
            mode: ReplicaMode::Full,
            members: vec![alice.id()],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
        })
        .unwrap();
    std::fs::write(root.join("report.pdf"), b"%PDF quarterly numbers").unwrap();
    engine.scan(FOLDER).await.unwrap();

    let accounts = AccountStore::new(&data_dir);
    accounts
        .upsert("alice", "alice password 1", alice.id(), true)
        .unwrap();
    accounts
        .upsert("bob", "bob password 12", bob.id(), false)
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let (engine, data_dir) = (engine.clone(), data_dir.clone());
        async move {
            tidefell_web::serve(
                listener,
                engine,
                &data_dir,
                WebConfig {
                    secure_cookies: false,
                },
            )
            .await
        }
    });

    // Signed out: everything redirects to the sign-in page.
    let mut browser = Browser::new(addr);
    let page = browser.get("/").await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    assert_eq!(page.location(), "/login");
    let login = browser.get("/login").await;
    assert!(login.body.contains("Sign in"));
    let csp = login.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
    assert_eq!(login.headers["x-content-type-options"], "nosniff");

    // Wrong password.
    let page = browser.login("alice", "not the password").await;
    assert_eq!(page.status, StatusCode::UNAUTHORIZED);
    assert!(page.body.contains("Wrong username or password"));
    assert!(browser.cookie.is_none());

    // Sign in.
    let page = browser.login("alice", "alice password 1").await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    let set_cookie = page.headers["set-cookie"].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    let home = browser.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("team-docs"), "{}", home.body);

    // Browse and download.
    let folder = browser.get(&format!("/folders/{FOLDER}")).await;
    assert!(folder.body.contains("report.pdf"));
    let download = browser
        .get(&format!("/folders/{FOLDER}/download?path=report.pdf"))
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, "%PDF quarterly numbers");
    assert!(download.headers["content-disposition"]
        .to_str()
        .unwrap()
        .contains("report.pdf"));
    let traversal = browser
        .get(&format!(
            "/folders/{FOLDER}/download?path=../../data/device.key"
        ))
        .await;
    assert_eq!(traversal.status, StatusCode::NOT_FOUND);

    // Delete on disk, then restore from the web.
    std::fs::remove_file(root.join("report.pdf")).unwrap();
    engine.scan(FOLDER).await.unwrap();
    let bin = browser.get(&format!("/folders/{FOLDER}/recycle")).await;
    assert!(bin.body.contains("report.pdf"));
    let entry = engine.recycle_bin(FOLDER).unwrap()[0].entry.to_string();
    let restore = format!("/folders/{FOLDER}/recycle/{entry}/restore");
    let forged = browser.post(&restore, &[("csrf", &"0".repeat(64))]).await;
    assert_eq!(
        forged.status,
        StatusCode::FORBIDDEN,
        "CSRF token is required"
    );
    let restored = browser.post(&restore, &[("csrf", &bin.csrf())]).await;
    assert_eq!(restored.status, StatusCode::SEE_OTHER);
    assert!(restored.location().contains("restored=report.pdf"));
    eventually("restored file on disk", || {
        std::fs::read(root.join("report.pdf")).ok().as_deref()
            == Some(b"%PDF quarterly numbers".as_slice())
    })
    .await;

    // Bob is not a member: he sees nothing and cannot guess his way in.
    let mut bob_browser = Browser::new(addr);
    bob_browser.login("bob", "bob password 12").await;
    let home = bob_browser.get("/").await;
    assert!(!home.body.contains("team-docs"));
    assert_eq!(
        bob_browser.get(&format!("/folders/{FOLDER}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob_browser
            .get(&format!("/folders/{FOLDER}/download?path=report.pdf"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob_browser.get("/admin/accounts").await.status,
        StatusCode::FORBIDDEN
    );

    // Membership is checked on every request: once bob is added he sees the folder.
    engine.add_member(FOLDER, bob.id()).unwrap();
    assert!(bob_browser.get("/").await.body.contains("team-docs"));

    // Alice administers accounts: add carol, remove bob (who is signed out at once).
    let admin = browser.get("/admin/accounts").await;
    assert!(admin.body.contains("bob"));
    let carol = UserKey::from_seed([4; 32]).id().to_string();
    let saved = browser
        .post(
            "/admin/accounts",
            &[
                ("csrf", &admin.csrf()),
                ("username", "carol"),
                ("password", "carol password 1"),
                ("user", &carol),
            ],
        )
        .await;
    assert!(saved.body.contains("Saved account carol"), "{}", saved.body);
    let removed = browser
        .post("/admin/accounts/bob/delete", &[("csrf", &admin.csrf())])
        .await;
    assert!(removed.body.contains("Removed account bob"));
    assert_eq!(bob_browser.get("/").await.status, StatusCode::SEE_OTHER);
    let mut carol_browser = Browser::new(addr);
    assert_eq!(
        carol_browser
            .login("carol", "carol password 1")
            .await
            .status,
        StatusCode::SEE_OTHER
    );

    // Sign out.
    let home = browser.get("/").await;
    let out = browser.post("/logout", &[("csrf", &home.csrf())]).await;
    assert_eq!(out.location(), "/login");
    assert!(browser.cookie.is_none());
    assert_eq!(browser.get("/").await.status, StatusCode::SEE_OTHER);
}
