mod datasource;
mod image_processing;
mod palette;
mod sawthat;
mod text;
mod widget;

use fastly::http::{Method, StatusCode};
use fastly::{Error, Request, Response};

const OPENAPI_SPEC: &str = r#"{
  "openapi": "3.1.0",
  "info": {
    "title": "Concert Display Edge API",
    "description": "Widget API and image processing for concert display e-paper frame",
    "version": "0.1.0"
  },
  "servers": [
    { "url": "http://localhost:7676", "description": "Local development" }
  ],
  "paths": {
    "/health": {
      "get": {
        "summary": "Health check",
        "responses": {
          "200": { "description": "Service is healthy", "content": { "text/plain": { "schema": { "type": "string", "example": "ok" } } } }
        }
      }
    },
    "/api/widget/{widget_name}": {
      "get": {
        "summary": "Get widget data",
        "parameters": [
          { "name": "widget_name", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Name of the widget (e.g., 'sawthat')" }
        ],
        "responses": {
          "200": { "description": "Widget data", "content": { "application/json": { "schema": { "type": "object" } } } },
          "404": { "description": "Widget not found" }
        }
      }
    },
    "/api/widget/{widget_name}/{orientation}/{image_path}": {
      "get": {
        "summary": "Get processed widget image",
        "parameters": [
          { "name": "widget_name", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Name of the widget" },
          { "name": "orientation", "in": "path", "required": true, "schema": { "type": "string", "enum": ["horiz", "vert"] }, "description": "Display orientation: horiz (400x480 or 800x480) or vert (480x800)" },
          { "name": "image_path", "in": "path", "required": true, "schema": { "type": "string" }, "description": "Path to the image resource" }
        ],
        "responses": {
          "200": { "description": "Processed image", "content": { "image/png": { "schema": { "type": "string", "format": "binary" } } } },
          "404": { "description": "Image not found" }
        }
      }
    }
  }
}"#;

const SCALAR_HTML: &str = r#"<!doctype html>
<html>
<head>
  <title>Concert Display API</title>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
</head>
<body>
  <script id="api-reference" data-url="/openapi.json"></script>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>"#;

#[fastly::main]
fn main(req: Request) -> Result<Response, Error> {
    // Log request for debugging
    log::info!(
        "{} {}",
        req.get_method(),
        req.get_path()
    );

    // Route requests
    match (req.get_method(), req.get_path()) {
        // Health check
        (&Method::GET, "/health") => Ok(Response::from_status(StatusCode::OK)
            .with_body_text_plain("ok")),

        // API docs
        (&Method::GET, "/docs") => Ok(Response::from_status(StatusCode::OK)
            .with_content_type(fastly::mime::TEXT_HTML_UTF_8)
            .with_body(SCALAR_HTML)),

        // OpenAPI spec
        (&Method::GET, "/openapi.json") => Ok(Response::from_status(StatusCode::OK)
            .with_content_type(fastly::mime::APPLICATION_JSON)
            .with_body(OPENAPI_SPEC)),

        // Widget data endpoint: GET /api/widget/{widget_name}
        (&Method::GET, path) if path.starts_with("/api/widget/") => {
            handle_widget_request(req)
        }

        // Not found
        _ => Ok(Response::from_status(StatusCode::NOT_FOUND)
            .with_body_text_plain("Not found")),
    }
}

fn handle_widget_request(req: Request) -> Result<Response, Error> {
    let path = req.get_path();
    let parts: Vec<&str> = path
        .strip_prefix("/api/widget/")
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match parts.as_slice() {
        // Widget data: /api/widget/{widget_name}
        [widget_name] => widget::handle_widget_data(widget_name),

        // Widget image: /api/widget/{widget_name}/{orientation}/{path...}
        [widget_name, orientation, rest @ ..] => {
            let image_path = rest.join("/");
            widget::handle_widget_image(widget_name, orientation, &image_path)
        }

        _ => Ok(Response::from_status(StatusCode::BAD_REQUEST)
            .with_body_text_plain("Invalid widget path")),
    }
}
