//! The phone app's files on the tailnet router: resolving the folder, the
//! fallback that serves it, the headers every answer carries, and the `503`
//! while no folder is installed.

use super::remote::TailnetState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tower::ServiceExt as _;
use tower_http::services::{ServeDir, ServeFile};
use tracing::warn;

/// The variable that overrides the folder, for tests and dev runs.
pub(super) const PWA_DIR_ENV: &str = "RUSTLING_TULIP_PWA_DIR";

/// The body of every static answer while no folder is installed.
const NOT_INSTALLED: &str = "The phone app is not installed with this daemon.";

/// Only same-origin scripts, styles (plus inline styles, below), images and
/// connections, so the page can never load code from, or talk to, another
/// origin. `connect-src 'self'`
/// covers the same-origin `wss://` socket a backgrounded page reopens.
/// Inline styles are allowed because the phone terminal's xterm.js writes its
/// own `<style>` rules at runtime; scripts stay same-origin only.
const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; ",
    "img-src 'self' data:; font-src 'self'; connect-src 'self'; ",
    "manifest-src 'self'; worker-src 'self'; base-uri 'none'; ",
    "frame-ancestors 'none'; form-action 'none'",
);

/// A hashed build asset never changes under its name.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Everything else is revalidated, so a stale page or service worker never
/// pins an old build.
const NO_CACHE: &str = "no-cache";

/// The paths only the loopback listener serves: `build_loopback_router`'s
/// `/shutdown`, `/proof` and `/hook/{session_id}` routes (matched by the
/// `/hook/` prefix, since the entry carries a trailing `/`), plus `/pair`, the
/// pairing route the tailnet router must never answer. The fallback answers
/// `404` for each of them, whatever the method and whether or not the phone
/// app's folder is installed, so a tailnet client never reaches a `503`, nor a
/// deep link to the app's page, on a path only the host's own listener has.
const LOOPBACK_ONLY_PATHS: &[&str] = &["/shutdown", "/proof", "/pair", "/hook/"];

/// The folder the listener serves the phone app from: a non-empty
/// `override_dir` wins; else the install folder `bin_templates` names; else
/// beside the daemon's own exe; else there is none. In a macOS app bundle, whose
/// binaries sit in `Contents/MacOS`, the folder is `Contents/Resources/pwa`. No
/// existence check: the caller answers `503` per request while the folder is
/// missing.
pub(super) fn resolve_pwa_dir(
    override_dir: Option<&OsStr>,
    bin_templates: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    bin_templates
        .map(pwa_dir_in)
        .or_else(|| exe_dir.map(pwa_dir_in))
}

/// The phone app's folder for a folder the daemon's binaries sit in: a macOS app
/// bundle's `Contents/MacOS` keeps it in `Contents/Resources/pwa`, since the
/// bundle's signature seals `Resources` and rejects non-code files in `MacOS`;
/// every other folder keeps it beside itself, as `pwa`.
fn pwa_dir_in(dir: &Path) -> PathBuf {
    let bundle_contents = dir.parent().filter(|contents| {
        (dir.file_name() == Some(OsStr::new("MacOS")))
            && (contents.file_name() == Some(OsStr::new("Contents")))
    });
    bundle_contents.map_or_else(
        || dir.join("pwa"),
        |contents| contents.join("Resources").join("pwa"),
    )
}

/// Log once, at listener start, when the resolved folder cannot serve a page.
/// The fallback keeps answering `503` per request, so installing the folder
/// later serves it without a restart.
pub(super) fn log_missing_folder(pwa_dir: Option<&Path>) {
    match pwa_dir {
        Some(dir) if dir.join("index.html").is_file() => {}
        Some(dir) => warn!(
            path = %dir.display(),
            "tailnet listener: the phone app's folder has no index.html; its pages answer 503"
        ),
        None => warn!("tailnet listener: no folder for the phone app; its pages answer 503"),
    }
}

/// The tailnet router's fallback: the phone app's files for `GET` and `HEAD`,
/// `404` for every other method, and `503` while no folder is installed.
pub(super) async fn serve(State(state): State<TailnetState>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let response = answer(state.pwa_dir.as_deref(), method, &path, req).await;
    with_headers(response, &path)
}

/// Whether `path` is one of the loopback router's own routes: equal to one of
/// [`LOOPBACK_ONLY_PATHS`]' entry, or under an entry that ends in `/`.
fn is_loopback_only_path(path: &str) -> bool {
    LOOPBACK_ONLY_PATHS
        .iter()
        .any(|route| path == *route || (route.ends_with('/') && path.starts_with(*route)))
}

/// The answer before the security and cache headers: `404` for a path only the
/// loopback listener serves, nothing for a write, `503` without an installed
/// folder, else the file `ServeDir` finds, with an extension-less miss answered
/// by the page so a deep link opens the app.
async fn answer(folder: Option<&Path>, method: Method, path: &str, req: Request) -> Response {
    if is_loopback_only_path(path) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(root) = folder else {
        return not_installed();
    };
    let index = root.join("index.html");
    if !is_file(&index).await {
        return not_installed();
    }
    let response = serve_dir(root, req).await;
    if response.status() != StatusCode::NOT_FOUND || !is_page_path(path) {
        return response;
    }
    let mut index_request = Request::new(Body::empty());
    *index_request.method_mut() = method;
    *index_request.uri_mut() = Uri::from_static("/");
    let served = serve_file(&index, index_request).await;
    let Some(status) = deep_link_status(served.status()) else {
        return not_installed();
    };
    let mut response = served;
    *response.status_mut() = status;
    response
}

/// The status a deep link's `index.html` answer takes: `ServeFile`'s own
/// success status, or `None` when it failed, which the caller answers with
/// [`not_installed`] instead of turning a failed read into a `200`.
fn deep_link_status(served: StatusCode) -> Option<StatusCode> {
    served.is_success().then_some(served)
}

fn not_installed() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        NOT_INSTALLED,
    )
        .into_response()
}

async fn serve_dir(root: &Path, req: Request) -> Response {
    match ServeDir::new(root).oneshot(req).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    }
}

async fn serve_file(file: &Path, req: Request) -> Response {
    match ServeFile::new(file).oneshot(req).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    }
}

/// Whether `path` names a page route rather than a file: not under
/// `/assets/`, and its percent-decoded last segment carries no `.`, so
/// `/s/abc123` is a deep link while `/assets/foo` and `/x%2Ejs` are files.
fn is_page_path(path: &str) -> bool {
    !path.starts_with("/assets/") && !last_segment_decoded(path).contains('.')
}

/// `path`'s last segment with its percent escapes decoded, since a client may
/// send `.` as `%2E`.
fn last_segment_decoded(path: &str) -> String {
    let raw = path.rsplit('/').next().unwrap_or_default();
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match (bytes[index], bytes.get(index + 1), bytes.get(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                if let (Some(high), Some(low)) = (hex_value(*high), hex_value(*low)) {
                    out.push(high * 16 + low);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// A hashed asset is immutable, and so is the `304` a cache revalidates it
/// into: the cache takes the revalidation's headers.
fn cache_control(path: &str, status: StatusCode) -> &'static str {
    let cacheable = status == StatusCode::OK
        || status == StatusCode::PARTIAL_CONTENT
        || status == StatusCode::NOT_MODIFIED;
    if cacheable && path.starts_with("/assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    }
}

fn with_headers(mut response: Response, path: &str) -> Response {
    let cache = cache_control(path, response.status());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn resolve_pwa_dir_prefers_the_override() {
        let expected = PathBuf::from("override");
        assert_eq!(
            resolve_pwa_dir(
                Some(OsString::from("override").as_os_str()),
                Some(Path::new("templates")),
                Some(Path::new("exe")),
            ),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_uses_bin_templates_next() {
        let expected = Path::new("templates").join("pwa");
        assert_eq!(
            resolve_pwa_dir(None, Some(Path::new("templates")), Some(Path::new("exe"))),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_falls_back_to_the_exe_dir() {
        let expected = Path::new("exe").join("pwa");
        assert_eq!(
            resolve_pwa_dir(None, None, Some(Path::new("exe"))),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_ignores_an_empty_override() {
        let expected = Path::new("templates").join("pwa");
        assert_eq!(
            resolve_pwa_dir(
                Some(OsString::from("").as_os_str()),
                Some(Path::new("templates")),
                Some(Path::new("exe")),
            ),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_is_none_with_nothing() {
        assert_eq!(resolve_pwa_dir(None, None, None), None);
    }

    #[test]
    fn resolve_pwa_dir_uses_a_bundles_resources_folder() {
        let bundle_binaries = Path::new("rustling-tulip.app")
            .join("Contents")
            .join("MacOS");
        let expected = Path::new("rustling-tulip.app")
            .join("Contents")
            .join("Resources")
            .join("pwa");
        assert_eq!(
            resolve_pwa_dir(None, Some(bundle_binaries.as_path()), None),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_uses_a_bundles_resources_folder_from_the_exe_dir() {
        let bundle_binaries = Path::new("rustling-tulip.app")
            .join("Contents")
            .join("MacOS");
        let expected = Path::new("rustling-tulip.app")
            .join("Contents")
            .join("Resources")
            .join("pwa");
        assert_eq!(
            resolve_pwa_dir(None, None, Some(bundle_binaries.as_path())),
            Some(expected)
        );
    }

    #[test]
    fn resolve_pwa_dir_keeps_pwa_beside_a_folder_named_macos_outside_a_bundle() {
        let binaries = Path::new("tools").join("MacOS");
        assert_eq!(
            resolve_pwa_dir(None, Some(binaries.as_path()), None),
            Some(binaries.join("pwa"))
        );
    }

    #[test]
    fn resolve_pwa_dir_keeps_the_override_inside_a_bundle() {
        let bundle_binaries = Path::new("rustling-tulip.app")
            .join("Contents")
            .join("MacOS");
        assert_eq!(
            resolve_pwa_dir(
                Some(OsString::from("override").as_os_str()),
                Some(bundle_binaries.as_path()),
                None,
            ),
            Some(PathBuf::from("override"))
        );
    }

    #[test]
    fn is_loopback_only_path_matches_the_loopback_routes() {
        for path in ["/proof", "/shutdown", "/pair", "/hook/s1", "/hook/"] {
            assert!(is_loopback_only_path(path), "{path} is loopback only");
        }
        for path in [
            "/",
            "/s/abc",
            "/assets/app.js",
            "/hooks",
            "/proofs",
            "/pairing",
        ] {
            assert!(!is_loopback_only_path(path), "{path} is not loopback only");
        }
    }

    #[test]
    fn a_failed_index_serve_is_not_turned_into_a_200() {
        assert_eq!(deep_link_status(StatusCode::OK), Some(StatusCode::OK));
        assert_eq!(deep_link_status(StatusCode::NOT_FOUND), None);
        assert_eq!(deep_link_status(StatusCode::INTERNAL_SERVER_ERROR), None);
    }
}
