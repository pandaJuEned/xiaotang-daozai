//! 从 NOS 拉取 WebDAV 连接配置并落到本机。
//!
//! 配置入口只有一处：NOS Web「我的」页。各端连接 NOS 后自动拉取地址、用户名
//! 与密码，写入本机 `webdav.json` 与凭据管理器；岛仔侧不再需要手填，也不把
//! 这三项当成可同步账本设置（它们是机器私有的落地结果）。
//!
//! 拉取结果通过两种方式回到界面：
//! - `fetch` + `apply`：给「测试连接」这类一次性动作用；
//! - `Service`：随课表服务存在，成功拉到课表时顺带拉一次配置，失败静默退避。

use std::io::Read;

use serde::{Deserialize, Serialize};

/// 服务端 `/api/daozai/webdav-config` 的响应。字段都带默认值：
/// 老服务端没有该接口时返回 404，解析失败也不会 panic。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub configured: bool,
    pub url: String,
    pub user: String,
    pub password: String,
    pub has_password: bool,
    pub updated_at: String,
}

/// 请求地址：站点根 + 固定路径，与 `nos::agenda_url` 同一套约定。
pub fn config_url(base: &str) -> Result<url::Url, String> {
    url::Url::parse(base.trim())
        .map_err(|_| "服务器地址无效".to_string())?
        .join("/api/daozai/webdav-config")
        .map_err(|_| "服务器地址无效".to_string())
}

/// 拉取一次配置。`token` 是不含 "Bearer " 前缀的 PAT。
pub fn fetch(agent: &ureq::Agent, base: &str, token: &str) -> Result<RemoteConfig, String> {
    let url = config_url(base)?;
    let response = agent
        .get(url.as_str())
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {}", token.trim()))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(401, _) => "令牌无效或已被撤销，请在设置里重新生成".to_string(),
            ureq::Error::Status(403, _) => "令牌缺少读取权限（需要 sync:read 或 agenda:read）".to_string(),
            ureq::Error::Status(404, _) => "服务端没有 WebDAV 配置接口，请升级 NOS".to_string(),
            ureq::Error::Status(503, _) => "服务端未配置访问令牌".to_string(),
            ureq::Error::Status(code, _) => format!("服务返回 {code}"),
            _ => "网络不可用".to_string(),
        })?;
    let mut body = String::new();
    response
        .into_reader()
        .take(1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|error| format!("读取响应失败：{error}"))?;
    serde_json::from_str(&body).map_err(|error| format!("服务返回的数据无法解析：{error}"))
}

/// 把远端配置落到本机：校验地址 → 写 `webdav.json` → 写凭据管理器。
/// 服务端未配置时返回 `false`，不动本机已有配置（离线也能继续用上次的连接）。
pub fn apply(remote: &RemoteConfig) -> Result<bool, String> {
    if !remote.configured {
        return Ok(false);
    }
    let url = crate::webdav::normalize_base(&remote.url)
        .ok_or_else(|| "NOS 上的 WebDAV 地址无效".to_string())?;
    // 服务端已校验过，这里再挡一次：岛仔可能连的是旧版或配置被手工改过的服务端，
    // 公网明文 http 会把凭据送给任何人，不能因为"来自 NOS"就放过。
    {
        let parsed = url::Url::parse(&url).map_err(|_| "NOS 上的 WebDAV 地址无效".to_string())?;
        let loopback = parsed.scheme() == "http"
            && parsed.host_str().is_some_and(|host| {
                host == "localhost"
                    || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
            });
        if parsed.scheme() != "https" && !loopback {
            return Err("NOS 下发的 WebDAV 地址不是 https（本机回环除外），已拒绝".to_string());
        }
    }
    let user = remote.user.trim().to_string();
    if user.is_empty() {
        return Err("NOS 上的 WebDAV 用户名为空".to_string());
    }
    let old = crate::webdav::Config::load();
    // 同一版本、同一地址账号就不重复写盘与凭据管理器（自动拉取每 5 分钟一次）。
    if old.from_nos
        && old.from_nos_at == remote.updated_at
        && old.url == url
        && old.user == user
    {
        return Ok(false);
    }
    // 换服务器时旧目录属于另一个源，清掉；同源则保留浏览位置。先算好再移动 url。
    let dir = if old.dir.starts_with(&url) { old.dir.clone() } else { String::new() };
    let cfg = crate::webdav::Config {
        url,
        user,
        dir,
        from_nos: true,
        from_nos_at: remote.updated_at.clone(),
    };
    cfg.save().map_err(|error| format!("保存 WebDAV 配置失败：{error}"))?;
    if remote.password.is_empty() {
        crate::webdav::delete_password(&cfg);
    } else {
        crate::webdav::save_password(&cfg, &remote.password)?;
    }
    Ok(true)
}

/// 一次性拉取并落地。返回给界面显示的提示。
pub fn pull(agent: &ureq::Agent, base: &str, token: &str) -> Result<String, String> {
    let remote = fetch(agent, base, token)?;
    if !remote.configured {
        return Ok("NOS 上还没有配置 WebDAV 音乐，请到 NOS Web「我的」页填写".to_string());
    }
    let changed = apply(&remote)?;
    Ok(if changed {
        format!("已从 NOS 获取 WebDAV 连接：{} · {}", remote.url, remote.user)
    } else {
        "NOS 上的 WebDAV 配置未变更".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_url_lands_on_the_dedicated_endpoint() {
        let url = config_url("https://nos.example.com/").unwrap();
        assert_eq!(url.as_str(), "https://nos.example.com/api/daozai/webdav-config");
    }

    #[test]
    fn remote_config_defaults_when_server_is_old() {
        let cfg: RemoteConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, RemoteConfig::default());
        assert!(!cfg.configured);
    }

    #[test]
    fn remote_config_parses_full_payload() {
        let raw = r#"{"configured":true,"url":"https://webmusic.redbrick.asia/music/",
            "user":"pandaju","password":"pw","has_password":true,"updated_at":"2026-11-20T21:00:00"}"#;
        let cfg: RemoteConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.configured);
        assert_eq!(cfg.password, "pw");
        assert_eq!(cfg.updated_at, "2026-11-20T21:00:00");
    }

    #[test]
    fn empty_remote_does_not_touch_local_config() {
        let remote = RemoteConfig::default();
        // 不访问磁盘凭据就能验证早退分支。
        assert!(!apply(&remote).unwrap());
    }

    #[test]
    fn invalid_remote_url_is_rejected() {
        let remote = RemoteConfig {
            configured: true,
            url: "http://public.example.com/music".into(),
            user: "u".into(),
            password: "p".into(),
            ..Default::default()
        };
        assert!(apply(&remote).is_err());
    }
}
