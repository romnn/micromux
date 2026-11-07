//! The HTML pages the proxy answers with itself.

use std::fmt::Write as _;

/// A live session as listed on the not-found page.
pub(crate) struct ListedSession {
    pub(crate) name: String,
    pub(crate) config_path: String,
    pub(crate) urls: Vec<String>,
}

/// The page for a hostname and path no route serves.
pub(crate) fn not_found(host: &str, path: &str, sessions: &[ListedSession]) -> String {
    let mut body = format!(
        "<p>No micromux route serves <code>{}{}</code>.</p>",
        escape(host),
        escape(path)
    );
    if sessions.is_empty() {
        body.push_str("<p>No session is registered with the proxy right now.</p>");
    } else {
        body.push_str("<p>Live sessions:</p><ul>");
        for session in sessions {
            let _ = write!(
                body,
                "<li><strong>{}</strong> <small>({})</small><ul>",
                escape(&session.name),
                escape(&session.config_path)
            );
            for url in &session.urls {
                let url = escape(url);
                let _ = write!(body, "<li><a href=\"{url}\">{url}</a></li>");
            }
            body.push_str("</ul></li>");
        }
        body.push_str("</ul>");
    }
    page("404 Not Found: no micromux route", &body)
}

/// The page for a route whose upstream does not accept connections.
///
/// `route` names the route as its host followed by its path prefix, if it has one.
pub(crate) fn bad_gateway(
    session: &str,
    service: Option<&str>,
    route: &str,
    port: u16,
    error: &str,
) -> String {
    let target = match service {
        Some(service) => format!(
            "service <strong>{}</strong> of session <strong>{}</strong>",
            escape(service),
            escape(session)
        ),
        None => format!("session <strong>{}</strong>", escape(session)),
    };
    let body = format!(
        "<p><code>{}</code> is routed to {target} on <code>127.0.0.1:{port}</code>, but nothing \
         accepted the connection: {}.</p><p>The service may still be starting, or it has \
         stopped. Check it in micromux.</p>",
        escape(route),
        escape(error),
    );
    page("502 Bad Gateway: service unavailable", &body)
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:48rem;margin:3rem auto;\
         padding:0 1rem;line-height:1.5}}code{{background:#8882;padding:0 .2em}}</style>\
         </head><body><h1>{title}</h1>{body}<hr><small>micromux proxy</small></body></html>"
    )
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hostile Host header cannot inject markup into the page.
    #[test]
    fn pages_escape_untrusted_text() {
        let page = not_found("<script>x</script>", "/<b>", &[]);
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(page.contains("/&lt;b&gt;"));
    }

    #[test]
    fn bad_gateway_names_the_session_and_service() {
        let page = bad_gateway(
            "luup2",
            Some("api"),
            "app.luup2.localhost",
            20001,
            "refused",
        );
        assert!(page.contains("<strong>api</strong>"));
        assert!(page.contains("<strong>luup2</strong>"));
        assert!(page.contains("127.0.0.1:20001"));
    }
}
