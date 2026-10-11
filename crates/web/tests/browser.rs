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
        let content_type = body
            .is_some()
            .then_some("application/x-www-form-urlencoded");
        self.send(method, path, content_type, body.map(String::into_bytes))
            .await
    }

    async fn send(
        &mut self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> Page {
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
        if let Some(content_type) = content_type {
            request = request.header(hyper::header::CONTENT_TYPE, content_type);
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

    /// Posts a multipart form: text fields, then files as (field, file name, contents).
    async fn upload(
        &mut self,
        path: &str,
        fields: &[(&str, &str)],
        files: &[(&str, &str, &[u8])],
    ) -> Page {
        const BOUNDARY: &str = "tidefell-test-boundary";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        for (name, file_name, contents) in files {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; \
                     filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(contents);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
        self.send(Method::POST, path, Some(&content_type), Some(body))
            .await
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
                metadata_dir: None,
                relay: RelayConfig::Disabled,
                address_lookup: false,
                local_discovery: false,
                watch: false,
                limits: Default::default(),
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
            sync: Default::default(),
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
                    share_links: false,
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

    // Delete, then restore from the web. The engine keeps the content of what it deletes; a
    // file deleted by hand on the only device has no copy left to restore (ADR 0027).
    engine.delete(FOLDER, "report.pdf").await.unwrap();
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

    // The folder's activity: what this server applied, newest first, and by whom (ADR 0030).
    let activity = browser.get(&format!("/folders/{FOLDER}/activity")).await;
    assert_eq!(activity.status, StatusCode::OK);
    let restored_at = activity.body.find("restored").expect("the restore");
    let deleted_at = activity.body.find("deleted").expect("the delete");
    assert!(restored_at < deleted_at, "{}", activity.body);
    assert!(activity.body.contains("report.pdf") && activity.body.contains("this server"));
    let folder = browser.get(&format!("/folders/{FOLDER}")).await;
    assert!(folder.body.contains("<th>By</th>"), "{}", folder.body);

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
            .get(&format!("/folders/{FOLDER}/activity"))
            .await
            .status,
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

/// An engine with one folder holding `files`, and the web peer serving it with `config`.
async fn serve_folder(
    tmp: &std::path::Path,
    files: &[(&str, &[u8])],
    config: WebConfig,
) -> (Arc<Engine>, PathBuf, SocketAddr) {
    let alice = UserKey::from_seed([1; 32]);
    let key = DeviceKey::from_seed([3; 32]);
    let data_dir = tmp.join("data");
    let root = tmp.join("team-docs");
    let engine = Arc::new(
        Engine::start(
            EngineConfig {
                data_dir: data_dir.clone(),
                metadata_dir: None,
                relay: RelayConfig::Disabled,
                address_lookup: false,
                local_discovery: false,
                watch: false,
                limits: Default::default(),
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
            sync: Default::default(),
        })
        .unwrap();
    for (path, contents) in files {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, contents).unwrap();
    }
    engine.scan(FOLDER).await.unwrap();
    AccountStore::new(&data_dir)
        .upsert("alice", "alice password 1", alice.id(), false)
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn({
        let engine = engine.clone();
        async move { tidefell_web::serve(listener, engine, &data_dir, config).await }
    });
    (engine, root, addr)
}

/// The path part of the share link shown after creating one.
fn created_link(page: &Page) -> String {
    let marker = "value=\"http://localhost/s/";
    let start = page.body.find(marker).expect("page shows the new link") + marker.len() - 3;
    let end = start + page.body[start..].find('"').unwrap();
    page.body[start..end].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_and_share_links() {
    let tmp = tempfile::tempdir().unwrap();
    let config = WebConfig {
        secure_cookies: false,
        share_links: true,
    };
    let (engine, root, addr) = serve_folder(
        tmp.path(),
        &[
            ("report.pdf", b"%PDF quarterly numbers"),
            ("photos/beach.jpg", b"sand"),
            ("photos/2026/sea.jpg", b"waves"),
            ("private/diary.txt", b"not for sharing"),
        ],
        config,
    )
    .await;
    let mut alice = Browser::new(addr);
    alice.login("alice", "alice password 1").await;
    let folder_page = alice.get(&format!("/folders/{FOLDER}")).await;
    let csrf = folder_page.csrf();
    assert!(folder_page.body.contains("Upload files"));
    assert!(folder_page.body.contains("share"));

    // Upload two files into a new directory; they are recorded at once.
    let upload = format!("/folders/{FOLDER}/upload");
    let page = alice
        .upload(
            &upload,
            &[("csrf", &csrf), ("dir", "notes/2026")],
            &[
                ("file", "todo.txt", b"buy milk"),
                ("file", "C:\\Users\\alice\\plan.md", b"# plan"),
            ],
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert!(page.location().contains("uploaded=2"));
    assert_eq!(
        std::fs::read(root.join("notes/2026/todo.txt")).unwrap(),
        b"buy milk"
    );
    assert_eq!(
        std::fs::read(root.join("notes/2026/plan.md")).unwrap(),
        b"# plan"
    );
    let files = engine.files(FOLDER).unwrap();
    assert!(files.iter().any(|f| f.path == "notes/2026/todo.txt"));

    // Uploading over a file keeps the earlier version, which the history page restores
    // (ADR 0029).
    let page = alice
        .upload(
            &upload,
            &[("csrf", &csrf), ("dir", "notes/2026")],
            &[("file", "todo.txt", b"buy oat milk")],
        )
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.body);
    assert_eq!(
        std::fs::read(root.join("notes/2026/todo.txt")).unwrap(),
        b"buy oat milk"
    );
    let history = alice
        .get(&format!(
            "/folders/{FOLDER}/history?path=notes%2F2026%2Ftodo.txt"
        ))
        .await;
    assert!(history.body.contains("Earlier"), "{}", history.body);
    let earlier = engine.versions(FOLDER, "notes/2026/todo.txt").unwrap()[1]
        .id
        .clone();
    let restored = alice
        .post(
            &format!("/folders/{FOLDER}/history/restore"),
            &[
                ("csrf", &history.csrf()),
                ("path", "notes/2026/todo.txt"),
                ("version", &earlier),
            ],
        )
        .await;
    assert_eq!(restored.status, StatusCode::SEE_OTHER, "{}", restored.body);
    assert!(restored.location().contains("restored=true"));
    assert_eq!(
        std::fs::read(root.join("notes/2026/todo.txt")).unwrap(),
        b"buy milk"
    );
    let page = alice
        .get(&format!("/folders/{FOLDER}?uploaded=2"))
        .await
        .body;
    assert!(page.contains("Uploaded 2 file(s)"));
    // Uploads are made on this server, and no full replica holds them yet.
    assert!(page.contains("only on this server"));

    // Uploads need the form's CSRF token, and stay out of the engine's directory.
    let forged = alice
        .upload(
            &upload,
            &[("csrf", &"0".repeat(64))],
            &[("file", "x.txt", b"x")],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let missing = alice.upload(&upload, &[], &[("file", "x.txt", b"x")]).await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    let escape = alice
        .upload(
            &upload,
            &[("csrf", &csrf), ("dir", "../outside")],
            &[("file", "x.txt", b"x")],
        )
        .await;
    assert_eq!(escape.status, StatusCode::BAD_REQUEST);
    let reserved = alice
        .upload(
            &upload,
            &[("csrf", &csrf), ("dir", ".tidefell")],
            &[("file", "x.txt", b"x")],
        )
        .await;
    assert_eq!(reserved.status, StatusCode::BAD_REQUEST);
    assert!(!root.join("x.txt").exists() && !tmp.path().join("outside").exists());

    // A file link with a password and a limit of one download.
    let form = alice
        .get(&format!("/folders/{FOLDER}/share?path=report.pdf"))
        .await;
    assert_eq!(form.status, StatusCode::OK);
    let created = alice
        .post(
            &format!("/folders/{FOLDER}/share"),
            &[
                ("csrf", &form.csrf()),
                ("path", "report.pdf"),
                ("expires_days", "7"),
                ("password", "open sesame"),
                ("max_downloads", "1"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let file_link = created_link(&created);

    let mut visitor = Browser::new(addr);
    let page = visitor.get(&file_link).await;
    assert!(page.body.contains("Password needed"), "{}", page.body);
    assert_eq!(
        visitor.get(&format!("{file_link}/download")).await.status,
        StatusCode::SEE_OTHER,
        "no download before the password"
    );
    let wrong = visitor.post(&file_link, &[("password", "guess")]).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let right = visitor
        .post(&file_link, &[("password", "open sesame")])
        .await;
    assert_eq!(right.status, StatusCode::SEE_OTHER);
    let page = visitor.get(&file_link).await;
    assert!(page.body.contains("report.pdf") && page.body.contains("expires"));
    let download = visitor.get(&format!("{file_link}/download")).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, "%PDF quarterly numbers");
    assert_eq!(
        visitor.get(&format!("{file_link}/download")).await.status,
        StatusCode::GONE,
        "the limit was one download"
    );
    // The unlock cookie belongs to that link only, and other visitors still need the password.
    assert!(Browser::new(addr)
        .get(&file_link)
        .await
        .body
        .contains("Password needed"));

    // A directory link: its files and nothing else.
    let form = alice
        .get(&format!("/folders/{FOLDER}/share?path=photos"))
        .await;
    let created = alice
        .post(
            &format!("/folders/{FOLDER}/share"),
            &[("csrf", &form.csrf()), ("path", "photos")],
        )
        .await;
    let dir_link = created_link(&created);
    let mut visitor = Browser::new(addr);
    let page = visitor.get(&dir_link).await;
    assert!(page.body.contains("beach.jpg") && page.body.contains("2026/sea.jpg"));
    assert!(!page.body.contains("diary") && !page.body.contains("report.pdf"));
    let sea = visitor
        .get(&format!("{dir_link}/download?path=2026/sea.jpg"))
        .await;
    assert_eq!(sea.body, "waves");
    for outside in [
        "../private/diary.txt",
        "private/diary.txt",
        "",
        "../report.pdf",
    ] {
        assert_eq!(
            visitor
                .get(&format!("{dir_link}/download?path={outside}"))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{outside}"
        );
    }
    assert_eq!(
        visitor
            .get("/s/00000000000000000000000000000000")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(visitor.get("/shares").await.status, StatusCode::SEE_OTHER);

    // Alice lists and revokes her links; the tokens are not stored.
    let shares = alice.get("/shares").await;
    assert!(shares.body.contains("photos/") && shares.body.contains("1 of 1"));
    let stored = std::fs::read_to_string(tmp.path().join("data/web/shares.json")).unwrap();
    assert!(!stored.contains(dir_link.trim_start_matches("/s/")));
    let marker = "action=\"/shares/";
    let start = shares.body.find(marker).unwrap() + marker.len();
    let link_id = &shares.body[start..start + 12];
    let revoked = alice
        .post(
            &format!("/shares/{link_id}/revoke"),
            &[("csrf", &shares.csrf())],
        )
        .await;
    assert!(revoked.body.contains("Revoked"), "{}", revoked.body);
    // Links are listed newest first, so that was the directory link.
    assert_eq!(visitor.get(&dir_link).await.status, StatusCode::NOT_FOUND);
    assert_eq!(visitor.get(&file_link).await.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_only_links() {
    let tmp = tempfile::tempdir().unwrap();
    let config = WebConfig {
        secure_cookies: false,
        share_links: true,
    };
    let (engine, root, addr) = serve_folder(
        tmp.path(),
        &[
            ("inbox/taken.txt", b"already here"),
            ("report.pdf", b"%PDF"),
        ],
        config,
    )
    .await;
    let mut alice = Browser::new(addr);
    alice.login("alice", "alice password 1").await;

    // Only a directory link can be upload-only.
    let form = alice
        .get(&format!("/folders/{FOLDER}/share?path=report.pdf"))
        .await;
    assert!(!form.body.contains("upload_only"));
    let refused = alice
        .post(
            &format!("/folders/{FOLDER}/share"),
            &[
                ("csrf", &form.csrf()),
                ("path", "report.pdf"),
                ("upload_only", "on"),
            ],
        )
        .await;
    assert!(refused.body.contains("Only a link to a directory"));

    let form = alice
        .get(&format!("/folders/{FOLDER}/share?path=inbox"))
        .await;
    assert!(form.body.contains("upload_only"));
    let created = alice
        .post(
            &format!("/folders/{FOLDER}/share"),
            &[
                ("csrf", &form.csrf()),
                ("path", "inbox"),
                ("upload_only", "on"),
                ("max_upload_mib", "1"),
                ("max_uploads", "3"),
            ],
        )
        .await;
    assert!(created.body.contains("without seeing"), "{}", created.body);
    let link = created_link(&created);

    // Visitors see a form and none of the directory's contents, and cannot download.
    let mut visitor = Browser::new(addr);
    let page = visitor.get(&link).await;
    assert!(page.body.contains("/upload") && page.body.contains("3 more file(s)"));
    assert!(!page.body.contains("taken.txt"), "{}", page.body);
    assert_eq!(
        visitor
            .get(&format!("{link}/download?path=taken.txt"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A name already taken gets a number, and nothing is replaced.
    let upload = format!("{link}/upload");
    let sent = visitor
        .upload(
            &upload,
            &[],
            &[
                ("file", "taken.txt", b"from a visitor"),
                ("file", "notes", b"no extension"),
            ],
        )
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.body);
    assert!(sent.location().ends_with("uploaded=2"));
    assert_eq!(
        std::fs::read(root.join("inbox/taken.txt")).unwrap(),
        b"already here"
    );
    assert_eq!(
        std::fs::read(root.join("inbox/taken (1).txt")).unwrap(),
        b"from a visitor"
    );
    assert!(engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == "inbox/notes"));
    let page = visitor.get(&format!("{link}?uploaded=2")).await;
    assert!(page.body.contains("Sent 2 file(s)") && page.body.contains("1 more file(s)"));

    // Over the byte limit: refused, and nothing is left behind.
    let big = vec![7u8; (1 << 20) + 1];
    let refused = visitor
        .upload(&upload, &[], &[("file", "big.bin", &big)])
        .await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!root.join("inbox/big.bin").exists());
    let staged = std::fs::read_dir(root.join(".tidefell/tmp"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("upload-"))
        .count();
    assert_eq!(staged, 0);

    // The third file uses up the link.
    let sent = visitor
        .upload(
            &upload,
            &[],
            &[("file", "last.txt", b"3"), ("file", "extra.txt", b"4")],
        )
        .await;
    assert_eq!(sent.status, StatusCode::GONE);
    assert!(root.join("inbox/last.txt").exists() && !root.join("inbox/extra.txt").exists());
    assert!(visitor.get(&link).await.body.contains("upload limit"));

    let shares = alice.get("/shares").await;
    assert!(shares.body.contains("upload-only"));
    assert!(shares.body.contains("3 of 3 files"), "{}", shares.body);

    // Uploads through an ordinary link, or before a password is entered, are refused.
    let form = alice
        .get(&format!("/folders/{FOLDER}/share?path=inbox"))
        .await;
    let read_link = created_link(
        &alice
            .post(
                &format!("/folders/{FOLDER}/share"),
                &[("csrf", &form.csrf()), ("path", "inbox")],
            )
            .await,
    );
    let refused = visitor
        .upload(
            &format!("{read_link}/upload"),
            &[],
            &[("file", "x.txt", b"x")],
        )
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    let created = alice
        .post(
            &format!("/folders/{FOLDER}/share"),
            &[
                ("csrf", &form.csrf()),
                ("path", "inbox"),
                ("upload_only", "on"),
                ("password", "open sesame"),
            ],
        )
        .await;
    let locked = created_link(&created);
    let mut visitor = Browser::new(addr);
    let refused = visitor
        .upload(&format!("{locked}/upload"), &[], &[("file", "x.txt", b"x")])
        .await;
    assert_eq!(refused.status, StatusCode::SEE_OTHER);
    assert!(!root.join("inbox/x.txt").exists());
    visitor.post(&locked, &[("password", "open sesame")]).await;
    let sent = visitor
        .upload(&format!("{locked}/upload"), &[], &[("file", "x.txt", b"x")])
        .await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.body);
    assert!(root.join("inbox/x.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn share_links_are_off_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, _, addr) = serve_folder(
        tmp.path(),
        &[("report.pdf", b"numbers")],
        WebConfig {
            secure_cookies: false,
            share_links: false,
        },
    )
    .await;
    let mut alice = Browser::new(addr);
    alice.login("alice", "alice password 1").await;
    let folder = alice.get(&format!("/folders/{FOLDER}")).await;
    assert!(!folder.body.contains("/share"));
    assert!(
        folder.body.contains("Upload files"),
        "uploads stay available"
    );
    assert_eq!(
        alice
            .get(&format!("/folders/{FOLDER}/share?path=report.pdf"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(alice.get("/shares").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        alice
            .get("/s/00000000000000000000000000000000")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        alice
            .upload(
                "/s/00000000000000000000000000000000/upload",
                &[],
                &[("file", "x.txt", b"x")]
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// A second device of alice's with the same folder, started on `root`.
async fn laptop(tmp: &std::path::Path) -> Engine {
    let alice = UserKey::from_seed([1; 32]);
    let key = DeviceKey::from_seed([4; 32]);
    Engine::start(
        EngineConfig {
            data_dir: tmp.join("laptop-data"),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: tmp.join("laptop"),
                mode: ReplicaMode::Full,
                members: vec![alice.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
            }],
        },
        key.clone(),
        alice.certify(key.id(), "laptop", 0),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn deciding_a_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let config = WebConfig {
        secure_cookies: false,
        share_links: false,
    };
    let (engine, root, addr) = serve_folder(tmp.path(), &[("doc.txt", b"original")], config).await;
    let laptop_root = tmp.path().join("laptop");
    let other = laptop(tmp.path()).await;
    other.connect(engine.addr()).await.unwrap();
    eventually("the laptop has the file", || {
        std::fs::read(laptop_root.join("doc.txt")).is_ok()
    })
    .await;
    other.shutdown().await;

    // Both edit apart; the laptop's edit is later and wins.
    std::fs::write(root.join("doc.txt"), b"edit on the server").unwrap();
    engine.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(laptop_root.join("doc.txt"), b"edit on the laptop").unwrap();
    let other = laptop(tmp.path()).await;
    other.scan(FOLDER).await.unwrap();
    other.connect(engine.addr()).await.unwrap();
    eventually("the conflict reaches the server", || {
        engine
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "doc.txt" && f.conflicts == 1)
    })
    .await;

    let mut alice = Browser::new(addr);
    alice.login("alice", "alice password 1").await;
    let folder = alice.get(&format!("/folders/{FOLDER}")).await;
    assert!(
        folder.body.contains("wait for you to decide"),
        "{}",
        folder.body
    );
    assert!(folder.body.contains("conflict: decide"));
    let page = alice
        .get(&format!("/folders/{FOLDER}/conflict?path=doc.txt"))
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body.contains("laptop") && page.body.contains("web"),
        "{}",
        page.body
    );
    // The server's own version lost; it can be opened before deciding.
    let marker = "version=conflict%3A";
    let start = page.body.find(marker).expect("an other version") + "version=".len();
    let end = start + page.body[start..].find('"').unwrap();
    let version = page.body[start..end].to_string();
    let opened = alice
        .get(&format!(
            "/folders/{FOLDER}/conflict/open?path=doc.txt&version={version}"
        ))
        .await;
    assert_eq!(opened.body, "edit on the server");

    let decided = alice
        .post(
            &format!("/folders/{FOLDER}/resolve"),
            &[
                ("csrf", &page.csrf()),
                ("path", "doc.txt"),
                ("keep", "both"),
            ],
        )
        .await;
    assert_eq!(decided.status, StatusCode::SEE_OTHER, "{}", decided.body);
    assert!(decided.location().contains("resolved=doc.txt"));
    eventually("both versions are files on the laptop", || {
        std::fs::read_dir(&laptop_root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name().to_string_lossy().starts_with("doc (web, ")
                    && std::fs::read(e.path()).unwrap() == b"edit on the server"
            })
    })
    .await;
    let folder = alice.get(&format!("/folders/{FOLDER}")).await;
    assert!(!folder.body.contains("wait for you to decide"));
    // Decided: the conflict page goes back to the folder.
    let page = alice
        .get(&format!("/folders/{FOLDER}/conflict?path=doc.txt"))
        .await;
    assert_eq!(page.status, StatusCode::SEE_OTHER);
    other.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_problems() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let config = WebConfig {
        secure_cookies: false,
        share_links: false,
    };
    let (engine, root, addr) = serve_folder(tmp.path(), &[("readable.txt", b"fine")], config).await;
    AccountStore::new(&tmp.path().join("data"))
        .upsert(
            "root",
            "root password 12",
            UserKey::from_seed([1; 32]).id(),
            true,
        )
        .unwrap();

    let mut alice = Browser::new(addr);
    alice.login("alice", "alice password 1").await;
    let folders = alice.get("/").await;
    assert!(folders.body.contains("up to date"), "{}", folders.body);
    assert!(!folders.body.contains("href=\"/status\""));
    assert_eq!(alice.get("/status").await.status, StatusCode::FORBIDDEN);

    // A file this server cannot read is listed on the folder's page until it can.
    let locked = root.join("locked.txt");
    std::fs::write(&locked, b"secret").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let root_user = std::fs::read(&locked).is_ok();
    engine.scan(FOLDER).await.unwrap();
    let page = alice.get(&format!("/folders/{FOLDER}")).await;
    if !root_user {
        assert!(page.body.contains("locked.txt") && page.body.contains("read: permission denied"));
        let folders = alice.get("/").await;
        assert!(folders.body.contains("1 problem(s)"), "{}", folders.body);
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    engine.scan(FOLDER).await.unwrap();
    let page = alice.get(&format!("/folders/{FOLDER}")).await;
    assert!(!page.body.contains("permission denied"));

    // Administrators see the server's status.
    let mut admin = Browser::new(addr);
    admin.login("root", "root password 12").await;
    let status = admin.get("/status").await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.body);
    assert!(status.body.contains("does not sync with any other device"));
    assert!(status.body.contains("0 B sent"));
    let preview = admin.get("/status/diagnostics").await;
    assert!(preview.body.contains("summary.json") && preview.body.contains("doctor.json"));
    assert!(
        !preview.body.contains("team-docs"),
        "the folder's name is hashed"
    );
    let zip = admin.get("/status/diagnostics.zip").await;
    assert_eq!(zip.status, StatusCode::OK);
    assert!(zip.body.starts_with("PK"));
    assert_eq!(
        alice.get("/status/diagnostics").await.status,
        StatusCode::FORBIDDEN
    );
    let checked = admin.get("/status?check=true").await;
    assert!(
        checked.body.contains("can be read and written"),
        "{}",
        checked.body
    );
    assert!(checked.body.contains("engine state of 1 folder(s) loaded"));

    // A crash not dismissed yet can be written up for the tracker (ADR 0030).
    let file = tidefell_engine::diagnostics::write_crash(
        &engine.data_dir().join("crashes"),
        "rust",
        "boom <b>",
        "location: src/x.rs:1",
    )
    .unwrap();
    let id = file.file_name().unwrap().to_str().unwrap().to_owned();
    let status = admin.get("/status").await;
    let link = format!("/status/crashes/report?id={id}");
    assert!(status.body.contains(&link), "{}", status.body);
    let report = admin.get(&link).await;
    assert_eq!(report.status, StatusCode::OK);
    assert!(report.body.contains("Crash: boom"), "{}", report.body);
    assert!(!report.body.contains("boom <b>"), "escaped");
    assert!(report.body.contains("location: src/x.rs:1"));
    assert!(report.body.contains("Copy the title and body"));
    engine.set_report_url(Some("https://example.org/issues".into()));
    let report = admin.get(&link).await;
    assert!(report
        .body
        .contains("href=\"https://example.org/issues/new?title=Crash%3A%20boom"));
    assert!(report.body.contains("rel=\"noopener noreferrer\""));
    assert_eq!(alice.get(&link).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        admin
            .get("/status/crashes/report?id=../device.key")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
