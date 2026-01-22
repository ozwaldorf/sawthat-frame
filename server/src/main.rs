mod cache;
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
        .route(
            "/concerts/{orientation}/{*image_path}",
            get(get_concerts_image),
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
            [(
                header::HeaderName::from_static("x-cache-policy"),
                cache_policy.to_string(),
            )],
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
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        png_data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_processing::{extract_primary_color, process_image_with_color};
    use crate::text::ConcertInfo;
    use crate::widget::WidgetWidth;
    use std::fs;
    use std::path::Path;

    /// Concert data: (filename, band_name, date, venue, image_url)
    /// Uses Deezer album art URLs for period-appropriate artwork
    const EXAMPLE_CONCERTS: &[(&str, &str, &str, &str, &str)] = &[
        (
            "santana_2012",
            "Santana",
            "July 27th, 2012",
            "SPAC, Saratoga, NY",
            "https://cdn-images.dzcdn.net/images/cover/3e501a236755d6f137cc1ebe1c43b261/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "primus_2014",
            "Primus",
            "October 24th, 2014",
            "The Palace Theatre, Albany, NY",
            "https://cdn-images.dzcdn.net/images/cover/818c296a5b7f748301d2419751c874a8/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "billy_strings_2017",
            "Billy Strings",
            "July 14th, 2017",
            "Grey Fox",
            "https://cdn-images.dzcdn.net/images/cover/63620774463dce288c9151e4c8fff3f6/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "korn_2022",
            "Korn",
            "March 20th, 2022",
            "MVP Arena, Albany, NY",
            "https://cdn-images.dzcdn.net/images/cover/84eefcf43b9eac0da217408632c7a8c9/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "griz_2022",
            "GRiZ",
            "December 30th, 2022",
            "HiJinx, PA",
            "https://cdn-images.dzcdn.net/images/cover/bc4026f540f3052331511a4ad6d7de15/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "yonder_mountain_2024",
            "Yonder Mountain String Band",
            "September 1st, 2024",
            "Lake George",
            "https://cdn-images.dzcdn.net/images/cover/4b30dd2ef2fb7f6d4d41dc2fd3848e5c/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "atmosphere_2025",
            "Atmosphere",
            "February 7th, 2025",
            "Empire Live",
            "https://cdn-images.dzcdn.net/images/cover/ef8bb006d8c9ff8850b4607801b68aac/1000x1000-000000-80-0-0.jpg",
        ),
        (
            "phish_2025",
            "Phish",
            "July 25th, 2025",
            "SPAC, Saratoga, NY",
            "https://cdn-images.dzcdn.net/images/cover/7696975fc09328bcf935ded738e0358c/1000x1000-000000-80-0-0.jpg",
        ),
    ];

    const OUTPUT_DIR: &str = "examples";

    /// Generate example images for the README.
    /// Run with: cargo test generate_readme_examples -- --nocapture
    #[tokio::test]
    async fn generate_readme_examples() {
        let client = reqwest::Client::new();

        let output_path = Path::new(OUTPUT_DIR);
        if !output_path.exists() {
            fs::create_dir_all(output_path).expect("Failed to create output directory");
        }

        println!("\nGenerating README example images...\n");

        for (filename, band_name, date, venue, image_url) in EXAMPLE_CONCERTS {
            println!("Processing: {} - {}", band_name, date);
            println!("  Fetching image from: {}", image_url);

            let response = client
                .get(*image_url)
                .send()
                .await
                .expect("Failed to fetch image");

            if !response.status().is_success() {
                eprintln!("  Error: Failed to fetch image, status {}", response.status());
                continue;
            }

            let image_data = response
                .bytes()
                .await
                .expect("Failed to read image bytes")
                .to_vec();

            println!("  Downloaded {} bytes", image_data.len());

            let primary_color = extract_primary_color(&image_data).expect("Failed to extract color");
            println!(
                "  Primary color: RGB({}, {}, {}), light: {}",
                primary_color.r, primary_color.g, primary_color.b, primary_color.is_light
            );

            let concert_info = ConcertInfo {
                band_name: band_name.to_string(),
                date: date.to_string(),
                venue: venue.to_string(),
            };

            // Generate horizontal image (400x480)
            let (horiz_width, horiz_height) = Orientation::Horiz.dimensions(WidgetWidth::Half);
            let horiz_png = process_image_with_color(
                &image_data,
                horiz_width,
                horiz_height,
                Some(&concert_info),
                &primary_color,
            )
            .expect("Failed to process horizontal image");

            let horiz_path = format!("{}/{}_horiz.png", OUTPUT_DIR, filename);
            fs::write(&horiz_path, &horiz_png).expect("Failed to write horizontal image");
            println!("  Saved: {} ({} bytes)", horiz_path, horiz_png.len());

            // Generate vertical image (480x800)
            let (vert_width, vert_height) = Orientation::Vert.dimensions(WidgetWidth::Half);
            let vert_png = process_image_with_color(
                &image_data,
                vert_width,
                vert_height,
                Some(&concert_info),
                &primary_color,
            )
            .expect("Failed to process vertical image");

            let vert_path = format!("{}/{}_vert.png", OUTPUT_DIR, filename);
            fs::write(&vert_path, &vert_png).expect("Failed to write vertical image");
            println!("  Saved: {} ({} bytes)", vert_path, vert_png.len());

            println!();
        }

        println!("Done! Generated {} example images.", EXAMPLE_CONCERTS.len() * 2);
    }
}
