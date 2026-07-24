//! The bundled dashboard: a trace browser embedded in the server binary.
//!
//! The page is a React app built to the Traza design system; its source
//! lives in `ui/` and `npm run build` (Vite, single-file output) regenerates
//! the checked-in `src/dashboard.html` — never edit that file by hand. The
//! result is still one self-contained HTML document compiled in via
//! `include_str!` and served at `GET /` and `GET /dashboard`, so building
//! the server needs no Node toolchain. It consumes only the public JSON API
//! (spans, traces, sessions, LLM analytics, annotations, payloads, export,
//! flush, stats) — no dashboard-specific endpoints exist. When auth is
//! enabled the SHELL stays open (this module is consulted before the auth
//! gate) while every API call the page makes remains gated; the page prompts
//! for a bearer token on the first 401 and keeps it in `sessionStorage` only.

/// The dashboard HTML document embedded in the server binary.
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// A response the server should write verbatim for a dashboard route.
pub struct DashboardResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers (name, value), content type included.
    pub headers: &'static [(&'static str, &'static str)],
    /// Response body.
    pub body: &'static str,
}

const DASHBOARD_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "text/html; charset=utf-8"),
    ("Cache-Control", "no-store"),
    ("X-Content-Type-Options", "nosniff"),
];

/// The dashboard response for `path`, or `None` when the path is not a
/// dashboard route (the caller falls through to the JSON API).
///
/// Exactly `/`, `/dashboard`, and `/dashboard/` serve the page; deeper
/// `/dashboard/*` paths return `None` so unknown assets 404 through the API
/// handler instead of masquerading as the page.
pub fn route(path: &str) -> Option<DashboardResponse> {
    match path {
        "/" | "/dashboard" | "/dashboard/" => Some(DashboardResponse {
            status: 200,
            headers: DASHBOARD_HEADERS,
            body: DASHBOARD_HTML,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::route;

    #[test]
    fn recognizes_only_dashboard_routes() {
        assert!(route("/").is_some());
        assert!(route("/dashboard").is_some());
        assert!(route("/dashboard/").is_some());
        assert!(route("/dashboard/app.js").is_none());
        assert!(route("/v1/spans").is_none());
        assert!(route("/v1/stats").is_none());
    }

    #[test]
    fn embeds_a_substantive_html_document() {
        let page = route("/").expect("root serves the dashboard").body;
        assert!(page.len() > 4_000, "embedded page looks truncated");
        assert!(page[..30]
            .to_ascii_lowercase()
            .starts_with("<!doctype html>"));
        assert!(page.contains("<title>Traza</title>"));
    }

    #[test]
    fn declares_safe_response_metadata() {
        let response = route("/dashboard").expect("dashboard route serves");
        assert_eq!(response.status, 200);
        let headers: Vec<&str> = response.headers.iter().map(|(name, _)| *name).collect();
        assert!(headers.contains(&"Content-Type"));
        assert!(headers.contains(&"X-Content-Type-Options"));
        assert!(headers.contains(&"Cache-Control"));
    }
}
