mod datasource;
mod deezer;
mod error;
mod image_processing;
mod palette;
mod sawthat;
mod text;
mod widget;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use reqwest::Client;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::datasource::DataSourceRegistry;
use crate::error::AppError;
use crate::widget::{Orientation, WidgetName};

/// Application state shared across handlers
#[derive(Clone)]
struct AppState {
    registry: Arc<DataSourceRegistry>,
}

/// OpenAPI documentation
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Concert Display Edge API",
        description = "Widget API and image processing for concert display e-paper frame",
        version = "0.1.0"
    ),
    tags(
        (name = "Concerts", description = "Concert history widget endpoints")
    ),
    paths(health, get_concerts_data, get_concerts_image),
    components(schemas(Orientation))
)]
struct ApiDoc;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Create HTTP client
    let client = Client::new();

    // Create data source registry
    let registry = Arc::new(DataSourceRegistry::new(client));

    // Create app state
    let state = AppState { registry };

    // Build router
    let app = Router::new()
        .route("/health", get(health))
        .route("/concerts", get(get_concerts_data))
        .route("/concerts/{orientation}/{*image_path}", get(get_concerts_image))
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
        .route("/openapi.json", get(openapi_json))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Get port from environment or use default
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Starting server on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Health check endpoint
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service is healthy", body = String)
    )
)]
async fn health() -> &'static str {
    "ok"
}

/// Get OpenAPI JSON specification
async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Get concerts data
///
/// Returns a list of concert items to display.
#[utoipa::path(
    get,
    path = "/concerts",
    tag = "Concerts",
    responses(
        (status = 200, description = "Concert data", body = Vec<String>)
    )
)]
async fn get_concerts_data(State(state): State<AppState>) -> impl IntoResponse {
    let source = state.registry.get(WidgetName::Concerts);
    let items = source.fetch_data().await;
    let cache_policy = source.data_cache_policy();

    match items {
        Ok(items) => Ok((
            [(header::HeaderName::from_static("x-cache-policy"), cache_policy.to_string())],
            Json(items),
        )),
        Err(e) => Err(e),
    }
}

/// Get processed concert image
///
/// Returns a processed PNG image for a concert item.
#[utoipa::path(
    get,
    path = "/concerts/{orientation}/{image_path}",
    tag = "Concerts",
    params(
        ("orientation" = Orientation, Path, description = "Display orientation: horiz (400x480 or 800x480) or vert (480x800)"),
        ("image_path" = String, Path, description = "Path to the image resource")
    ),
    responses(
        (status = 200, description = "Processed image", content_type = "image/png"),
        (status = 400, description = "Invalid orientation or path"),
        (status = 404, description = "Image not found")
    )
)]
async fn get_concerts_image(
    State(state): State<AppState>,
    Path((orientation, image_path)): Path<(Orientation, String)>,
) -> Result<Response, AppError> {
    tracing::info!(
        "Image request: concerts, orientation={:?}, path={}",
        orientation,
        image_path
    );

    let source = state.registry.get(WidgetName::Concerts);
    let png_data = source.fetch_image(&image_path, orientation).await?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        png_data,
    )
        .into_response())
}
