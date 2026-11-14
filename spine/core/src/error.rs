use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;
use utoipa::ToSchema;

use crate::math::MathError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Math(#[from] crate::math::MathError),
    #[error("field of view must be greater than 0 and at most {max} degrees")]
    FovOutOfRange { max: f64 },
    #[error("right ascension must be in [0, 360] and declination in [-90, 90]")]
    InvalidSky,
    #[error("region is outside the available exposure")]
    OutsideExposure,
    #[error("{0}")]
    NotFound(String),
    #[error("operation timed out after {seconds}s")]
    Deadline { seconds: u64 },
    #[error("upstream read failed: {0}")]
    Upstream(String),
    #[error("{0}")]
    Message(String),
    /// The process-wide work budget has no room for this job right now.
    #[error("{0}")]
    Busy(String),
}

/// The JSON body every extension returns for a failed request.
///
/// One schema keeps the routes consistent and leaves one place to change the
/// wire shape.
#[derive(Serialize, ToSchema)]
pub struct ApiError {
    pub error: String,
}

/// The one status mapping for a failed route.
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Math(MathError::Input(_)) => StatusCode::BAD_REQUEST,
            Self::Math(MathError::Busy { .. }) => StatusCode::TOO_MANY_REQUESTS,
            Self::Math(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::FovOutOfRange { .. } | Self::InvalidSky | Self::Message(_) => StatusCode::BAD_REQUEST,
            Self::Busy(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Deadline { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::OutsideExposure | Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
        };
        (status, Json(ApiError { error: self.to_string() })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_work_budget_is_too_many_requests() {
        let response = Error::Busy("no room".into()).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn math_errors_map_to_the_caller_or_the_server() {
        let status = |error: MathError| Error::Math(error).into_response().status();
        assert_eq!(status(MathError::Input("bad".into())), StatusCode::BAD_REQUEST);
        assert_eq!(status(MathError::Runtime("down".into())), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
