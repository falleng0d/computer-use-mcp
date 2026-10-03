use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use computer_protocol::{
    ApiError, CreateSession, Health, Observation, PROTOCOL_VERSION, SessionCreated, SessionId,
    VERSION,
};
use tracing::error;
use uuid::Uuid;

use crate::sessions::{SessionError, Sessions};

const BEARER_PREFIX: &str = "Bearer ";

#[derive(Clone)]
struct AppState {
    token: Arc<str>,
    sessions: Sessions,
}

pub fn router(token: String, sessions: Sessions) -> Router {
    let state = AppState {
        token: token.into(),
        sessions,
    };
    Router::new()
        .route("/health", get(health))
        .route("/sessions", post(create_session))
        .route("/sessions/{id}", delete(end_session))
        .route("/sessions/{id}/observe", post(observe))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(BEARER_PREFIX));
    if given.is_some_and(|given| constant_time_eq(given.as_bytes(), state.token.as_bytes())) {
        next.run(request).await
    } else {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        response
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health() -> Json<Health> {
    Json(Health {
        protocol_version: PROTOCOL_VERSION,
        version: VERSION.to_owned(),
    })
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSession>,
) -> (StatusCode, Json<SessionCreated>) {
    let id = SessionId::parse(&Uuid::new_v4().simple().to_string())
        .expect("a simple UUID is 32 lowercase hex digits");
    state
        .sessions
        .insert(id.clone(), request.title, request.screen_size);
    (StatusCode::CREATED, Json(SessionCreated { session: id }))
}

async fn end_session(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, SessionError> {
    state.sessions.end(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn observe(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
) -> Result<Json<Observation>, SessionError> {
    state.sessions.observe(&id).await.map(Json)
}

impl IntoResponse for SessionError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unknown => StatusCode::NOT_FOUND,
            Self::NoFreeScreen => StatusCode::SERVICE_UNAVAILABLE,
            Self::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if let Self::Failed(error) = &self {
            error!(error = %format!("{error:#}"), "session call failed");
        }
        let body = ApiError {
            message: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http};
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "secret";

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("{BEARER_PREFIX}{token}"));
        }
        let request = request.body(Body::from(body.to_owned())).unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn requests_without_the_right_token_are_refused() {
        let app = router(TOKEN.to_owned(), Sessions::default());
        let health = |token| send(&app, "GET", "/health", token, "");
        assert_eq!(health(None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(health(Some("secreT")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(health(Some("secre")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(health(Some(TOKEN)).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_session_can_be_ended_once() {
        let app = router(TOKEN.to_owned(), Sessions::default());
        let (status, body) = send(
            &app,
            "POST",
            "/sessions",
            Some(TOKEN),
            r#"{"title":"fix the build","screen_size":"1280x800"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let created: SessionCreated = serde_json::from_slice(&body).unwrap();
        let uri = format!("/sessions/{}", created.session);

        let end = || send(&app, "DELETE", &uri, Some(TOKEN), "");
        assert_eq!(end().await.0, StatusCode::NO_CONTENT);
        assert_eq!(end().await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn observing_an_ended_session_is_not_found() {
        let app = router(TOKEN.to_owned(), Sessions::default());
        let uri = format!("/sessions/{}/observe", "0".repeat(32));
        assert_eq!(
            send(&app, "POST", &uri, Some(TOKEN), "").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_blank_title_is_rejected() {
        let app = router(TOKEN.to_owned(), Sessions::default());
        let (status, _) = send(
            &app,
            "POST",
            "/sessions",
            Some(TOKEN),
            r#"{"title":"  ","screen_size":"1280x800"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
