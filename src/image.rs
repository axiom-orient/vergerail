//! Typed image-generation requests and items.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use flate2::read::ZlibDecoder;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::path::{Path, PathBuf};

/// Background selection for a direct image-generation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageBackground {
    /// Let the image provider choose the background treatment.
    Auto,
    /// Request an image with an alpha-capable transparent background.
    Transparent,
    /// Request an opaque image background.
    Opaque,
}

impl ImageBackground {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Transparent => "transparent",
            Self::Opaque => "opaque",
        }
    }
}

/// Quality selection for a direct image-generation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageQuality {
    /// Let the image provider choose the quality.
    Auto,
    /// Request low-cost generation.
    Low,
    /// Request medium-quality generation.
    Medium,
    /// Request high-quality generation.
    High,
}

impl ImageQuality {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Size selection for a direct image-generation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageSize {
    /// Let the image provider choose the dimensions.
    Auto,
    /// Request a square 1024×1024 image.
    Square,
    /// Request a 1536×1024 landscape image.
    Landscape,
    /// Request a 1024×1536 portrait image.
    Portrait,
}

impl ImageSize {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Square => "1024x1024",
            Self::Landscape => "1536x1024",
            Self::Portrait => "1024x1536",
        }
    }
}

/// One direct, non-idempotent image-generation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectImageRequest {
    /// Images API model identifier.
    pub model: String,
    /// Natural-language image prompt.
    pub prompt: String,
    /// Requested background treatment.
    pub background: ImageBackground,
    /// Requested output dimensions.
    pub size: ImageSize,
    /// Requested generation quality.
    pub quality: ImageQuality,
}

/// One validated PNG returned by the official Images endpoint adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectImageResponse {
    pub(crate) base64: String,
    pub(crate) media_type: String,
    pub(crate) byte_length: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) transparent_background: Option<bool>,
    pub(crate) alpha_capable: bool,
}

impl DirectImageResponse {
    /// Returns the base64-encoded PNG bytes.
    #[must_use]
    pub fn base64(&self) -> &str {
        &self.base64
    }

    /// Returns the MIME type reported by the official Images endpoint.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Returns the decoded PNG byte length.
    #[must_use]
    pub const fn byte_length(&self) -> usize {
        self.byte_length
    }

    /// Returns the validated PNG width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Returns the validated PNG height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Returns the provider's transparent-background metadata, when present.
    #[must_use]
    pub const fn transparent_background(&self) -> Option<bool> {
        self.transparent_background
    }

    /// Returns whether the PNG color format can carry an alpha channel.
    #[must_use]
    pub const fn alpha_capable(&self) -> bool {
        self.alpha_capable
    }
}

/// ChatGPT image endpoint used by the official Codex runtime.
pub(crate) const CHATGPT_IMAGE_GENERATION_ENDPOINT: &str =
    "https://chatgpt.com/backend-api/codex/images/generations";
const CODEX_VERSION: &str = "0.150.1";
const ORIGINATOR: &str = "codex_cli_rs";
const MAX_IMAGE_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_JWT_BYTES: usize = 256 * 1024;
const MAX_ACCOUNT_ID_BYTES: usize = 256;
static IMAGE_TURN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(crate) struct ChatGptImageAuth {
    access_token: String,
    account_id: String,
}

impl ChatGptImageAuth {
    pub(crate) fn from_auth_status(response: &Value) -> Result<Self, crate::error::Error> {
        let parsed: AuthStatus = serde_json::from_value(response.clone()).map_err(|error| {
            crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                format!("official app-server auth status was invalid: {error}"),
            )
        })?;
        if parsed.auth_method.as_deref() != Some("chatgpt") {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server did not report a ChatGPT login",
            ));
        }
        if parsed.requires_openai_auth == Some(false) {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server is configured without ChatGPT authentication",
            ));
        }
        let token = parsed.auth_token.ok_or_else(|| {
            crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server did not export an access token",
            )
        })?;
        if token.is_empty() || token.len() > MAX_JWT_BYTES || token.contains('\0') {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server exported an invalid bounded access token",
            ));
        }
        let account_id = account_id_from_jwt(&token)?;
        Ok(Self {
            access_token: token,
            account_id,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    auth_method: Option<String>,
    auth_token: Option<String>,
    requires_openai_auth: Option<bool>,
}

fn account_id_from_jwt(token: &str) -> Result<String, crate::error::Error> {
    let mut segments = token.split('.');
    let _header = segments.next();
    let payload = segments.next().ok_or_else(|| {
        crate::error::Error::new(
            crate::error::ErrorKind::Authentication,
            "image.auth",
            "official app-server access token is not a JWT",
        )
    })?;
    if segments.next().is_none() || segments.next().is_some() || payload.is_empty() {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::Authentication,
            "image.auth",
            "official app-server access token has an invalid JWT shape",
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .map_err(|_| {
            crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server access token payload is not valid base64url",
            )
        })?;
    let claims: Value = serde_json::from_slice(&decoded).map_err(|_| {
        crate::error::Error::new(
            crate::error::ErrorKind::Authentication,
            "image.auth",
            "official app-server access token payload is not valid JSON",
        )
    })?;
    let account = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_ACCOUNT_ID_BYTES
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            crate::error::Error::new(
                crate::error::ErrorKind::Authentication,
                "image.auth",
                "official app-server access token lacks a bounded ChatGPT account claim",
            )
        })?;
    Ok(account.to_owned())
}

#[derive(Debug, Deserialize)]
struct ImageEndpointResponse {
    #[serde(default)]
    data: Vec<ImageEndpointData>,
}

#[derive(Debug, Deserialize)]
struct ImageEndpointData {
    b64_json: String,
}

#[derive(Debug)]
pub(crate) enum ImageEndpointError {
    Unauthorized,
    Failed(crate::error::Error),
}

pub(crate) async fn generate_via_endpoint(
    endpoint: String,
    auth: ChatGptImageAuth,
    request: DirectImageRequest,
    timeout: Duration,
    turn_id: String,
) -> Result<DirectImageResponse, ImageEndpointError> {
    tokio::task::spawn_blocking(move || {
        send_image_request(&endpoint, &auth, &request, timeout, &turn_id)
    })
    .await
    .map_err(|error| {
        ImageEndpointError::Failed(crate::error::Error::new(
            crate::error::ErrorKind::Process,
            "image.http",
            format!("image request worker failed: {error}"),
        ))
    })?
}

fn send_image_request(
    endpoint: &str,
    auth: &ChatGptImageAuth,
    request: &DirectImageRequest,
    timeout: Duration,
    turn_id: &str,
) -> Result<DirectImageResponse, ImageEndpointError> {
    if request.model.trim().is_empty() || request.model.len() > 160 || request.model.contains('\0')
    {
        return Err(ImageEndpointError::Failed(crate::error::Error::new(
            crate::error::ErrorKind::InvalidInput,
            "image.generate",
            "image model must be a bounded non-empty value",
        )));
    }
    let body = serde_json::json!({
        "prompt": request.prompt,
        "background": request.background.as_str(),
        "model": request.model,
        "n": 1,
        "quality": request.quality.as_str(),
        "size": request.size.as_str(),
    });
    let body_bytes = serde_json::to_vec(&body).map_err(|error| {
        ImageEndpointError::Failed(crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            format!("image request could not be encoded: {error}"),
        ))
    })?;
    if body_bytes.len() > 256 * 1024 {
        return Err(ImageEndpointError::Failed(crate::error::Error::new(
            crate::error::ErrorKind::ResourceLimit,
            "image.http",
            "image request body exceeds the bounded limit",
        )));
    }
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(timeout))
        .build()
        .new_agent();
    let response = agent
        .post(endpoint)
        .header("Authorization", &format!("Bearer {}", auth.access_token))
        .header("Chatgpt-Account-Id", &auth.account_id)
        .header("Version", CODEX_VERSION)
        .header("Originator", ORIGINATOR)
        .header("x-codex-image-turn-id", turn_id)
        .content_type("application/json")
        .send(&body_bytes)
        .map_err(|error| {
            ImageEndpointError::Failed(crate::error::Error::new(
                crate::error::ErrorKind::Disconnected,
                "image.http",
                format!("official image endpoint request failed: {error}"),
            ))
        })?;
    if response.status() == 401 {
        return Err(ImageEndpointError::Unauthorized);
    }
    if response.status() != 200 {
        return Err(ImageEndpointError::Failed(crate::error::Error::new(
            crate::error::ErrorKind::Rpc,
            "image.http",
            format!(
                "official image endpoint returned HTTP {}",
                response.status()
            ),
        )));
    }
    let bytes = response
        .into_body()
        .with_config()
        .limit(MAX_IMAGE_JSON_BYTES as u64)
        .read_to_vec()
        .map_err(|error| {
            ImageEndpointError::Failed(crate::error::Error::new(
                crate::error::ErrorKind::Protocol,
                "image.http",
                format!("official image endpoint response could not be read: {error}"),
            ))
        })?;
    parse_image_endpoint_response(&bytes, request.background).map_err(ImageEndpointError::Failed)
}

pub(crate) fn image_turn_id() -> String {
    let sequence = IMAGE_TURN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "vergerail-image-{}-{nanos:016x}-{sequence}",
        std::process::id()
    )
}

fn parse_image_endpoint_response(
    bytes: &[u8],
    requested_background: ImageBackground,
) -> Result<DirectImageResponse, crate::error::Error> {
    let response: ImageEndpointResponse = serde_json::from_slice(bytes).map_err(|error| {
        crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            format!("official image endpoint returned invalid JSON: {error}"),
        )
    })?;
    if response.data.len() != 1 {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            format!(
                "official image endpoint returned {} images; exactly one is required",
                response.data.len()
            ),
        ));
    }
    let encoded = &response.data[0].b64_json;
    if encoded.is_empty() || encoded.len() > MAX_IMAGE_RESPONSE_BYTES.div_ceil(3) * 4 {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::ResourceLimit,
            "image.http",
            "official image endpoint returned an oversized or empty image",
        ));
    }
    let decoded = BASE64_STANDARD.decode(encoded).map_err(|error| {
        crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            format!("official image endpoint returned invalid PNG base64: {error}"),
        )
    })?;
    if decoded.len() > MAX_IMAGE_RESPONSE_BYTES {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::ResourceLimit,
            "image.http",
            "official image endpoint returned an oversized PNG",
        ));
    }
    let info = validate_png(&decoded)?;
    Ok(DirectImageResponse {
        base64: BASE64_STANDARD.encode(&decoded),
        media_type: "image/png".to_owned(),
        byte_length: decoded.len(),
        width: info.width,
        height: info.height,
        transparent_background: match requested_background {
            ImageBackground::Transparent => Some(info.has_transparent_pixels),
            ImageBackground::Auto | ImageBackground::Opaque => None,
        },
        alpha_capable: info.alpha_capable,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PngInfo {
    width: u32,
    height: u32,
    alpha_capable: bool,
    has_transparent_pixels: bool,
}

fn validate_png(bytes: &[u8]) -> Result<PngInfo, crate::error::Error> {
    if bytes.len() < 33 || bytes.get(..8) != Some(b"\x89PNG\r\n\x1a\n") {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            "image response is not a PNG",
        ));
    }
    let mut offset = 8usize;
    let mut ihdr = None;
    let mut idat = Vec::new();
    let mut saw_iend = false;
    while offset < bytes.len() {
        if bytes.len() - offset < 12 {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Protocol,
                "image.http",
                "PNG chunk is truncated",
            ));
        }
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let chunk_end = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| {
                crate::error::Error::new(
                    crate::error::ErrorKind::ResourceLimit,
                    "image.http",
                    "PNG chunk length overflow",
                )
            })?;
        if chunk_end > bytes.len() {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Protocol,
                "image.http",
                "PNG chunk exceeds response",
            ));
        }
        let chunk_type = &bytes[offset + 4..offset + 8];
        let data = &bytes[offset + 8..offset + 8 + length];
        let observed_crc =
            u32::from_be_bytes(bytes[offset + 8 + length..chunk_end].try_into().unwrap());
        if png_crc32(&bytes[offset + 4..offset + 8 + length]) != observed_crc {
            return Err(crate::error::Error::new(
                crate::error::ErrorKind::Protocol,
                "image.http",
                "PNG chunk CRC mismatch",
            ));
        }
        match chunk_type {
            b"IHDR" => {
                if ihdr.is_some() || length != 13 || offset != 8 {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG has an invalid IHDR",
                    ));
                }
                let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let bit_depth = data[8];
                let color_type = data[9];
                if width == 0
                    || height == 0
                    || width > 8192
                    || height > 8192
                    || bit_depth != 8
                    || !matches!(color_type, 0 | 2 | 3 | 4 | 6)
                    || data[10] != 0
                    || data[11] != 0
                {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG IHDR dimensions or format are unsupported",
                    ));
                }
                ihdr = Some((width, height, color_type));
            }
            b"IDAT" => {
                if ihdr.is_none() {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG IDAT appears before IHDR",
                    ));
                }
                idat.extend_from_slice(data);
                if idat.len() > MAX_IMAGE_JSON_BYTES {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::ResourceLimit,
                        "image.http",
                        "PNG compressed data exceeds the bound",
                    ));
                }
            }
            b"IEND" => {
                if length != 0 || idat.is_empty() {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG has an invalid IEND",
                    ));
                }
                saw_iend = true;
                if chunk_end != bytes.len() {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG contains data after IEND",
                    ));
                }
            }
            _ => {}
        }
        offset = chunk_end;
    }
    let (width, height, color_type) = ihdr.ok_or_else(|| {
        crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            "PNG is missing IHDR",
        )
    })?;
    if !saw_iend {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            "PNG is missing IEND",
        ));
    }
    let channels = match color_type {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => unreachable!(),
    };
    let expected_raw = (width as usize)
        .checked_mul(channels)
        .and_then(|row| row.checked_add(1))
        .and_then(|row| row.checked_mul(height as usize))
        .ok_or_else(|| {
            crate::error::Error::new(
                crate::error::ErrorKind::ResourceLimit,
                "image.http",
                "PNG dimensions exceed decoder bounds",
            )
        })?;
    let decoder = ZlibDecoder::new(idat.as_slice());
    let mut raw = Vec::with_capacity(expected_raw.min(MAX_IMAGE_JSON_BYTES));
    decoder
        .take(expected_raw as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| {
            crate::error::Error::new(
                crate::error::ErrorKind::Protocol,
                "image.http",
                "PNG IDAT is not valid zlib data",
            )
        })?;
    if raw.len() != expected_raw {
        return Err(crate::error::Error::new(
            crate::error::ErrorKind::Protocol,
            "image.http",
            "PNG decompressed data length does not match IHDR",
        ));
    }
    let row_bytes = (width as usize).checked_mul(channels).ok_or_else(|| {
        crate::error::Error::new(
            crate::error::ErrorKind::ResourceLimit,
            "image.http",
            "PNG row size exceeds decoder bounds",
        )
    })?;
    let mut previous = vec![0u8; row_bytes];
    let mut current = vec![0u8; row_bytes];
    let mut has_transparent_pixels = false;
    for row in 0..height as usize {
        let row_start = row * (row_bytes + 1);
        let filter = raw[row_start];
        let filtered = &raw[row_start + 1..row_start + 1 + row_bytes];
        for (index, value) in filtered.iter().copied().enumerate() {
            let left = if index >= channels {
                current[index - channels]
            } else {
                0
            };
            let up = previous[index];
            let upper_left = if index >= channels {
                previous[index - channels]
            } else {
                0
            };
            current[index] = match filter {
                0 => value,
                1 => value.wrapping_add(left),
                2 => value.wrapping_add(up),
                3 => value.wrapping_add(((u16::from(left) + u16::from(up)) / 2) as u8),
                4 => value.wrapping_add(paeth_predictor(left, up, upper_left)),
                _ => {
                    return Err(crate::error::Error::new(
                        crate::error::ErrorKind::Protocol,
                        "image.http",
                        "PNG row uses an unsupported filter",
                    ));
                }
            };
        }
        if matches!(color_type, 4 | 6) {
            let alpha_offset = if color_type == 4 { 1 } else { 3 };
            let pixel_channels = channels;
            has_transparent_pixels |= (0..width as usize)
                .any(|pixel| current[pixel * pixel_channels + alpha_offset] < u8::MAX);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(PngInfo {
        width,
        height,
        alpha_capable: matches!(color_type, 4 | 6),
        has_transparent_pixels,
    })
}

fn paeth_predictor(left: u8, up: u8, upper_left: u8) -> u8 {
    let left = i32::from(left);
    let up = i32::from(up);
    let upper_left = i32::from(upper_left);
    let estimate = left + up - upper_left;
    let left_distance = (estimate - left).abs();
    let up_distance = (estimate - up).abs();
    let upper_left_distance = (estimate - upper_left).abs();
    if left_distance <= up_distance && left_distance <= upper_left_distance {
        left as u8
    } else if up_distance <= upper_left_distance {
        up as u8
    } else {
        upper_left as u8
    }
}

fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use serde_json::json;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn auth_status(account_id: &str) -> Value {
        let payload = BASE64_STANDARD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id
                }
            }))
            .expect("JWT payload"),
        );
        let encoded_payload =
            URL_SAFE_NO_PAD.encode(BASE64_STANDARD.decode(payload).expect("payload bytes"));
        json!({
            "authMethod": "chatgpt",
            "authToken": format!("e30.{encoded_payload}.sig"),
            "requiresOpenaiAuth": true
        })
    }

    fn auth(account_id: &str) -> ChatGptImageAuth {
        ChatGptImageAuth::from_auth_status(&auth_status(account_id)).expect("auth status")
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(data.len() + 12);
        output.extend_from_slice(&(data.len() as u32).to_be_bytes());
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        output.extend_from_slice(&png_crc32(&output[4..]).to_be_bytes());
        output
    }

    fn png(color_type: u8, row: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();
        let mut encoder = ZlibEncoder::new(&mut compressed, Compression::default());
        encoder.write_all(row).expect("compress row");
        encoder.finish().expect("finish compressed row");
        let mut output = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, color_type, 0, 0, 0]);
        output.extend_from_slice(&chunk(b"IHDR", &ihdr));
        output.extend_from_slice(&chunk(b"IDAT", &compressed));
        output.extend_from_slice(&chunk(b"IEND", &[]));
        output
    }

    fn read_http_request(mut stream: &TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            let count = stream.read(&mut buffer).expect("request read");
            assert!(count > 0, "request must contain headers");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(
                bytes.len() < 512 * 1024,
                "request headers must remain bounded"
            );
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut buffer).expect("request body read");
            assert!(count > 0, "request body must complete");
            bytes.extend_from_slice(&buffer[..count]);
        }
        bytes
    }

    fn request_header(request: &[u8], name: &str) -> Option<String> {
        String::from_utf8_lossy(request).lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    fn serve_once(
        listener: TcpListener,
        status: u16,
        body: Vec<u8>,
    ) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request connection");
            let request = read_http_request(&stream);
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("response headers");
            stream.write_all(&body).expect("response body");
            request
        })
    }

    fn request() -> DirectImageRequest {
        DirectImageRequest {
            model: "gpt-image-1".to_owned(),
            prompt: "a red fox isolated on transparent background".to_owned(),
            background: ImageBackground::Transparent,
            size: ImageSize::Square,
            quality: ImageQuality::High,
        }
    }

    #[test]
    fn auth_status_requires_chatgpt_token_and_bounded_account_claim() {
        let parsed = ChatGptImageAuth::from_auth_status(&auth_status("account-test"))
            .expect("ChatGPT auth status");
        assert_eq!(parsed.account_id, "account-test");
        assert!(
            ChatGptImageAuth::from_auth_status(&json!({
                "authMethod": "apiKey",
                "authToken": "not-a-jwt"
            }))
            .is_err()
        );
    }

    #[test]
    fn endpoint_request_contains_auth_headers_and_explicit_image_controls() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
        let address = listener.local_addr().expect("listener address");
        let image = png(6, &[0, 255, 0, 0, 0]);
        let response = serde_json::to_vec(&json!({
            "created": 1,
            "data": [{"b64_json": BASE64_STANDARD.encode(&image)}]
        }))
        .expect("response JSON");
        let server = serve_once(listener, 200, response);
        let turn_id = image_turn_id();
        let result = send_image_request(
            &format!("http://{address}/images/generations"),
            &auth("account-test"),
            &request(),
            Duration::from_secs(5),
            &turn_id,
        )
        .expect("image response");
        let request_bytes = server.join().expect("server thread");
        let request_text = String::from_utf8_lossy(&request_bytes).to_ascii_lowercase();
        assert!(request_text.contains("authorization: bearer "));
        assert!(request_text.contains("chatgpt-account-id: account-test"));
        assert!(request_text.contains("version: 0.150.1"));
        assert!(request_text.contains("originator: codex_cli_rs"));
        assert!(request_text.contains("x-codex-image-turn-id: vergerail-image-"));
        let body = String::from_utf8_lossy(
            &request_bytes[request_bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("body separator")
                + 4..],
        );
        let body: Value = serde_json::from_str(&body).expect("request body JSON");
        assert_eq!(body["model"], "gpt-image-1");
        assert_eq!(body["n"], 1);
        assert_eq!(body["background"], "transparent");
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["quality"], "high");
        assert!(result.alpha_capable());
        assert_eq!(result.transparent_background(), Some(true));
    }

    #[test]
    fn endpoint_401_supports_one_caller_refresh_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
        let address = listener.local_addr().expect("listener address");
        let image = png(6, &[0, 255, 0, 0, 0]);
        let body = serde_json::to_vec(&json!({
            "data": [{"b64_json": BASE64_STANDARD.encode(&image)}]
        }))
        .expect("response JSON");
        let server = thread::spawn(move || {
            let (mut first_stream, _) = listener.accept().expect("first connection");
            let first_request = read_http_request(&first_stream);
            first_stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("401 response");
            let (mut second_stream, _) = listener.accept().expect("second connection");
            let second_request = read_http_request(&second_stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            );
            second_stream
                .write_all(response.as_bytes())
                .expect("response headers");
            second_stream.write_all(&body).expect("response body");
            (first_request, second_request)
        });
        let endpoint = format!("http://{address}/images/generations");
        let turn_id = image_turn_id();
        let first_result = send_image_request(
            &endpoint,
            &auth("account-test"),
            &request(),
            Duration::from_secs(5),
            &turn_id,
        );
        assert!(matches!(
            first_result,
            Err(ImageEndpointError::Unauthorized)
        ));
        let second_result = send_image_request(
            &endpoint,
            &auth("refreshed-account"),
            &request(),
            Duration::from_secs(5),
            &turn_id,
        )
        .expect("one refresh retry response");
        assert!(second_result.alpha_capable());
        let (first_request, second_request) = server.join().expect("server");
        assert!(
            String::from_utf8_lossy(&first_request)
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: account-test")
        );
        assert!(
            String::from_utf8_lossy(&second_request)
                .to_ascii_lowercase()
                .contains("chatgpt-account-id: refreshed-account")
        );
        assert_eq!(
            request_header(&first_request, "x-codex-image-turn-id"),
            request_header(&second_request, "x-codex-image-turn-id")
        );
    }

    #[test]
    fn endpoint_timeout_is_honored_instead_of_using_a_fixed_default() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("request connection");
            let _request = read_http_request(&stream);
            thread::sleep(Duration::from_millis(250));
        });
        let started = std::time::Instant::now();
        let result = send_image_request(
            &format!("http://{address}/images/generations"),
            &auth("account-test"),
            &request(),
            Duration::from_millis(50),
            &image_turn_id(),
        );
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(ImageEndpointError::Failed(error)) if error.kind() == crate::error::ErrorKind::Disconnected)
        );
        assert!(elapsed < Duration::from_millis(200), "elapsed={elapsed:?}");
        server.join().expect("server");
    }

    #[test]
    fn endpoint_other_status_is_not_retryable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local listener");
        let address = listener.local_addr().expect("listener address");
        let server = serve_once(listener, 429, Vec::new());
        let result = send_image_request(
            &format!("http://{address}/images/generations"),
            &auth("account-test"),
            &request(),
            Duration::from_secs(5),
            &image_turn_id(),
        );
        assert!(
            matches!(result, Err(ImageEndpointError::Failed(error)) if error.kind() == crate::error::ErrorKind::Rpc)
        );
        let request_bytes = server.join().expect("server");
        assert!(String::from_utf8_lossy(&request_bytes).contains("POST /images/generations"));
    }

    #[test]
    fn malformed_multiple_and_crc_invalid_png_results_are_rejected() {
        let image = png(6, &[0, 255, 0, 0, 0]);
        let one = || {
            serde_json::to_vec(&json!({"data": [{"b64_json": BASE64_STANDARD.encode(&image)}]}))
                .expect("JSON")
        };
        assert!(
            parse_image_endpoint_response(
                &serde_json::to_vec(&json!({"data": []})).expect("JSON"),
                ImageBackground::Auto
            )
            .is_err()
        );
        assert!(parse_image_endpoint_response(&serde_json::to_vec(&json!({"data": [{"b64_json": BASE64_STANDARD.encode(&image)}, {"b64_json": BASE64_STANDARD.encode(&image)}]})).expect("JSON"), ImageBackground::Auto).is_err());
        let mut invalid = image.clone();
        let crc = invalid.len() - 8;
        invalid[crc] ^= 1;
        assert!(parse_image_endpoint_response(&one(), ImageBackground::Auto).is_ok());
        assert!(
            parse_image_endpoint_response(
                &serde_json::to_vec(
                    &json!({"data": [{"b64_json": BASE64_STANDARD.encode(&invalid)}]})
                )
                .expect("JSON"),
                ImageBackground::Auto
            )
            .is_err()
        );
        let oversized = "A".repeat(MAX_IMAGE_RESPONSE_BYTES.div_ceil(3) * 4 + 1);
        assert!(
            parse_image_endpoint_response(
                &serde_json::to_vec(&json!({"data": [{"b64_json": oversized}]})).expect("JSON"),
                ImageBackground::Auto
            )
            .is_err()
        );

        let opaque = png(6, &[0, 255, 0, 0, 255]);
        let opaque_response = serde_json::to_vec(&json!({
            "data": [{"b64_json": BASE64_STANDARD.encode(&opaque)}]
        }))
        .expect("JSON");
        let parsed = parse_image_endpoint_response(&opaque_response, ImageBackground::Transparent)
            .expect("opaque RGBA PNG remains valid");
        assert!(parsed.alpha_capable());
        assert_eq!(parsed.transparent_background(), Some(false));
    }
}

/// Provider-reported failure for an image-generation item.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ImageGenerationFailure {
    /// The account reached an image-generation usage limit.
    UsageLimitExceeded {
        /// Provider limit identifier.
        limit_id: String,
        /// Unix timestamp at which the limit resets, when reported.
        resets_at: Option<i64>,
    },
}

impl ImageGenerationFailure {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::UsageLimitExceeded { limit_id, .. } => limit_id.len(),
        }
    }
}

/// One image-generation item from Codex app-server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGeneration {
    id: String,
    status: String,
    revised_prompt: Option<String>,
    result_base64: String,
    transparent_background: Option<bool>,
    failure: Option<ImageGenerationFailure>,
    saved_path: Option<PathBuf>,
}

impl ImageGeneration {
    pub(crate) fn new(
        id: String,
        status: String,
        revised_prompt: Option<String>,
        result_base64: String,
        transparent_background: Option<bool>,
        failure: Option<ImageGenerationFailure>,
        saved_path: Option<PathBuf>,
    ) -> Self {
        Self {
            id,
            status,
            revised_prompt,
            result_base64,
            transparent_background,
            failure,
            saved_path,
        }
    }

    /// Returns the Codex item identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the provider-defined lifecycle status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Returns the revised prompt produced by the image model, when reported.
    #[must_use]
    pub fn revised_prompt(&self) -> Option<&str> {
        self.revised_prompt.as_deref()
    }

    /// Returns the base64-encoded generated image bytes.
    ///
    /// The value is empty while generation is in progress or when generation failed.
    #[must_use]
    pub fn result_base64(&self) -> &str {
        &self.result_base64
    }

    /// Returns whether a transparent background was requested or selected, when reported.
    #[must_use]
    pub const fn transparent_background(&self) -> Option<bool> {
        self.transparent_background
    }

    /// Returns the typed provider failure, when generation failed.
    #[must_use]
    pub const fn failure(&self) -> Option<&ImageGenerationFailure> {
        self.failure.as_ref()
    }

    /// Returns the sandboxed saved path, when Codex persisted the generated image.
    #[must_use]
    pub fn saved_path(&self) -> Option<&Path> {
        self.saved_path.as_deref()
    }

    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        let mut bytes = self.id.len().checked_add(self.status.len())?;
        bytes = bytes.checked_add(self.result_base64.len())?;
        if let Some(prompt) = self.revised_prompt.as_deref() {
            bytes = bytes.checked_add(prompt.len())?;
        }
        if let Some(path) = self.saved_path.as_deref() {
            bytes = bytes.checked_add(path.as_os_str().len())?;
        }
        if let Some(failure) = self.failure.as_ref() {
            bytes = bytes.checked_add(failure.retained_bytes())?;
        }
        Some(bytes)
    }
}
