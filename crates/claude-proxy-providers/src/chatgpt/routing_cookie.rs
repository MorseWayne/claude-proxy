//! Per-client storage for the Codex backend's infrastructure routing cookie.

use reqwest::Url;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::HeaderValue;

#[derive(Default)]
pub(super) struct RoutingCookieJar {
    jar: Jar,
}

fn allowed_url(url: &Url) -> bool {
    url.scheme() == "https" && matches!(url.host_str(), Some("chatgpt.com" | "chat.openai.com"))
}

impl CookieStore for RoutingCookieJar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        if allowed_url(url) {
            let mut routing = headers.filter(|header| {
                header
                    .to_str()
                    .ok()
                    .and_then(|value| value.split_once('='))
                    .is_some_and(|(name, _)| name.trim() == "__oailb")
            });
            // Jar enforces domain, path, Secure and expiration semantics.
            self.jar.set_cookies(&mut routing, url);
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        if !allowed_url(url) {
            return None;
        }
        self.jar.cookies(url).map(|mut header| {
            header.set_sensitive(true);
            header
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_cookie_respects_scope_expiration_and_client_isolation() {
        let jar = RoutingCookieJar::default();
        let url = Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
        let cookies = [
            HeaderValue::from_static(
                "__oailb=route; Path=/backend-api; Max-Age=3600; Secure; HttpOnly",
            ),
            HeaderValue::from_static("session=secret; Path=/"),
            HeaderValue::from_static("__oailb_other=secret; Path=/"),
        ];
        jar.set_cookies(&mut cookies.iter(), &url);
        let followup = Url::parse("https://chatgpt.com/backend-api/codex/models").unwrap();
        assert_eq!(jar.cookies(&followup).unwrap(), "__oailb=route");
        assert!(jar.cookies(&followup).unwrap().is_sensitive());
        assert!(RoutingCookieJar::default().cookies(&followup).is_none());
        for outside in [
            "http://chatgpt.com/backend-api/codex/responses",
            "https://chatgpt.com/",
            "https://other.chatgpt.com/backend-api/codex/responses",
            "https://chat.openai.com/backend-api/codex/responses",
            "https://api.openai.com/backend-api/codex/responses",
            "https://chatgpt.com.attacker.example/backend-api/codex/responses",
        ] {
            assert!(
                jar.cookies(&Url::parse(outside).unwrap()).is_none(),
                "{outside}"
            );
        }
        let expired = HeaderValue::from_static("__oailb=; Path=/backend-api; Max-Age=0; Secure");
        jar.set_cookies(&mut std::iter::once(&expired), &url);
        assert!(jar.cookies(&url).is_none());
    }

    #[test]
    fn routing_cookie_rejects_untrusted_sources_and_invalid_domains() {
        let jar = RoutingCookieJar::default();
        let cookie = HeaderValue::from_static("__oailb=route; Domain=chatgpt.com; Path=/");
        for source in [
            "http://chatgpt.com/",
            "https://other.chatgpt.com/",
            "https://attacker.example/",
        ] {
            jar.set_cookies(&mut std::iter::once(&cookie), &Url::parse(source).unwrap());
        }
        let url = Url::parse("https://chatgpt.com/").unwrap();
        assert!(jar.cookies(&url).is_none());
        let invalid_domain = HeaderValue::from_static("__oailb=route; Domain=example.com; Path=/");
        jar.set_cookies(&mut std::iter::once(&invalid_domain), &url);
        assert!(jar.cookies(&url).is_none());
    }
}
