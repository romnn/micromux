//! The HTML pages the proxy answers with itself.

use std::fmt::Write as _;
use std::time::Duration;

use micromux::session::GitOverview;

use crate::protocol::RouteTarget;

/// A live session as listed on the not-found page.
pub(crate) struct ListedSession {
    pub(crate) git_metadata: bool,
    pub(crate) name: String,
    /// The session domain every route lives under, such as `luup2.localhost`.
    pub(crate) domain: String,
    pub(crate) config_path: String,
    pub(crate) working_dir: Option<String>,
    /// The micromux release the session runs, or `None` from an older session.
    pub(crate) version: Option<String>,
    pub(crate) registered_for: Duration,
    /// The proxy port the session's routes are served on.
    pub(crate) proxy_port: u16,
    pub(crate) routes: Vec<RouteTarget>,
    pub(crate) git: Option<GitOverview>,
}

/// What the proxy's pages say about the proxy itself.
pub(crate) struct ProxyFacts {
    pub(crate) version: &'static str,
    pub(crate) pid: u32,
    pub(crate) uptime: Duration,
}

/// The URL a route of `session` answers on.
pub(crate) fn route_url(session: &ListedSession, route: &RouteTarget) -> String {
    let (labels, port, path) = route_parts(session, route);
    let labels = labels
        .map(|labels| format!("{labels}."))
        .unwrap_or_default();
    format!("http://{labels}{}{port}{path}", session.domain)
}

/// The parts of a route's URL: its own host labels (none for the catch-all, which serves the
/// session domain itself), the port suffix, and the path.
fn route_parts<'a>(
    session: &ListedSession,
    route: &'a RouteTarget,
) -> (Option<&'a str>, String, &'a str) {
    let labels = (route.host != "*").then_some(route.host.as_str());
    let port = match session.proxy_port {
        80 => String::new(),
        port => format!(":{port}"),
    };
    (labels, port, route.path.as_deref().unwrap_or("/"))
}

/// Whether `host` is `domain` or lies under it.
fn within(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Whether a visitor of `host` came for the overview rather than a route: the host is a loopback
/// address or the parent domain of a session, such as `localhost`.
fn overview_host(host: &str, sessions: &[ListedSession]) -> bool {
    let address = host.trim_start_matches('[').trim_end_matches(']');
    address
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
        || host == micromux::session::DEFAULT_DOMAIN
        || sessions
            .iter()
            .any(|session| session.domain.split_once('.').map(|(_, parent)| parent) == Some(host))
}

/// The page for a hostname and path no route serves.
///
/// Each live session folds out to its routes.
/// The session whose domain contains the host comes first and starts unfolded, as does a lone
/// session.
pub(crate) fn not_found(
    host: &str,
    path: &str,
    sessions: &[ListedSession],
    proxy: &ProxyFacts,
) -> String {
    let routes = sessions.iter().map(|session| session.routes.len()).sum();
    let summary = format!(
        "{} · {}",
        count(sessions.len(), "session"),
        count(routes, "route")
    );
    let mut body = String::from("<header>");
    if path == "/" && overview_host(host, sessions) {
        let _ = write!(body, "<h1>{summary}</h1>");
    } else {
        let _ = write!(
            body,
            "<h1>No route serves <code>{}{}</code></h1><p class=\"muted\">{summary}</p>",
            escape(host),
            escape(path)
        );
    }
    body.push_str(
        "<button id=\"theme\" type=\"button\" hidden title=\"Switch the theme\"></button></header>",
    );
    if sessions.is_empty() {
        body.push_str("<p class=\"muted\">No session is registered with the proxy right now.</p>");
    } else {
        body.push_str(
            "<input id=\"filter\" type=\"search\" hidden autocomplete=\"off\" spellcheck=\"false\" \
             placeholder=\"Filter sessions and routes (press /)\" aria-label=\"Filter sessions and routes\">",
        );
    }

    // The session the host belongs to first, then the rest by name
    let likely = sessions
        .iter()
        .filter(|session| within(host, &session.domain))
        .max_by_key(|session| session.domain.len());
    let ordered = likely.into_iter().chain(
        sessions
            .iter()
            .filter(|session| !likely.is_some_and(|likely| std::ptr::eq(likely, *session))),
    );
    for session in ordered {
        let open =
            sessions.len() == 1 || likely.is_some_and(|likely| std::ptr::eq(likely, session));
        session_block(&mut body, session, open, proxy.version);
    }

    body.push_str(SCRIPT);
    let footer = format!(
        "micromux proxy v{} · pid {} · up {}",
        escape(proxy.version),
        proxy.pid,
        age(proxy.uptime)
    );
    page("404 Not Found: no micromux route", &body, &footer)
}

/// Appends one session's summary line and its folded-out routes.
fn session_block(body: &mut String, session: &ListedSession, open: bool, proxy_version: &str) {
    let open = if open { " open" } else { "" };
    let _ = write!(
        body,
        "<details{open}><summary><span class=\"name\">{}</span>",
        escape(&session.name)
    );
    if let Some(git) = &session.git {
        let _ = write!(body, "<code class=\"muted\">{}</code>", escape(&git.branch));
        if let Some(pull_request) = &git.pull_request {
            let number = pull_request.number;
            if safe_https(&pull_request.url) {
                let _ = write!(
                    body,
                    "<a class=\"pill\" href=\"{}\">PR #{number}</a>",
                    escape(&pull_request.url)
                );
            } else {
                let _ = write!(body, "<span class=\"pill\">PR #{number}</span>");
            }
        }
    }
    match &session.version {
        Some(version) if version == proxy_version => {
            let _ = write!(body, "<span class=\"pill\">v{}</span>", escape(version));
        }
        Some(version) => {
            let relation = match compare(version, proxy_version) {
                Some(std::cmp::Ordering::Less) => " · older than proxy",
                Some(std::cmp::Ordering::Greater) => " · newer than proxy",
                _ => " · differs from proxy",
            };
            let _ = write!(
                body,
                "<span class=\"pill warn\">v{}{relation}</span>",
                escape(version)
            );
        }
        None => body.push_str("<span class=\"pill warn\">version unknown</span>"),
    }
    let _ = write!(
        body,
        "<span class=\"muted meta\">{} · {} ago</span></summary>\
         <p class=\"muted path\">{}</p><table><tbody>",
        count(session.routes.len(), "route"),
        age(session.registered_for),
        escape(&session.config_path)
    );

    // Routes in DNS order, so a host's subdomains stay together, then by path
    let mut routes = session.routes.iter().collect::<Vec<_>>();
    routes.sort_by_cached_key(|route| {
        let labels = (route.host != "*").then(|| route.host.rsplit('.').collect::<Vec<_>>());
        (labels, route.path.clone())
    });
    for route in routes {
        let (labels, port, path) = route_parts(session, route);
        let _ = write!(
            body,
            "<tr><td><a href=\"{}\">",
            escape(&route_url(session, route))
        );
        match labels {
            Some(labels) => {
                let _ = write!(
                    body,
                    "<span class=\"own\">{}</span><span class=\"domain\">.{}{port}</span>",
                    breakable(labels),
                    breakable(&session.domain)
                );
            }
            None => {
                let _ = write!(
                    body,
                    "<span class=\"own\">{}</span><span class=\"domain\">{port}</span>",
                    breakable(&session.domain)
                );
            }
        }
        let _ = write!(
            body,
            "<span class=\"own\">{}</span></a></td>",
            breakable(path)
        );
        let service = route
            .service
            .as_deref()
            .map(|service| format!("{} ", escape(service)))
            .unwrap_or_default();
        let _ = write!(
            body,
            "<td class=\"target\">{service}<span class=\"muted\">:{}</span></td></tr>",
            route.port
        );
    }
    body.push_str("</tbody></table></details>");
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
    let footer = format!("micromux proxy v{}", env!("CARGO_PKG_VERSION"));
    page("502 Bad Gateway: service unavailable", &body, &footer)
}

/// `n` of `noun`, pluralized with an `s`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A duration as its largest whole unit, such as `3h`.
fn age(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Orders two releases by `SemVer` precedence, or `None` when either does not parse.
fn compare(ours: &str, theirs: &str) -> Option<std::cmp::Ordering> {
    let ours = semver::Version::parse(ours).ok()?;
    let theirs = semver::Version::parse(theirs).ok()?;
    Some(ours.cmp_precedence(&theirs))
}

/// Whether `url` is an absolute `https` URL with a host, and nothing that could leave the
/// attribute it is written into even before escaping.
fn safe_https(url: &str) -> bool {
    url.strip_prefix("https://").is_some_and(|rest| {
        !rest.is_empty()
            && !rest.starts_with('/')
            && !rest.contains(|c: char| {
                c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '`')
            })
    })
}

/// Escapes `text` and lets it wrap after each dot and slash.
fn breakable(text: &str) -> String {
    escape(text).replace('.', ".<wbr>").replace('/', "/<wbr>")
}

/// Filters the sessions and routes, follows the chosen theme, and focuses the filter on `/`.
///
/// Without JavaScript the filter and the theme switch stay hidden and the page works as a plain
/// list.
const SCRIPT: &str = r#"<script>
(() => {
  const root = document.documentElement;
  const button = document.getElementById("theme");
  const themes = ["auto", "light", "dark"];
  const labels = { auto: "◐ Auto", light: "☀ Light", dark: "☾ Dark" };
  const show = (theme) => {
    if (theme === "auto") delete root.dataset.theme; else root.dataset.theme = theme;
    button.textContent = labels[theme];
  };
  let theme = localStorage.getItem("micromux-theme");
  if (!themes.includes(theme)) theme = "auto";
  show(theme);
  button.hidden = false;
  button.addEventListener("click", () => {
    theme = themes[(themes.indexOf(theme) + 1) % themes.length];
    localStorage.setItem("micromux-theme", theme);
    show(theme);
  });

  const filter = document.getElementById("filter");
  if (!filter) return;
  filter.hidden = false;
  const sessions = [...document.querySelectorAll("details")];
  const unfolded = sessions.map((session) => session.open);
  filter.addEventListener("input", () => {
    const query = filter.value.trim().toLowerCase();
    sessions.forEach((session, index) => {
      const rows = [...session.querySelectorAll("tr")];
      if (!query) {
        session.hidden = false;
        session.open = unfolded[index];
        rows.forEach((row) => (row.hidden = false));
        return;
      }
      const about = session.querySelector("summary").textContent + " " +
        session.querySelector(".path").textContent;
      const named = about.toLowerCase().includes(query);
      const matching = rows.filter((row) => row.textContent.toLowerCase().includes(query));
      session.hidden = !named && matching.length === 0;
      session.open = !session.hidden;
      rows.forEach((row) => (row.hidden = matching.length > 0 && !matching.includes(row)));
    });
  });
  filter.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      const link = document.querySelector("details:not([hidden]) tr:not([hidden]) a");
      if (link) location.href = link.href;
    } else if (event.key === "Escape") {
      filter.value = "";
      filter.dispatchEvent(new Event("input"));
    }
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "/" && document.activeElement !== filter) {
      event.preventDefault();
      filter.focus();
    }
  });
})();
</script>"#;

/// The page's style: light or dark as the browser prefers, unless the theme switch chose one.
const STYLE: &str = "\
    :root{color-scheme:light;--fg:#1f2328;--muted:#656d76;--line:#d0d7de;--bg:#fff;\
    --hover:#f6f8fa;--link:#0969da;--warn:#9a6700}\
    @media (prefers-color-scheme:dark){:root:not([data-theme=light]){color-scheme:dark;\
    --fg:#e6edf3;--muted:#8d96a0;--line:#30363d;--bg:#0d1117;--hover:#161b22;--link:#4493f8;\
    --warn:#d29922}}\
    :root[data-theme=dark]{color-scheme:dark;--fg:#e6edf3;--muted:#8d96a0;--line:#30363d;\
    --bg:#0d1117;--hover:#161b22;--link:#4493f8;--warn:#d29922}\
    html{scrollbar-gutter:stable}\
    body{font:15px/1.5 system-ui,sans-serif;max-width:56rem;margin:3rem auto;padding:0 1rem;\
    color:var(--fg);background:var(--bg);overflow-wrap:anywhere}\
    header{display:flex;align-items:baseline;gap:.5rem 1rem;flex-wrap:wrap;margin:0 0 1rem}\
    header p{margin:0}\
    h1{font-size:1.1rem;font-weight:600;margin:0}\
    #theme{margin-left:auto;font:inherit;font-size:.8rem;color:var(--muted);background:none;\
    border:1px solid var(--line);border-radius:999px;padding:.1rem .6rem;cursor:pointer}\
    #theme:hover{background:var(--hover)}\
    #filter{display:block;box-sizing:border-box;width:100%;margin:0 0 .75rem;font:inherit;\
    color:inherit;background:var(--bg);border:1px solid var(--line);border-radius:8px;\
    padding:.45rem .8rem}\
    #filter[hidden],#theme[hidden]{display:none}\
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
    .pill{font-size:.75rem;border:1px solid var(--line);border-radius:999px;padding:0 .5rem;\
    color:var(--fg)}\
    a.pill:hover{border-color:var(--link);text-decoration:none}\
    .pill.warn{color:var(--warn);border-color:var(--warn)}\
    .meta{margin-left:auto;font-size:.85rem}\
    .path{margin:0;padding:0 .9rem 0 2.1rem;font-size:.8rem}\
    table{border-collapse:collapse;width:100%;margin:.3rem 0 .7rem}\
    td{padding:.1rem 0;vertical-align:baseline;\
    font:.85rem/1.6 ui-monospace,SFMono-Regular,Menlo,monospace}\
    td:first-child{padding-left:2.1rem;overflow-wrap:break-word}\
    td.target{padding:0 .9rem 0 1rem;text-align:right;white-space:nowrap;color:var(--fg)}\
    tr[hidden]{display:none}\
    tr:hover td{background:var(--hover)}\
    .domain{color:var(--muted)}\
    a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}\
    footer{margin-top:2rem;font-size:.8rem}\
    @media (max-width:40rem){tr{display:flex;flex-wrap:wrap}td:first-child{flex:1 1 100%}\
    td.target{padding:0 .9rem .3rem 3.1rem;text-align:left}}";

/// A small route icon for the browser tab, so the browser asks for no `/favicon.ico`.
const ICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' \
    viewBox='0 0 16 16'%3E%3Crect width='16' height='16' rx='4' fill='%230969da'/%3E\
    %3Cpath d='M4 5h3l2 3-2 3H4m5-3h3' fill='none' stroke='white' stroke-width='1.6' \
    stroke-linecap='round' stroke-linejoin='round'/%3E%3C/svg%3E";

fn page(title: &str, body: &str, footer: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <link rel=\"icon\" href=\"{ICON}\">\
         <title>{title}</title><style>{STYLE}</style></head><body>{body}\
         <footer class=\"muted\">{footer}</footer></body></html>"
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
    use micromux::session::PullRequest;
    use similar_asserts::assert_eq;

    use super::*;

    const PROXY: ProxyFacts = ProxyFacts {
        version: "0.5.2",
        pid: 4242,
        uptime: Duration::from_hours(3),
    };

    fn route(host: &str, path: Option<&str>, port: u16) -> RouteTarget {
        RouteTarget {
            host: host.to_string(),
            path: path.map(ToString::to_string),
            strip_prefix: false,
            port,
            service: Some("web".to_string()),
        }
    }

    fn session(name: &str, version: Option<&str>) -> ListedSession {
        ListedSession {
            git_metadata: true,
            name: name.to_string(),
            domain: format!("{name}.localhost"),
            config_path: "/project/micromux.yaml".to_string(),
            working_dir: None,
            version: version.map(ToString::to_string),
            registered_for: Duration::from_secs(120),
            proxy_port: 80,
            routes: vec![route("app", None, 20_001)],
            git: None,
        }
    }

    /// A hostile Host header cannot inject markup into the page.
    #[test]
    fn pages_escape_untrusted_text() {
        let page = not_found("<script>x</script>", "/<b>", &[], &PROXY);
        assert!(!page.contains("<script>x"));
        assert!(
            page.contains("No route serves <code>&lt;script&gt;x&lt;/script&gt;/&lt;b&gt;</code>")
        );
        assert!(page.contains("micromux proxy v0.5.2 · pid 4242 · up 3h"));
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

    /// A session line shows its escaped branch, a linked pull request, its version against the
    /// proxy's, its age, and its routes sorted by host labels from the right, then by path.
    #[test]
    fn a_session_lists_its_context_and_sorted_routes() {
        let mut listed = session("loop1", Some("0.5.1"));
        listed.git = Some(GitOverview {
            branch: "feature/<script>&\"".to_string(),
            pull_request: Some(PullRequest {
                number: 77,
                url: "https://github.com/owner/repo/pull/77".to_string(),
            }),
        });
        listed.proxy_port = 8080;
        listed.routes = vec![
            route("api.app", None, 20_003),
            route("app", Some("/rest/"), 20_002),
            route("app", None, 20_001),
            route("*", None, 20_000),
        ];
        let page = not_found("localhost", "/", &[listed], &PROXY);

        assert!(page.contains("<code class=\"muted\">feature/&lt;script&gt;&amp;&quot;</code>"));
        assert!(page.contains(
            "<a class=\"pill\" href=\"https://github.com/owner/repo/pull/77\">PR #77</a>"
        ));
        assert!(page.contains("v0.5.1 · older than proxy"));
        assert!(page.contains("4 routes · 2m ago"));
        assert!(page.contains("web <span class=\"muted\">:20003</span>"));

        // The catch-all first, then `app` with its paths, then `api.app` under it
        let hrefs = page
            .split("<tr><td><a href=\"")
            .skip(1)
            .filter_map(|row| row.split('"').next())
            .collect::<Vec<_>>();
        assert_eq!(
            hrefs,
            [
                "http://loop1.localhost:8080/",
                "http://app.loop1.localhost:8080/",
                "http://app.loop1.localhost:8080/rest/",
                "http://api.app.loop1.localhost:8080/",
            ]
        );
    }

    #[test]
    fn versions_are_marked_against_the_proxy() {
        let page = not_found(
            "localhost",
            "/",
            &[
                session("same", Some("0.5.2")),
                session("newer", Some("0.6.0")),
                session("odd", Some("dev")),
                session("old", None),
            ],
            &PROXY,
        );
        assert!(page.contains("<span class=\"pill\">v0.5.2</span>"));
        assert!(page.contains("v0.6.0 · newer than proxy"));
        assert!(page.contains("vdev · differs from proxy"));
        assert!(page.contains("<span class=\"pill warn\">version unknown</span>"));
    }

    /// A pull request URL that is not plain `https` stays unlinked.
    #[test]
    fn only_https_pull_requests_are_linked() {
        for url in [
            "javascript:alert(1)",
            "http://example.com/1",
            "https:///x",
            "https://a\" b",
        ] {
            assert!(!safe_https(url), "{url}");
        }
        assert!(safe_https("https://github.com/owner/repo/pull/1"));

        let mut listed = session("loop1", Some("0.5.2"));
        listed.git = Some(GitOverview {
            branch: "main".to_string(),
            pull_request: Some(PullRequest {
                number: 5,
                url: "javascript:alert(1)".to_string(),
            }),
        });
        let page = not_found("localhost", "/", &[listed], &PROXY);
        assert!(page.contains("<span class=\"pill\">PR #5</span>"));
        assert!(!page.contains("javascript:"));
    }

    /// The bare proxy host gets the overview heading; a host inside a session's domain lists that
    /// session first and unfolded; others start folded.
    #[test]
    fn the_host_picks_the_heading_and_the_unfolded_session() {
        let sessions = [session("loop1", None), session("loop2", None)];
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let page = not_found(host, "/", &sessions, &PROXY);
            assert!(page.contains("<h1>2 sessions · 2 routes</h1>"), "{host}");
            assert!(!page.contains("<details open>"), "{host}");
        }

        let page = not_found("typo.loop2.localhost", "/", &sessions, &PROXY);
        assert!(page.contains("No route serves <code>typo.loop2.localhost/</code>"));
        let first = page.find("<details").unwrap_or_default();
        assert!(
            page[first..].starts_with("<details open><summary><span class=\"name\">loop2</span>")
        );
        assert_eq!(page.matches("<details open>").count(), 1);

        // A lone session starts unfolded
        let page = not_found("localhost", "/", &sessions[..1], &PROXY);
        assert!(page.contains("<details open>"));
    }

    #[test]
    fn ages_use_their_largest_whole_unit() {
        assert_eq!(age(Duration::from_secs(59)), "59s");
        assert_eq!(age(Duration::from_secs(60)), "1m");
        assert_eq!(age(Duration::from_secs(7_199)), "1h");
        assert_eq!(age(Duration::from_hours(48)), "2d");
    }
}
