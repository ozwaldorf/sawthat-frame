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
use crate::widget::{Orientation, WidgetItem, WidgetName};

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
    paths(health, get_widget_data, get_widget_image),
    components(schemas(WidgetItem, WidgetName, Orientation))
)]
struct ApiDoc;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "concert_display_server=info,tower_http=debug".into()),
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
        .route("/api/widget/{widget_name}", get(get_widget_data))
        .route(
            "/api/widget/{widget_name}/{orientation}/{*image_path}",
            get(get_widget_image),
        )
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

/// Get widget data
///
/// Returns a list of widget items for the specified widget.
#[utoipa::path(
    get,
    path = "/api/widget/{widget_name}",
    params(
        ("widget_name" = WidgetName, Path, description = "Name of the widget")
    ),
    responses(
        (status = 200, description = "Widget data", body = Vec<WidgetItem>),
        (status = 400, description = "Invalid widget name")
    )
)]
async fn get_widget_data(
    State(state): State<AppState>,
    Path(widget_name): Path<WidgetName>,
) -> impl IntoResponse {
    let source = state.registry.get(widget_name);
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

/// Get processed widget image
///
/// Returns a processed PNG image for the specified widget item.
#[utoipa::path(
    get,
    path = "/api/widget/{widget_name}/{orientation}/{image_path}",
    params(
        ("widget_name" = WidgetName, Path, description = "Name of the widget"),
        ("orientation" = Orientation, Path, description = "Display orientation: horiz (400x480 or 800x480) or vert (480x800)"),
        ("image_path" = String, Path, description = "Path to the image resource")
    ),
    responses(
        (status = 200, description = "Processed image", content_type = "image/png"),
        (status = 400, description = "Invalid widget name, orientation, or path"),
        (status = 404, description = "Image not found")
    )
)]
async fn get_widget_image(
    State(state): State<AppState>,
    Path((widget_name, orientation, image_path)): Path<(WidgetName, Orientation, String)>,
) -> Result<Response, AppError> {
    tracing::info!(
        "Image request: widget={:?}, orientation={:?}, path={}",
        widget_name,
        orientation,
        image_path
    );

    let source = state.registry.get(widget_name);
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
