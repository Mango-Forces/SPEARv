//! A minimal page containing only the rendered view and a link back to the app.
//!
//! `/embed` is mounted at the site root, not under `/api/v1`. It accepts the
//! same query as `render.png` and `delta.png`, validates it with the shared
//! parser, and rebuilds the image and attribution URLs from typed values so a
//! query cannot inject HTML or another URL.

use std::sync::Arc;

use axum::extract::RawQuery;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use spearv_core::{
    query_pairs, render_grid_size, Context, Error, Extension, Mode, Pin, Result, Sign, View, ViewQuery,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// The embed links to `/render.png` or `/delta.png`, so it inherits the same
/// ten-degree parser limit and then the render grid's 4096 px cap.
const MAX_FOV_DEG: f64 = 10.0;
/// Longer queries are not a view. Reject before parsing.
const MAX_QUERY_BYTES: usize = 2048;

pub struct Embed;

impl Extension for Embed {
    fn id(&self) -> &'static str {
        "embed"
    }

    fn routes(&self) -> OpenApiRouter<Arc<Context>> {
        OpenApiRouter::new().routes(routes!(embed_page))
    }
}

#[utoipa::path(
    get,
    path = "/embed",
    responses(
        (status = 200, description = "Minimal image page for iframes", content_type = "text/html"),
        (status = 400, description = "View parameters are invalid")
    )
)]
async fn embed_page(RawQuery(raw): RawQuery) -> Response {
    let raw = raw.as_deref().unwrap_or("");
    if raw.len() > MAX_QUERY_BYTES {
        return bad_request();
    }
    let view = match parse_view(raw) {
        Ok(view) => view,
        Err(_) => return bad_request(),
    };
    if view.mode == Mode::Delta && !delta_pins_ok(&view.pins) {
        return bad_request();
    }
    let mut response = Html(page(&view)).into_response();
    // Only the embed page is explicitly frameable. The rest of the app keeps
    // its existing response headers.
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors *"),
    );
    response
}

fn bad_request() -> Response {
    // Do not echo the query: the error is deliberately generic so a hostile
    // value cannot be reflected even as text.
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "invalid view parameters",
    )
        .into_response()
}

fn parse_view(raw: &str) -> Result<View> {
    let mut query = ViewQuery {
        ra: f64::NAN,
        dec: f64::NAN,
        fov_l: f64::NAN,
        fov_h: f64::NAN,
        mode: None,
        dr: None,
        pins: Vec::new(),
        mask: None,
    };
    for (key, value) in query_pairs(raw) {
        match key.as_str() {
            "ra" => query.ra = value.parse().unwrap_or(f64::NAN),
            "dec" => query.dec = value.parse().unwrap_or(f64::NAN),
            "fov_l" => query.fov_l = value.parse().unwrap_or(f64::NAN),
            "fov_h" => query.fov_h = value.parse().unwrap_or(f64::NAN),
            "mode" => query.mode = Some(value),
            "dr" => query.dr = Some(value),
            "mask" => query.mask = Some(value),
            "pin" => query.pins.push(value),
            _ => {}
        }
    }
    let view = query.parse(MAX_FOV_DEG)?;
    // The page embeds a render PNG, so a field the PNG grid would crop is a
    // broken embed, not a wider picture.
    render_grid_size(view.region.fov_l_deg)?;
    render_grid_size(view.region.fov_h_deg)?;
    if !release_ok(&view.release) {
        return Err(Error::Message("release id has unexpected characters".into()));
    }
    Ok(view)
}

/// Release ids in the index are `qr2`, `qr3`, `dr1`, `qr3_deep`, and future
/// names in the same shape. Whitespace, quotes, slashes and HTML are not release ids.
fn release_ok(release: &str) -> bool {
    !release.is_empty()
        && release.len() <= 32
        && release
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn delta_pins_ok(pins: &[Pin]) -> bool {
    if pins.is_empty() {
        return false;
    }
    let mut plus = false;
    let mut minus = false;
    for pin in pins {
        if pin.visit.is_none() || pin.sign.is_none() {
            return false;
        }
        match pin.sign {
            Some(Sign::Plus) => plus = true,
            Some(Sign::Minus) => minus = true,
            None => {}
        }
    }
    plus && minus
}

fn page(view: &View) -> String {
    let query = canonical_query(view);
    let image_path = if view.mode == Mode::Delta {
        "/api/v1/delta.png"
    } else {
        "/api/v1/render.png"
    };
    let image = html_escape(&format!("{image_path}?{query}"));
    let full = html_escape(&format!("/?{query}"));
    let alt = format!(
        "SPHEREx view at right ascension {:.3} degrees, declination {:.3} degrees",
        view.region.ra, view.region.dec
    );
    format!(
        concat!(
            "<!doctype html>\n",
            "<html lang=\"en\">\n",
            "<head>\n",
            "<meta charset=\"utf-8\">\n",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n",
            "<title>SPEARv</title>\n",
            "<style>\n",
            "html,body{{margin:0;height:100%;background:#0c0d09;color:#b7b1a4}}\n",
            "body{{display:grid;place-items:center}}\n",
            "figure{{margin:0;width:100%;height:100%;display:flex;flex-direction:column;min-height:0}}\n",
            "img{{display:block;flex:1 1 0;min-height:0;width:100%;height:auto;object-fit:contain;image-rendering:pixelated}}\n",
            "figcaption{{flex:0 0 auto;padding:.35rem .55rem;text-align:right;font:12px/1.3 ui-sans-serif,system-ui,sans-serif}}\n",
            "a{{color:#c4a15a}}\n",
            "</style>\n",
            "</head>\n",
            "<body>\n",
            "<figure>\n",
            "<img src=\"{image}\" alt=\"{alt}\">\n",
            "<figcaption><a href=\"{full}\" target=\"_blank\" rel=\"noopener\">SPEARv · open this view</a></figcaption>\n",
            "</figure>\n",
            "</body>\n",
            "</html>\n"
        ),
        image = image,
        alt = alt,
        full = full,
    )
}

fn canonical_query(view: &View) -> String {
    let mode = match view.mode {
        Mode::Composite => "composite",
        Mode::Slider => "slider",
        Mode::Range => "range",
        Mode::Delta => "delta",
    };
    let mut pairs = vec![
        format!("ra={}", encode(&view.region.ra.to_string())),
        format!("dec={}", encode(&view.region.dec.to_string())),
        format!("fov_l={}", encode(&view.region.fov_l_deg.to_string())),
        format!("fov_h={}", encode(&view.region.fov_h_deg.to_string())),
        format!("mode={}", encode(mode)),
        format!("dr={}", encode(&view.release)),
        format!("mask={}", if view.mask { "on" } else { "off" }),
    ];
    for pin in &view.pins {
        pairs.push(format!("pin={}", encode(&pin_value(pin))));
    }
    pairs.join("&")
}

fn pin_value(pin: &Pin) -> String {
    if pin.visit.is_none() && pin.sign.is_none() {
        return format!("{}:{}", pin.center, pin.width);
    }
    let visit = pin.visit.map(|visit| format!("v{visit}")).unwrap_or_default();
    let sign = match pin.sign {
        Some(Sign::Plus) => "+",
        Some(Sign::Minus) => "-",
        None => "",
    };
    format!("{}:{}:{}:{}", pin.center, pin.width, visit, sign)
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spearv_core::SkyRegion;

    fn view() -> View {
        View {
            region: SkyRegion::square(56.75, 24.12, 0.1),
            mode: Mode::Delta,
            release: "qr3_deep".into(),
            pins: vec![
                Pin {
                    center: 42,
                    width: 5,
                    visit: Some(1),
                    sign: Some(Sign::Plus),
                },
                Pin {
                    center: 42,
                    width: 5,
                    visit: Some(2),
                    sign: Some(Sign::Minus),
                },
            ],
            mask: false,
        }
    }

    #[test]
    fn rectangular_embed_is_canonical_and_rejects_scalar_fov() {
        assert!(parse_view("ra=10&dec=20&fov=0.1").is_err());
        let view = parse_view("ra=10&dec=20&fov_l=6.99&fov_h=0.1").unwrap();
        let query = canonical_query(&view);
        assert!(query.contains("fov_l=6.99"));
        assert!(query.contains("fov_h=0.1"));
        assert!(!query.contains("fov="));
        assert!(parse_view("ra=10&dec=20&fov_l=0.1&fov_h=7").is_err());
    }

    #[test]
    fn a_plus_sign_is_percent_encoded_so_a_reader_cannot_turn_it_into_space() {
        let query = canonical_query(&view());
        assert!(query.contains("pin=42%3A5%3Av1%3A%2B"), "{query}");
        assert!(query.contains("pin=42%3A5%3Av2%3A-"), "{query}");
        assert!(query.contains("mask=off"), "{query}");
    }

    #[test]
    fn html_escapes_attributes_and_uses_the_delta_route() {
        let html = page(&view());
        assert!(html.contains("/api/v1/delta.png?"), "{html}");
        assert!(html.contains("&amp;pin="), "{html}");
        assert!(html.contains("href=\"/?ra=56.75"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn a_hostile_release_cannot_break_out_of_the_attribute() {
        let mut view = view();
        view.release = "\"><script>alert(1)</script>".into();
        let html = page(&view);
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("alert(1)"), "{html}");
    }

    #[test]
    fn release_ids_are_restricted() {
        assert!(release_ok("qr3"));
        assert!(release_ok("qr3_deep"));
        assert!(release_ok("dr1"));
        assert!(release_ok("qr2-2026"));
        assert!(!release_ok(""));
        assert!(!release_ok("qr3 deep"));
        assert!(!release_ok("qr3/../qr2"));
        assert!(!release_ok("qr3\"><script>"));
    }

    #[test]
    fn delta_needs_two_signed_visits() {
        assert!(delta_pins_ok(&view().pins));
        assert!(!delta_pins_ok(&[]));
        let mut pins = view().pins;
        pins[0].visit = None;
        assert!(!delta_pins_ok(&pins));
        let mut pins = view().pins;
        pins[1].sign = Some(Sign::Plus);
        assert!(!delta_pins_ok(&pins));
    }

    #[test]
    fn an_embed_above_the_render_grid_is_rejected() {
        assert!(parse_view("ra=56.75&dec=24.12&fov_l=6.99&fov_h=6.99").is_ok());
        let error = parse_view("ra=56.75&dec=24.12&fov_l=7.0&fov_h=7.0").expect_err("the PNG grid would crop");
        assert!(error.to_string().contains("6.997"), "{error}");
    }
}
