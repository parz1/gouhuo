// SPDX-License-Identifier: MPL-2.0

//! 浏览器加入页。只发布地址和证书指纹，加入码始终留在浏览器里。
//! HTTP 监听供本机 HTTPS 反向代理使用，不复用语音控制端口。
//!
//! 同一份公开信息有两个读者：
//!
//! - **人**：`/` 是给浏览器看的页面，点按钮唤起客户端
//! - **客户端**：[`DISCOVERY_PATH`] 是给客户端读的说明。用户在客户端里只输域名，
//!   客户端来这儿取地址、端口和证书指纹（格式见 `protocol::Discovery`）
//!
//! 两边都**不含加入码**。

use std::io;
use std::net::SocketAddr;

use protocol::{Discovery, Invite, DISCOVERY_PATH};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub struct JoinPage {
    html: Vec<u8>,
    discovery: Vec<u8>,
}

impl JoinPage {
    pub fn new(invite: &Invite, name: &str) -> io::Result<Self> {
        let public = Invite {
            code: None,
            ..invite.clone()
        };
        let encoded = public.to_code().map_err(io::Error::other)?;
        let bytes = protocol::base32::decode(&encoded).map_err(io::Error::other)?;
        let payload: String = bytes[..bytes.len() - 2]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let address = if invite.port == protocol::DEFAULT_PORT {
            invite.host.clone()
        } else {
            format!("{}:{}", invite.host, invite.port)
        };
        let discovery = Discovery::public(invite, name)
            .to_text()
            .map_err(io::Error::other)?;
        // 每个替换值只插入一次，不把配置中的文本重新解释成模板。
        let mut html = String::new();
        for (index, part) in include_str!("../web/index.html").split("@@").enumerate() {
            if index % 2 == 0 {
                html.push_str(part);
            } else {
                html.push_str(&match part {
                    "NAME" => escape_html(name),
                    "ADDRESS" => escape_html(&address),
                    "PAYLOAD" => payload.clone(),
                    "PRIVATE" => invite.code.is_some().to_string(),
                    "CODE_HIDDEN" => if invite.code.is_some() { "" } else { "hidden" }.into(),
                    _ => return Err(io::Error::other("未知加入页模板字段")),
                });
            }
        }
        Ok(Self {
            html: html.into_bytes(),
            discovery: discovery.into_bytes(),
        })
    }

    fn resource(&self, path: &str) -> Option<(&'static str, &[u8])> {
        match path {
            "/" | "/join" | "/join/" => Some(("text/html; charset=utf-8", &self.html)),
            DISCOVERY_PATH => Some(("text/plain; charset=utf-8", &self.discovery)),
            "/join.css" => Some(("text/css; charset=utf-8", include_bytes!("../web/join.css"))),
            "/join.mjs" => Some((
                "text/javascript; charset=utf-8",
                include_bytes!("../web/join.mjs"),
            )),
            "/fire.mjs" => Some((
                "text/javascript; charset=utf-8",
                include_bytes!("../web/fire.mjs"),
            )),
            "/flame.svg" => Some(("image/svg+xml", include_bytes!("../web/flame.svg"))),
            _ => None,
        }
    }
}

/// 先绑定后起线程：端口占用必须在启动时暴露出来。
pub fn spawn(address: SocketAddr, page: JoinPage) -> io::Result<()> {
    let server = Server::http(address).map_err(io::Error::other)?;
    std::thread::Builder::new().name("gouhuo-web".into()).spawn(move || {
        for request in server.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("");
            let (status, mime, body) = if !matches!(request.method(), Method::Get | Method::Head) {
                (405, "text/plain; charset=utf-8", &b"Method not allowed"[..])
            } else if let Some((mime, body)) = page.resource(path) {
                (200, mime, body)
            } else {
                (404, "text/plain; charset=utf-8", &b"Not found"[..])
            };
            let mut response = Response::from_data(body).with_status_code(StatusCode(status));
            for (name, value) in [
                ("Content-Type", mime),
                ("Cache-Control", "no-store"),
                ("Referrer-Policy", "no-referrer"),
                ("X-Content-Type-Options", "nosniff"),
                ("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
            ] {
                response.add_header(Header::from_bytes(name, value).expect("静态 HTTP 响应头"));
            }
            if status == 405 {
                response.add_header(Header::from_bytes("Allow", "GET, HEAD").unwrap());
            }
            let _ = request.respond(response);
        }
    })?;
    Ok(())
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 私人加入码放在 fragment：不会出现在 HTTP 请求或代理访问日志里。
pub fn browser_invite(base: &str, code: Option<&str>) -> io::Result<String> {
    if !base.starts_with("https://")
        || base[8..].is_empty()
        || base[8..].starts_with('/')
        || base
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '@'))
    {
        return Err(io::Error::other(
            "GOUHUO_JOIN_URL 必须是无查询参数的 HTTPS 加入页地址",
        ));
    }
    let mut url = base.to_string();
    if let Some(code) = code {
        url.push_str("#code=");
        for byte in code.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                url.push(byte as char);
            } else {
                url.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Fingerprint;

    fn invite() -> Invite {
        Invite {
            host: "voice.example.com".into(),
            port: 20800,
            cert: Fingerprint([7; 16]),
            code: Some("private-secret".into()),
        }
    }

    #[test]
    fn public_page_never_contains_join_credentials_and_escapes_config() {
        let page = JoinPage::new(&invite(), "<script>@@ADDRESS@@&\"").unwrap();
        let html = String::from_utf8(page.html.clone()).unwrap();
        assert!(!html.contains("private-secret"));
        assert!(html.contains("&lt;script&gt;@@ADDRESS@@&amp;&quot;"));
        let payload = html
            .split("data-payload=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let mut bytes: Vec<u8> = (0..payload.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&payload[i..i + 2], 16).unwrap())
            .collect();
        let encoded = Invite {
            code: None,
            ..invite()
        }
        .to_code()
        .unwrap();
        let expected = protocol::base32::decode(&encoded).unwrap();
        bytes.extend_from_slice(&expected[expected.len() - 2..]);
        assert_eq!(bytes, expected);
        assert!(page.resource("/join/").is_some());
        assert!(page.resource("/../invite-code.txt").is_none());
    }

    /// 客户端只输域名时读的那份说明：地址和指纹给，加入码不给。
    #[test]
    fn discovery_tells_clients_where_to_connect_but_not_the_code() {
        let page = JoinPage::new(&invite(), "周末开黑").unwrap();
        let (mime, body) = page.resource(DISCOVERY_PATH).unwrap();
        assert!(mime.starts_with("text/plain"));
        let text = std::str::from_utf8(body).unwrap();
        assert!(!text.contains("private-secret"));

        let discovery = Discovery::parse(text).unwrap();
        assert_eq!(
            discovery.invite,
            Invite {
                code: None,
                ..invite()
            }
        );
        assert_eq!(discovery.name, "周末开黑");
        assert!(discovery.private, "有加入码的服务器要告诉客户端去问用户要");

        let open = Invite {
            code: None,
            ..invite()
        };
        let page = JoinPage::new(&open, "公开").unwrap();
        let (_, body) = page.resource(DISCOVERY_PATH).unwrap();
        assert!(
            !Discovery::parse(std::str::from_utf8(body).unwrap())
                .unwrap()
                .private
        );
    }

    #[test]
    fn browser_invites_keep_credentials_in_encoded_fragment() {
        assert_eq!(
            browser_invite("https://voice.example.com/", Some("a+中 &")).unwrap(),
            "https://voice.example.com/#code=a%2B%E4%B8%AD%20%26"
        );
        assert_eq!(
            browser_invite("https://voice.example.com/", None).unwrap(),
            "https://voice.example.com/"
        );
        for bad in [
            "http://voice.example.com/",
            "https://",
            "https:///join",
            "https://host/#code=x",
            "https://host/?x=y",
            "https://user@host",
            "https://host\n",
        ] {
            assert!(browser_invite(bad, None).is_err());
        }
    }

    #[test]
    fn javascript_invitation_vectors_match_the_rust_protocol() {
        let public = Invite {
            code: None,
            ..invite()
        };
        assert_eq!(
            public.to_url().unwrap(),
            "gouhuo://j/0400e1r70w3ge1r70w3ge1r70w3gema025v6ytb3cmq6ay31dnr6rs9ecdqpt1v8"
        );
        let private = Invite {
            code: Some("开黑+朋友".into()),
            ..public
        };
        let browser_link = "gouhuo://j/040ge1r70w3ge1r70w3ge1r70w3gema025v6ytb3cmq6ay31dnr6rs9ecdqpt3f5qj0ekewh5fk9s2z5hy5ry3r";
        assert_eq!(private.to_url().unwrap(), browser_link);
        assert_eq!(Invite::parse(browser_link).unwrap(), private);
    }
}
