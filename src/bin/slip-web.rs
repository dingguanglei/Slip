//! Local-only Web application. Credentials and plaintext never leave this host
//! except through the authenticated mail transport (encrypted chat payloads).
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use slip::{ChatClient, Engine, Event, MailCache, MailConfig, MailCore};
use std::{
    collections::BTreeMap,
    io::Read,
    sync::{Arc, Mutex},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

struct Account {
    client: ChatClient,
    engine: Mutex<Option<Engine>>,
    status: Mutex<String>,
}
struct App {
    accounts: Mutex<BTreeMap<String, Arc<Account>>>,
    downloads: Mutex<BTreeMap<String, (std::time::Instant, Vec<u8>)>>,
    token: String,
    host: String,
    cache: MailCache,
}
#[derive(Deserialize)]
struct Action {
    action: String,
    #[serde(default)]
    files: Vec<Upload>,
    #[serde(default)]
    account: String,
    #[serde(default)]
    contact: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    fingerprint: String,
}
#[derive(Deserialize)]
struct Upload {
    name: String,
    data: String,
}
#[derive(Deserialize)]
struct MediaRequest {
    account: String,
    contact: String,
    id: String,
    index: usize,
}
struct TemporaryFiles(std::path::PathBuf);
impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn upload_name(name: &str, index: usize) -> String {
    let clean: String = name
        .chars()
        .take(180)
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if clean.trim().is_empty() {
        format!("attachment-{index}")
    } else {
        clean
    }
}
fn send_uploads(client: &ChatClient, input: &Action) -> Result<Value> {
    if input.body.len() > 64 * 1024 || input.files.len() > 8 {
        return Err(anyhow!("消息或附件数量超出限制"));
    }
    // Check before decoding or writing upload data.
    if !client.contact_info(&input.contact)?.encryption_active {
        return Err(anyhow!("请先交换并验证公钥，内容未发送"));
    }
    let directory = TemporaryFiles(
        client
            .cache()
            .root()
            .join("uploads")
            .join(slip::crypto::random_id()),
    );
    std::fs::create_dir_all(&directory.0)?;
    let mut total = 0;
    let mut paths = Vec::new();
    for (index, file) in input.files.iter().enumerate() {
        let bytes = STANDARD.decode(&file.data)?;
        total += bytes.len();
        if total > 12 * 1024 * 1024 {
            return Err(anyhow!("附件总大小不能超过 12 MB"));
        }
        let folder = directory.0.join(index.to_string());
        std::fs::create_dir(&folder)?;
        let name = upload_name(&file.name, index);
        if name == "." || name == ".." {
            return Err(anyhow!("无效文件名"));
        }
        let path = folder.join(name);
        std::fs::write(&path, bytes)?;
        paths.push(path);
    }
    Ok(json!(client.send_parts(
        &input.contact,
        &input.body,
        &paths
    )?))
}

fn read_media(app: &App, bytes: &[u8]) -> Result<(Vec<u8>, String)> {
    let input: MediaRequest = serde_json::from_slice(bytes)?;
    let account = app
        .accounts
        .lock()
        .unwrap()
        .get(&input.account)
        .cloned()
        .ok_or_else(|| anyhow!("unknown account"))?;
    let session = account.client.session(&input.contact)?;
    let media = session
        .messages
        .iter()
        .find(|m| m.id == input.id)
        .and_then(|m| m.media.get(input.index))
        .ok_or_else(|| anyhow!("attachment not found"))?;
    let path = std::fs::canonicalize(&media.path)?;
    if !path.starts_with(std::fs::canonicalize(account.client.cache().root())?) {
        return Err(anyhow!("attachment outside account store"));
    }
    let safe_mime = match media.mime.as_str() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" => media.mime.clone(),
        _ => "application/octet-stream".into(),
    };
    Ok((std::fs::read(path)?, safe_mime))
}
fn add_account(app: &App, mut config: MailConfig) -> Result<String> {
    if let Ok(subject) = std::env::var("SLIP_SUBJECT")
        && !subject.trim().is_empty()
    {
        config.subject = subject;
    }
    let address = config.address.trim().to_ascii_lowercase();
    let root = app
        .cache
        .root()
        .join("accounts")
        .join(hex::encode(address.as_bytes()));
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let client = ChatClient::new(MailCore::new(config), MailCache::at(root))?;
    app.accounts
        .lock()
        .unwrap()
        .entry(address.clone())
        .or_insert_with(|| {
            Arc::new(Account {
                client,
                engine: Mutex::new(None),
                status: Mutex::new("尚未连接".into()),
            })
        });
    Ok(address)
}
fn dispatch(app: &App, input: Action) -> Result<Value> {
    if input.action == "accounts" {
        return Ok(
            json!({"accounts": app.accounts.lock().unwrap().keys().cloned().collect::<Vec<_>>()}),
        );
    }
    if input.action == "login" {
        let address = input.account.trim().to_ascii_lowercase();
        let provider = slip::providers::provider_for_address(&address)
            .ok_or_else(|| anyhow!("暂支持 QQ、Gmail、iCloud 等预设邮箱"))?;
        let config = MailConfig::from_provider(provider, address, input.password);
        MailCore::new(config.clone())
            .check_login()
            .map_err(|_| anyhow!("登录失败，请检查授权码、IMAP 服务和网络"))?;
        return Ok(json!({"account": add_account(app, config)?}));
    }
    if input.action == "logout" {
        let removed = app.accounts.lock().unwrap().remove(&input.account);
        if let Some(account) = removed {
            if let Some(engine) = account.engine.lock().unwrap().take() {
                engine.shutdown();
            }
            app.downloads.lock().unwrap().retain(|_, (_, bytes)| {
                serde_json::from_slice::<MediaRequest>(bytes)
                    .is_ok_and(|request| request.account != input.account)
            });
        }
        return Ok(json!({"ok":true}));
    }
    let account = app
        .accounts
        .lock()
        .unwrap()
        .get(&input.account)
        .cloned()
        .ok_or_else(|| anyhow!("请选择邮箱"))?;
    let client = &account.client;
    match input.action.as_str() {
        "state" => {
            let mut engine = account.engine.lock().unwrap();
            if engine.is_none() {
                *engine = Some(Engine::start_with_cleanup(
                    client.clone(),
                    "INBOX".into(),
                    true,
                ));
            }
            if let Some(engine) = engine.as_ref() {
                while let Some(event) = engine.try_next() {
                    if let Event::Status(s) | Event::WatcherStatus(s) = event {
                        *account.status.lock().unwrap() = s;
                    }
                }
            }
            let conversation = if input.contact.is_empty() {
                Value::Null
            } else {
                json!(client.session(&input.contact)?)
            };
            let info = if input.contact.is_empty() {
                Value::Null
            } else {
                json!(client.contact_info(&input.contact)?)
            };
            Ok(
                json!({"sessions":client.sessions()?, "conversation":conversation, "info":info,
                "status":*account.status.lock().unwrap(), "fingerprint":client.identity_fingerprint(), "public_key":client.identity_public_key()}),
            )
        }
        "info" => Ok(json!(client.contact_info(&input.contact)?)),
        "history" => Ok(json!(client.session(&input.contact)?)),
        "contact" => {
            client.add_contact(&input.contact)?;
            Ok(json!({"ok":true}))
        }
        "exchange" => {
            client.exchange_key(&input.contact)?;
            Ok(json!({"ok":true}))
        }
        "send" => {
            let result = send_uploads(client, &input)?;
            if let Some(engine) = account.engine.lock().unwrap().as_ref() {
                engine.send(slip::engine::Command::Sync);
            }
            Ok(result)
        }
        "read" => {
            client.mark_read(&input.contact)?;
            Ok(json!({"ok":true}))
        }
        "retry" => Ok(json!(client.retry_last_failed(&input.contact)?)),
        "trust" => {
            let info = client.contact_info(&input.contact)?;
            if info.pending_fingerprint.as_deref() != Some(input.fingerprint.as_str()) {
                return Err(anyhow!("请确认当前待验证指纹"));
            }
            Ok(json!(client.trust_peer(&input.contact)?))
        }
        "sync" => Ok(json!(client.sync("INBOX", 200, true)?)),
        _ => Err(anyhow!("未知操作")),
    }
}
fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name, value).unwrap()
}
fn respond(req: Request, code: u16, mime: &str, data: String) {
    let response = Response::from_string(data).with_status_code(StatusCode(code))
        .with_header(header("Content-Type", mime))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data: blob:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    let _ = req.respond(response);
}
fn handle(mut req: Request, app: &App) {
    let host = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str());
    if host != Some(app.host.as_str()) {
        return respond(req, 403, "text/plain", "Forbidden host".into());
    }
    if req.method() == &Method::Get && req.url().starts_with("/download/") {
        let entry = app.downloads.lock().unwrap().remove(&req.url()[10..]);
        if let Some((created, request)) = entry
            && created.elapsed().as_secs() < 60
            && let Ok((data, mime)) = read_media(app, &request)
        {
            let _ = req.respond(
                Response::from_data(data)
                    .with_header(header("Content-Type", &mime))
                    .with_header(header("Content-Disposition", "attachment"))
                    .with_header(header("Cache-Control", "no-store"))
                    .with_header(header("X-Content-Type-Options", "nosniff"))
                    .with_header(header("Referrer-Policy", "no-referrer")),
            );
            return;
        }
        return respond(req, 404, "text/plain", "Download expired".into());
    }
    if req.method() == &Method::Get {
        let asset = match req.url() {
            "/" => Some((
                "text/html; charset=utf-8",
                include_str!("../../web/index.html"),
            )),
            "/app.js" => Some((
                "text/javascript; charset=utf-8",
                include_str!("../../web/app.js"),
            )),
            "/style.css" => Some((
                "text/css; charset=utf-8",
                include_str!("../../web/style.css"),
            )),
            _ => None,
        };
        if let Some((mime, content)) = asset {
            return respond(req, 200, mime, content.into());
        }
    }
    let authorized = req.headers().iter().any(|h| {
        h.field.equiv("Authorization") && h.value.as_str() == format!("Bearer {}", app.token)
    });
    if !authorized {
        return respond(
            req,
            401,
            "application/json",
            json!({"error":"请从启动时显示的完整链接打开面板"}).to_string(),
        );
    }
    if req.method() == &Method::Post && req.url() == "/download-ticket" {
        let result = (|| -> Result<String> {
            let mut bytes = Vec::new();
            req.as_reader().take(4097).read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(anyhow!("request too large"));
            }
            read_media(app, &bytes)?;
            let id = slip::crypto::random_id();
            let mut tickets = app.downloads.lock().unwrap();
            tickets.retain(|_, (created, _)| created.elapsed().as_secs() < 60);
            if tickets.len() >= 256 {
                return Err(anyhow!("too many downloads"));
            }
            tickets.insert(id.clone(), (std::time::Instant::now(), bytes));
            Ok(format!("/download/{id}"))
        })();
        return match result {
            Ok(url) => respond(req, 200, "application/json", json!({"url":url}).to_string()),
            Err(_) => respond(
                req,
                404,
                "application/json",
                json!({"error":"attachment unavailable"}).to_string(),
            ),
        };
    }
    if req.method() == &Method::Post && req.url() == "/media" {
        let mut bytes = Vec::new();
        let result = req
            .as_reader()
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(anyhow::Error::from)
            .and_then(|_| {
                if bytes.len() > 4096 {
                    return Err(anyhow!("request too large"));
                }
                read_media(app, &bytes)
            });
        match result {
            Ok((data, mime)) => {
                let _ = req.respond(
                    Response::from_data(data)
                        .with_header(header("Content-Type", &mime))
                        .with_header(header("Cache-Control", "no-store"))
                        .with_header(header("X-Content-Type-Options", "nosniff"))
                        .with_header(header("Content-Disposition", "attachment")),
                );
            }
            Err(_) => respond(
                req,
                404,
                "application/json",
                json!({"error":"attachment unavailable"}).to_string(),
            ),
        }
        return;
    }
    if req.method() != &Method::Post || req.url() != "/api" {
        return respond(req, 404, "text/plain", "Not found".into());
    }
    let result = (|| -> Result<Value> {
        let mut bytes = Vec::new();
        req.as_reader()
            .take(18 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 18 * 1024 * 1024 {
            return Err(anyhow!("请求过大"));
        }
        dispatch(app, serde_json::from_slice(&bytes)?)
    })();
    match result {
        Ok(v) => respond(req, 200, "application/json", v.to_string()),
        Err(e) => respond(
            req,
            400,
            "application/json",
            json!({"error":e.to_string()}).to_string(),
        ),
    }
}
fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help") {
        println!(
            "slip-web: local Web chat on 127.0.0.1:8025. Set SLIP_WEB_PORT / SLIP_HOME. Default: manual login. Open the printed token URL."
        );
        return Ok(());
    }
    let port: u16 = std::env::var("SLIP_WEB_PORT")
        .unwrap_or_else(|_| "8025".into())
        .parse()?;
    if std::env::args().any(|a| a == "--parent-stdio") {
        std::thread::spawn(|| {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            std::process::exit(0);
        });
    }
    let server = Arc::new(Server::http(("127.0.0.1", port)).map_err(|e| anyhow!("{e}"))?);
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("missing local port"))?
        .port();
    let cache = MailCache::default();
    let app = Arc::new(App {
        accounts: Mutex::new(BTreeMap::new()),
        downloads: Mutex::new(BTreeMap::new()),
        token: slip::crypto::random_id(),
        host: format!("127.0.0.1:{port}"),
        cache,
    });
    println!("Slip Web: http://{}/#{}", app.host, app.token);
    // Fixed worker count bounds concurrent requests and memory use.
    for _ in 0..7 {
        let app = app.clone();
        let server = server.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                handle(req, &app);
            }
        });
    }
    for req in server.incoming_requests() {
        handle(req, &app);
    }
    Ok(())
}
