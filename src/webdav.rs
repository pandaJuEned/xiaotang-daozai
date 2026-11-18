//! WebDAV 音乐库：配置、目录列举与文件下载。
//!
//! 只访问用户自己填写的服务器：地址必须是 `https`（本机回环允许 `http`），
//! 返回的地址必须与服务器同源，带用户信息的地址一律拒收。
//! 密码进 Windows 凭据管理器，`webdav.json` 只存地址、用户名与当前目录。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 凭据管理器里的服务名。
pub const SERVICE: &str = "Daozai.WebDav";
const CONFIG_FILE: &str = "webdav.json";
/// 一次列目录最多接受多少条，防止异常服务器把内存撑爆。
const MAX_ENTRIES: usize = 2000;
/// 单个文件上限：再大就不是歌曲了。
pub const MAX_FILE: u64 = 512 * 1024 * 1024;

/// 能交给 rodio 解码的扩展名（与 Cargo.toml 里开启的 rodio 特性保持一致）。
const AUDIO: [&str; 7] = ["mp3", "flac", "wav", "ogg", "oga", "m4a", "aac"];

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// 服务器根地址，例如 `https://nas.example.com:5006/music`。
    pub url: String,
    pub user: String,
    /// 上次浏览到的集合地址（绝对 URL）；空表示从 `url` 开始。
    pub dir: String,
}

impl Config {
    pub fn path() -> PathBuf {
        crate::model::config_dir().join(CONFIG_FILE)
    }

    pub fn load() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self).unwrap_or_default();
        crate::model::atomic_write(&Self::path(), &body)
    }

    /// 地址与用户名都填了才允许连接（密码可以为空，有些服务器是匿名只读）。
    pub fn ready(&self) -> bool {
        !self.url.trim().is_empty() && !self.user.trim().is_empty()
    }

    /// 规范化后的根地址；地址不合法时为 `None`。
    pub fn base(&self) -> Option<String> {
        normalize_base(&self.url)
    }

    /// 浏览起点：`dir` 有效就用它，否则用根地址。
    pub fn start(&self) -> Option<String> {
        let base = self.base()?;
        match &self.dir {
            dir if !dir.trim().is_empty() && same_origin(&base, dir.trim()) => Some(dir.trim().to_string()),
            _ => Some(base),
        }
    }
}

fn password_entry(cfg: &Config) -> Result<keyring::Entry, String> {
    let host = cfg
        .base()
        .and_then(|base| url::Url::parse(&base).ok())
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "server".into());
    keyring::Entry::new(SERVICE, &format!("{}@{}", cfg.user.trim(), host))
        .map_err(|e| format!("凭据管理器不可用：{e}"))
}

pub fn save_password(cfg: &Config, password: &str) -> Result<(), String> {
    password_entry(cfg)?
        .set_password(password)
        .map_err(|e| format!("保存密码失败：{e}"))
}

pub fn load_password(cfg: &Config) -> Result<String, String> {
    password_entry(cfg)?
        .get_password()
        .map_err(|_| "没有保存密码".to_string())
}

pub fn delete_password(cfg: &Config) {
    if let Ok(entry) = password_entry(cfg) {
        let _ = entry.delete_credential();
    }
}

/* ---------------- 地址 ---------------- */

/// 补全 scheme，并把路径规范成「集合」形式：末尾一定带斜杠。
///
/// 尾斜杠不是装饰：Apache 的 mod_dav 遇到 `/music` 会 301 跳到 `/music/`，
/// 而跳转会丢掉 Authorization（见 `send`），于是连接被误报成密码错误；
/// 相对 href（`b.mp3`）也只有按 `/music/` 解析才会落到同一目录。
pub fn normalize_base(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let text = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
    let mut url = url::Url::parse(&text).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    // 地址里不允许带账号密码：凭据只走凭据管理器，避免留在配置文件和日志里。
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let path = url.path().to_string();
    let path = if path.ends_with('/') { path } else { format!("{path}/") };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

/// 明文 http 只允许访问本机回环地址。
fn transport_ok(url: &url::Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
            || url.host_str() == Some("localhost"),
        _ => false,
    }
}

fn same_origin(base: &str, target: &str) -> bool {
    match (url::Url::parse(base), url::Url::parse(target)) {
        (Ok(base), Ok(target)) => base.origin() == target.origin(),
        _ => false,
    }
}

/// 把服务器返回的 `href` 解析成可请求的绝对地址，并做安全校验。
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    let mut target = base.join(href).ok()?;
    if !transport_ok(&target) || !target.username().is_empty() || target.password().is_some() {
        return None;
    }
    target.set_fragment(None);
    Some(target.to_string())
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn is_audio(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// 显示名（已做百分号解码）。
    pub name: String,
    /// 可请求的绝对地址。
    pub url: String,
    pub is_dir: bool,
    pub bytes: u64,
}

impl Entry {
    pub fn playable(&self) -> bool {
        !self.is_dir && is_audio(&self.name)
    }
}

/* ---------------- 207 解析 ---------------- */

/// 把 PROPFIND 的 207 拆成条目；跳过集合自身。
pub fn parse_entries(xml: &str, collection: &str) -> Vec<Entry> {
    let block = regex::Regex::new(r"(?is)<(?:[a-z0-9]+:)?response\b[^>]*>(.*?)</(?:[a-z0-9]+:)?response>").unwrap();
    let href = regex::Regex::new(r"(?is)<(?:[a-z0-9]+:)?href\b[^>]*>(.*?)</(?:[a-z0-9]+:)?href>").unwrap();
    let resource = regex::Regex::new(r"(?is)<(?:[a-z0-9]+:)?resourcetype\b[^>]*>(.*?)</(?:[a-z0-9]+:)?resourcetype>").unwrap();
    let is_collection = regex::Regex::new(r"(?is)<(?:[a-z0-9]+:)?collection\b").unwrap();
    let size = regex::Regex::new(r"(?is)<(?:[a-z0-9]+:)?getcontentlength\b[^>]*>(.*?)</(?:[a-z0-9]+:)?getcontentlength>").unwrap();

    let self_url = url::Url::parse(collection).ok();
    let mut out = Vec::new();
    for cap in block.captures_iter(xml) {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let inner = &cap[1];
        let Some(raw) = href.captures(inner).map(|cap| unescape(&cap[1])) else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Some(url) = resolve(collection, raw) else { continue };
        // 只接受与本集合同源的条目：服务器不能把我们带到别的域名去下载。
        if !same_origin(collection, &url) {
            continue;
        }
        // 集合自身不算条目。
        if self_url.as_ref().is_some_and(|this| {
            let mut a = this.clone();
            let mut b = url::Url::parse(&url).unwrap_or(a.clone());
            a.set_query(None);
            b.set_query(None);
            a.path().trim_end_matches('/') == b.path().trim_end_matches('/') && a.origin() == b.origin()
        }) {
            continue;
        }
        let dir = resource.captures(inner).is_some_and(|cap| is_collection.is_match(&cap[1])) || raw.ends_with('/');
        let name = url::Url::parse(&url)
            .ok()
            .map(|parsed| {
                let path = parsed.path().trim_end_matches('/');
                percent_decode(path.rsplit('/').next().unwrap_or(""))
            })
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let bytes = size
            .captures(inner)
            .and_then(|cap| cap[1].trim().parse::<u64>().ok())
            .unwrap_or(0);
        out.push(Entry { name, url, is_dir: dir, bytes });
    }
    // 目录在前，各自按名称排序，避免服务器顺序不可预期。
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

fn unescape(text: &str) -> String {
    // href 里常见的实体只处理这几个，够用且不会误伤路径中的 &。
    text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
}

/* ---------------- 请求 ---------------- */

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(25))
        .timeout_write(std::time::Duration::from_secs(25))
        .timeout(std::time::Duration::from_secs(40))
        // 自己跟随跳转：ureq 的默认实现会把 301 改成 GET 并丢掉 Authorization。
        .redirects(0)
        .user_agent(concat!("Daozai/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn auth(user: &str, password: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:displayname/></d:prop></d:propfind>"#;

/// 一次请求最多跟几跳。
const MAX_REDIRECTS: usize = 3;

/// 请求失败的三类来源：状态码、网络错误、被拒绝的跳转。
#[derive(Debug)]
enum Failure {
    Status(u16),
    Network(String),
    Redirect(String),
}

impl Failure {
    fn for_list(self) -> String {
        match self {
            Failure::Status(401) => "用户名或密码不正确（401）".to_string(),
            Failure::Status(403) => "该账号没有读取权限（403）".to_string(),
            Failure::Status(404) => "目录不存在，请检查地址与路径".to_string(),
            Failure::Status(405) => "该地址不支持 WebDAV（服务器未开启 PROPFIND）".to_string(),
            Failure::Status(code) => format!("服务器返回 {code}"),
            Failure::Network(error) => error,
            Failure::Redirect(reason) => reason,
        }
    }

    fn for_download(self) -> String {
        match self {
            Failure::Status(401) => "用户名或密码不正确（401）".to_string(),
            Failure::Status(403) => "没有下载权限".to_string(),
            Failure::Status(404) => "文件已不存在".to_string(),
            Failure::Status(code) => format!("服务器返回 {code}"),
            Failure::Network(error) => error,
            Failure::Redirect(reason) => reason,
        }
    }
}

/// 手动跟随跳转，保持方法与凭据。
///
/// ureq 自带的跟随在这里是错的：301 会把 PROPFIND 变成 GET，且默认丢掉
/// Authorization。服务器把 `/music` 跳到 `/music/` 时，第二次请求就成了匿名
/// GET，返回 401 —— 界面上看着像密码错了，实际是跳转把凭据弄丢了。
/// 这里只跟同源、且不会降级成明文 http 的跳转，其余直接报错说明原因。
fn send(
    method: &str,
    url: &str,
    body: Option<&str>,
    user: &str,
    password: &str,
) -> Result<(ureq::Response, String), Failure> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let parsed = url::Url::parse(&current).map_err(|_| Failure::Redirect("地址无效".to_string()))?;
        if !transport_ok(&parsed) {
            return Err(Failure::Redirect("只支持 https 地址（本机可用 http）".to_string()));
        }
        let mut request = agent().request(method, &current).set("Authorization", &auth(user, password));
        if body.is_some() {
            request = request.set("Depth", "1").set("Content-Type", "application/xml; charset=utf-8");
        }
        let result = match body {
            Some(body) => request.send_string(body),
            None => request.call(),
        };
        match result {
            Ok(response) => {
                if !(300..400).contains(&response.status()) {
                    return Ok((response, current));
                }
                // 3xx 但没有 Location：交给调用方按状态码报错。
                let Some(location) = response.header("location") else {
                    return Ok((response, current));
                };
                current = next_hop(&current, location)?;
            }
            Err(ureq::Error::Status(code, _)) => return Err(Failure::Status(code)),
            Err(error) => return Err(Failure::Network(format!("连接失败：{error}"))),
        }
    }
    Err(Failure::Redirect("重定向次数过多，请检查服务器地址".to_string()))
}

/// 校验下一跳：必须是同源、且仍然是 https（或本机回环）。
fn next_hop(current: &str, location: &str) -> Result<String, Failure> {
    let base = url::Url::parse(current).map_err(|_| Failure::Redirect("地址无效".to_string()))?;
    let mut target = base
        .join(location)
        .map_err(|_| Failure::Redirect(format!("服务器给出的跳转地址无效：{location}")))?;
    target.set_fragment(None);
    if !transport_ok(&target) {
        return Err(Failure::Redirect(
            "服务器要求跳转到 http 地址，已拒绝（凭据会明文传输）".to_string(),
        ));
    }
    if !same_origin(current, target.as_str()) {
        return Err(Failure::Redirect("服务器跳转到了其他站点，已拒绝".to_string()));
    }
    Ok(target.to_string())
}

/// 列出一个集合下的直接子项，返回实际生效的集合地址（跟过跳转后可能补上尾斜杠）。
/// `collection` 必须是绝对地址。
pub fn list(collection: &str, user: &str, password: &str) -> Result<(String, Vec<Entry>), String> {
    let (response, collection) = send("PROPFIND", collection, Some(PROPFIND_BODY), user, password)
        .map_err(Failure::for_list)?;
    let status = response.status();
    if status >= 300 {
        return Err(format!("服务器返回 {status}，但没有给出跳转地址"));
    }
    let xml = response.into_string().map_err(|e| format!("读取目录失败：{e}"))?;
    let entries = parse_entries(&xml, &collection);
    Ok((collection, entries))
}

/// 下载到 `dest`，边下边回报进度（0..1，拿不到总长度时一直是 0）。
pub fn download(
    base: &str,
    url: &str,
    user: &str,
    password: &str,
    dest: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<u64, String> {
    let parsed = url::Url::parse(url).map_err(|_| "文件地址无效".to_string())?;
    if !transport_ok(&parsed) {
        return Err("只支持 https 地址（本机可用 http）".into());
    }
    if !same_origin(base, url) {
        return Err("文件地址与服务器不同源，已拒绝下载".into());
    }
    let (response, _) = send("GET", url, None, user, password).map_err(Failure::for_download)?;
    let total: u64 = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if total > MAX_FILE {
        return Err("文件超过 512 MB，暂不支持".into());
    }
    let mut reader = response.into_reader();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建缓存目录失败：{e}"))?;
    }
    let mut file = std::fs::File::create(dest).map_err(|e| format!("写入缓存失败：{e}"))?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let read = reader.read(&mut buffer).map_err(|e| format!("读取数据失败：{e}"))?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > MAX_FILE {
            let _ = std::fs::remove_file(dest);
            return Err("文件超过 512 MB，暂不支持".into());
        }
        file.write_all(&buffer[..read]).map_err(|e| format!("写入缓存失败：{e}"))?;
        if total > 0 {
            progress((written as f32 / total as f32).clamp(0.0, 1.0));
        }
    }
    file.flush().map_err(|e| format!("写入缓存失败：{e}"))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_normalized_and_credentials_are_rejected() {
        // 集合地址一律带尾斜杠：少了会被服务器 301 到带斜杠的地址，跳转会丢凭据。
        assert_eq!(normalize_base("nas.local/music/"), Some("https://nas.local/music/".into()));
        assert_eq!(normalize_base("nas.local/music"), Some("https://nas.local/music/".into()));
        assert_eq!(normalize_base(" https://nas.local "), Some("https://nas.local/".into()));
        assert_eq!(
            normalize_base(" http://127.0.0.1:5005/dav "),
            Some("http://127.0.0.1:5005/dav/".into())
        );
        // 带账号密码的地址直接拒收，避免凭据留在配置文件里。
        assert_eq!(normalize_base("https://user:pw@nas.local/music"), None);
        assert_eq!(normalize_base("ftp://nas.local"), None);
        assert_eq!(normalize_base("   "), None);
    }

    #[test]
    fn redirects_stay_on_the_same_https_origin() {
        // 服务器补尾斜杠：跟。
        assert_eq!(
            next_hop("https://nas.local/music", "/music/").unwrap(),
            "https://nas.local/music/"
        );
        assert_eq!(
            next_hop("https://nas.local/music/", "sub/").unwrap(),
            "https://nas.local/music/sub/"
        );
        // 反代写错成明文 http：拒绝，别把 Basic 凭据送出去。
        let downgrade = next_hop("https://nas.local/music", "http://nas.local/music/").unwrap_err();
        assert!(matches!(downgrade, Failure::Redirect(reason) if reason.contains("http")));
        // 跨站跳转也拒绝。
        assert!(next_hop("https://nas.local/music", "https://evil.example/music/").is_err());
    }

    #[test]
    fn plain_http_is_only_allowed_on_loopback() {
        assert!(resolve("https://nas.local/music/", "https://nas.local/music/a.mp3").is_some());
        assert!(resolve("http://127.0.0.1:5005/dav/", "/dav/a.mp3").is_some());
        assert!(resolve("http://192.168.1.9/dav/", "/dav/a.mp3").is_none(), "明文 http 不能走局域网");
    }

    #[test]
    fn hrefs_resolve_against_the_collection() {
        let base = "https://nas.local/music/";
        assert_eq!(resolve(base, "/music/a.mp3"), Some("https://nas.local/music/a.mp3".into()));
        assert_eq!(resolve(base, "b.mp3"), Some("https://nas.local/music/b.mp3".into()));
        // 服务器不能把我们带到别的域名。
        assert!(resolve(base, "https://evil.example/x.mp3").is_some(), "绝对地址本身合法");
        assert!(!same_origin(base, "https://evil.example/x.mp3"));
    }

    #[test]
    fn audio_extensions_are_case_insensitive() {
        for name in ["a.mp3", "b.FLAC", "c.wav", "d.Ogg", "e.m4a"] {
            assert!(is_audio(name), "{name}");
        }
        for name in ["cover.jpg", "notes.txt", "noext", "x.mp3.part"] {
            assert!(!is_audio(name), "{name}");
        }
    }

    #[test]
    fn propfind_response_is_split_into_sorted_entries() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:href>/music/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/music/Zeppelin/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
  <d:response><d:href>/music/b%20track.mp3</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1024</d:getcontentlength></d:prop></d:propstat></d:response>
  <d:response><d:href>/music/a.mp3</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>2048</d:getcontentlength></d:prop></d:propstat></d:response>
  <d:response><d:href>/music/cover.jpg</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>9</d:getcontentlength></d:prop></d:propstat></d:response>
  <d:response><d:href>https://evil.example/music/x.mp3</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>9</d:getcontentlength></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let entries = parse_entries(xml, "https://nas.local/music/");
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        // 集合自身被跳过；目录在前，其余按名称排序；跨源条目被丢弃。
        assert_eq!(names, ["Zeppelin", "a.mp3", "b track.mp3", "cover.jpg"]);
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].bytes, 2048);
        assert_eq!(entries[2].url, "https://nas.local/music/b%20track.mp3");
        assert!(entries[1].playable());
        assert!(!entries[3].playable(), "封面不是音频");
    }

    #[test]
    fn percent_encoded_names_are_decoded_for_display() {
        assert_eq!(percent_decode("b%20track.mp3"), "b track.mp3");
        assert_eq!(percent_decode("%E5%91%A8%E6%9D%B0%E4%BC%A6.mp3"), "周杰伦.mp3");
        // 非法转义原样保留，不吞字符。
        assert_eq!(percent_decode("100%.mp3"), "100%.mp3");
    }
}
