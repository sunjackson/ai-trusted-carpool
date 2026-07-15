use crate::runtime::RuntimeState;
use tauri::{AppHandle, Emitter, Manager, State};
use url::Url;

pub const JOIN_LINK_EVENT: &str = "trusted-carpool:join-link";
const OFFICIAL_JOIN_HOST: &str = "p2p.cnaigc.ai";

fn normalize_code(value: &str) -> Option<String> {
    let code = value.trim().to_ascii_uppercase();
    (code.len() == 12
        && code
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'2'..=b'9')))
    .then_some(code)
}

pub fn parse_join_code(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let is_custom_link = url.scheme() == "trusted-carpool" && url.host_str() == Some("join");
    let is_official_link = url.scheme() == "https"
        && url.host_str() == Some(OFFICIAL_JOIN_HOST)
        && url.port().is_none();
    if !is_custom_link && !is_official_link {
        return None;
    }

    let path_segments = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let path_code = if is_custom_link {
        path_segments.first().copied()
    } else {
        match path_segments.as_slice() {
            ["join", code] | ["api", "v1", "carpool", "join", code] => Some(*code),
            _ => None,
        }
    };
    let query_code = url
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then_some(value.into_owned()));
    normalize_code(path_code.or(query_code.as_deref())?)
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn accept_urls<I, S>(app: &AppHandle, state: &RuntimeState, urls: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let code = urls
        .into_iter()
        .find_map(|candidate| parse_join_code(candidate.as_ref()))?;
    if let Ok(mut runtime) = state.inner.lock() {
        runtime.pending_join_code = Some(code.clone());
    }
    show_main_window(app);
    let _ = app.emit(JOIN_LINK_EVENT, &code);
    Some(code)
}

#[tauri::command]
pub fn take_pending_join_code(state: State<'_, RuntimeState>) -> Result<Option<String>, String> {
    state
        .inner
        .lock()
        .map_err(|_| "运行状态暂时不可用".to_string())
        .map(|mut runtime| runtime.pending_join_code.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_custom_and_official_join_links() {
        let code = "7G2K5LQ8M4TZ";
        assert_eq!(
            parse_join_code(&format!("trusted-carpool://join/{code}")),
            Some(code.to_string())
        );
        assert_eq!(
            parse_join_code(&format!("trusted-carpool://join?code={code}")),
            Some(code.to_string())
        );
        assert_eq!(
            parse_join_code(&format!("https://p2p.cnaigc.ai/join/{code}")),
            Some(code.to_string())
        );
        assert_eq!(
            parse_join_code(&format!("https://p2p.cnaigc.ai/api/v1/carpool/join/{code}")),
            Some(code.to_string())
        );
        assert_eq!(
            parse_join_code(&format!("https://p2p.cnaigc.ai/join?code={code}")),
            Some(code.to_string())
        );
    }

    #[test]
    fn rejects_untrusted_hosts_ports_and_unsafe_codes() {
        assert_eq!(
            parse_join_code("https://evil.example/join/7G2K5LQ8M4TZ"),
            None
        );
        assert_eq!(
            parse_join_code("https://p2p.cnaigc.ai:444/join/7G2K5LQ8M4TZ"),
            None
        );
        assert_eq!(
            parse_join_code("https://friend@p2p.cnaigc.ai/join/7G2K5LQ8M4TZ"),
            None
        );
        assert_eq!(
            parse_join_code("trusted-carpool://join:444/7G2K5LQ8M4TZ"),
            None
        );
        assert_eq!(
            parse_join_code("trusted-carpool://join/7G2K5LQ8M4TZ#ignored"),
            None
        );
        assert_eq!(parse_join_code("trusted-carpool://evil/7G2K5LQ8M4TZ"), None);
        assert_eq!(parse_join_code("trusted-carpool://join/AAAAAAAAAAA1"), None);
    }
}
