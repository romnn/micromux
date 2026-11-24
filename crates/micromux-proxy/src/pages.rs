//! The HTML pages the proxy answers with itself.

use std::fmt::Write as _;

/// A live session as listed on the not-found page.
pub(crate) struct ListedSession {
    pub(crate) git_metadata: bool,
    pub(crate) name: String,
    pub(crate) config_path: String,
    pub(crate) urls: Vec<String>,
    pub(crate) working_dir: Option<String>,
    pub(crate) git: Option<micromux::session::GitContext>,
}

/// The page for a hostname and path no route serves.
///
/// Each live session folds out to its routes, and a lone session starts unfolded.
pub(crate) fn not_found(host: &str, path: &str, sessions: &[ListedSession]) -> String {
    let mut body = format!(
        "<h1>No route serves <code>{}{}</code></h1>",
        escape(host),
        escape(path)
    );
    if sessions.is_empty() {
        body.push_str("<p class=\"muted\">No session is registered with the proxy right now.</p>");
    }
    for session in sessions {
        let open = if sessions.len() == 1 { " open" } else { "" };
        let _ = write!(
            body,
            "<details{open}><summary><span class=\"name\">{}</span>",
            escape(&session.name)
        );
        if let Some(git) = &session.git {
            let _ = write!(body, "<code class=\"muted\">{}</code>", escape(&git.branch));
            if let Some(number) = git.pull_request {
                let _ = write!(body, "<span class=\"pill\">PR #{number}</span>");
            }
        }
        let routes = match session.urls.len() {
            1 => "1 route".to_string(),
            count => format!("{count} routes"),
        };
        let _ = write!(
            body,
            "<span class=\"muted count\">{routes}</span></summary>\
             <p class=\"muted path\">{}</p><ul>",
            escape(&session.config_path)
        );
        for url in &session.urls {
            // The scheme is the same on every link, so only the target shows.
            let shown = url.strip_prefix("http://").unwrap_or(url);
            let _ = write!(
                body,
                "<li><a href=\"{}\">{}</a></li>",
                escape(url),
                escape(shown)
            );
        }
        body.push_str("</ul></details>");
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
        "<h1>Service unavailable</h1><p><code>{}</code> is routed to {target} on <code>127.0.0.1:{port}</code>, but nothing \
         accepted the connection: {}.</p><p>The service may still be starting, or it has \
         stopped. Check it in micromux.</p>",
        escape(route),
        escape(error),
    );
    page("502 Bad Gateway: service unavailable", &body)
}

/// The page's style, light or dark as the browser prefers.
const STYLE: &str = "\
    :root{color-scheme:light dark;--fg:#1f2328;--muted:#656d76;--line:#d0d7de;--bg:#fff;\
    --hover:#f6f8fa;--link:#0969da}\
    @media (prefers-color-scheme:dark){:root{--fg:#e6edf3;--muted:#8d96a0;--line:#30363d;\
    --bg:#0d1117;--hover:#161b22;--link:#4493f8}}\
    body{font:15px/1.5 system-ui,sans-serif;max-width:52rem;margin:3rem auto;padding:0 1rem;\
    color:var(--fg);background:var(--bg);overflow-wrap:anywhere}\
    h1{font-size:1.1rem;font-weight:600;margin:0 0 1.5rem}\
    code{font:.85rem ui-monospace,SFMono-Regular,Menlo,monospace}\
    .muted{color:var(--muted)}\
    details{border:1px solid var(--line);border-radius:8px;margin:.5rem 0;overflow:hidden}\
    summary{display:flex;flex-wrap:wrap;align-items:baseline;gap:.25rem .75rem;\
    padding:.6rem .9rem;cursor:pointer;list-style:none}\
    summary::-webkit-details-marker{display:none}\
    summary::before{content:\"\u{203a}\";color:var(--muted);transition:transform .15s}\
    details[open] summary::before{transform:rotate(90deg)}\
    summary:hover{background:var(--hover)}\
    .name{font-weight:600}\
    .pill{font-size:.75rem;border:1px solid var(--line);border-radius:999px;padding:0 .5rem}\
    .count{margin-left:auto;font-size:.85rem}\
    .path{margin:0;padding:0 .9rem 0 2.1rem;font-size:.8rem}\
    ul{list-style:none;margin:0;padding:.4rem .9rem .8rem 2.1rem}\
    li{font:.85rem/1.8 ui-monospace,SFMono-Regular,Menlo,monospace}\
    a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}\
    footer{margin-top:2rem;font-size:.8rem}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style></head><body>{body}\
         <footer class=\"muted\">micromux proxy</footer></body></html>"
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
    use similar_asserts::assert_eq;

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

    #[test]
    fn session_context_is_optional_and_escaped() {
        let session = || ListedSession {
            git_metadata: true,
            name: "loop1".to_string(),
            config_path: "/project/micromux.yaml".to_string(),
            urls: vec!["http://app.loop1.localhost/".to_string()],
            working_dir: None,
            git: Some(micromux::session::GitContext {
                branch: "feature/<script>&\"".to_string(),
                pull_request: Some(77),
            }),
        };
        let page = not_found("localhost", "/", &[session()]);
        assert!(page.contains("<span class=\"name\">loop1</span>"));
        assert!(page.contains("feature/&lt;script&gt;&amp;&quot;"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("PR #77"));
        assert!(page.contains("<a href=\"http://app.loop1.localhost/\">app.loop1.localhost/</a>"));

        // A lone session shows its routes; several start folded to their summaries.
        assert!(page.contains("<details open>"));
        let page = not_found("localhost", "/", &[session(), session()]);
        assert!(!page.contains("<details open>"));
        assert_eq!(page.matches("<details>").count(), 2);
    }
}
