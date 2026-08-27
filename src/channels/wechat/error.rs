use super::*;
// ── Error classification ──────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum ErrorClass {
    Auth,
    Network,
    Server,
    Parse,
}

pub(crate) fn error_class(err: &ApiError) -> ErrorClass {
    match err {
        ApiError::Http(code, _) if *code == 401 || *code == 403 => ErrorClass::Auth,
        ApiError::Api(code, msg) => {
            let lower = msg.to_lowercase();
            if *code == -1
                || lower.contains("token")
                || lower.contains("expired")
                || lower.contains("unauthorized")
                || lower.contains("not login")
                || lower.contains("请先登录")
                || lower.contains("未登录")
            {
                ErrorClass::Auth
            } else {
                ErrorClass::Server
            }
        }
        ApiError::Network(_) => ErrorClass::Network,
        ApiError::Parse(_) => ErrorClass::Parse,
        ApiError::NotAuthenticated => ErrorClass::Auth,
        ApiError::Http(_, _) => ErrorClass::Server,
    }
}

pub(crate) fn classify_backoff(err: &ApiError, count: u32) -> u64 {
    match err {
        ApiError::Network(_) => std::cmp::min(5 + 2 * count as u64, 30),
        ApiError::Parse(_) => 3,
        ApiError::Http(401, _) | ApiError::Http(403, _) => 5,
        _ => std::cmp::min(2u64.pow(std::cmp::min(count, 6)), 60),
    }
}
