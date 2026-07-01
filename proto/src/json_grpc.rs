use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::header::{CONTENT_TYPE, HeaderValue};
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tonic::{Code, Status};

/// Maximum JSON request body accepted by the gRPC JSON adapter.
pub const MAX_JSON_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

#[macro_export]
macro_rules! include_proto {
    ($package: tt) => {
        include!(concat!(env!("OUT_DIR"), concat!("/", $package, ".rs")));
        include!(concat!(env!("OUT_DIR"), concat!("/", $package, ".serde.rs")));
    };
}

#[derive(Debug, Serialize)]
struct JsonErrorEnvelope {
    #[serde(with = "grpc_code_as_i32")]
    code: Code,
    message: String,
    details: Vec<u8>,
}

/// Returns whether the request content type is exactly `application/json`.
#[inline(always)]
pub fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(Ok(content_type)) = headers.get(CONTENT_TYPE).map(HeaderValue::to_str) else {
        return false;
    };
    content_type.eq_ignore_ascii_case("application/json")
}

/// Reads, bounds, and decodes a JSON request body into a protobuf JSON message type.
#[inline(always)]
pub async fn decode_json_body<B, T>(req: http::Request<B>) -> Result<T, Status>
where
    B: tonic::codegen::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
    T: DeserializeOwned,
{
    let body = Limited::new(req.into_body(), MAX_JSON_REQUEST_BODY_BYTES)
        .collect()
        .await
        .map_err(|err| {
            if err.downcast_ref::<LengthLimitError>().is_some() {
                Status::resource_exhausted("JSON request body exceeds 4 MiB limit")
            } else {
                Status::invalid_argument(format!("failed to read request body: {err}"))
            }
        })?
        .to_bytes();

    serde_json::from_slice(&body).map_err(|err| Status::invalid_argument(format!("invalid JSON body: {err}")))
}

/// Serializes one successful unary JSON response.
#[inline(always)]
pub fn unary_json_response<T>(message: &T) -> http::Response<tonic::body::Body>
where
    T: Serialize,
{
    match serde_json::to_vec(message) {
        Ok(body) => response(StatusCode::OK, "application/json", body),
        Err(err) => json_error_response(Status::internal(format!("failed to serialize response JSON: {err}"))),
    }
}

/// Serializes a server stream as newline-delimited JSON.
#[inline(always)]
pub fn json_stream_response<S, T>(stream: S) -> http::Response<tonic::body::Body>
where
    S: Stream<Item = Result<T, Status>> + Send + 'static,
    T: Serialize + 'static,
{
    let ndjson = futures::stream::unfold((Box::pin(stream), false), |(mut stream, done)| async move {
        if done {
            return None;
        }

        match stream.next().await {
            Some(message) => match json_line(message) {
                Ok(line) => Some((line, (stream, false))),
                Err(err_line) => Some((err_line, (stream, true))),
            },
            None => None,
        }
    });

    let framed = ndjson.map(Ok::<Bytes, std::convert::Infallible>);
    let body = axum::body::Body::from_stream(framed);
    response_with_body(StatusCode::OK, "application/x-ndjson", tonic::body::Body::new(body))
}

/// Serializes a tonic status as a JSON error response with an HTTP status mapping.
#[inline(always)]
pub fn json_error_response(status: Status) -> http::Response<tonic::body::Body> {
    let status_code = grpc_code_to_http_status(status.code());
    let body = json_error_body(status);
    response(status_code, "application/json", body)
}

/// Builds the JSON response for an unsupported content type.
#[inline(always)]
pub fn unsupported_media_type_response() -> http::Response<tonic::body::Body> {
    json_error_response(Status::invalid_argument("unsupported content-type for RPC path"))
}

/// Builds the JSON response for an unknown RPC path.
#[inline(always)]
pub fn unknown_rpc_path_response() -> http::Response<tonic::body::Body> {
    json_error_response(Status::not_found("unknown RPC method path"))
}

/// Builds the JSON response for a streaming mode unsupported by the adapter.
#[inline(always)]
pub fn unsupported_streaming_kind_response() -> http::Response<tonic::body::Body> {
    json_error_response(Status::unimplemented(
        "JSON adapter supports only unary and server-streaming methods",
    ))
}

/// Builds the JSON response for non-POST JSON RPC requests.
#[inline(always)]
pub fn method_not_allowed_response() -> http::Response<tonic::body::Body> {
    json_error_response(Status::invalid_argument("JSON RPC requires POST method"))
}

#[inline(always)]
fn json_error_body(status: Status) -> Vec<u8> {
    let envelope = JsonErrorEnvelope {
        code: status.code(),
        message: status.message().to_owned(),
        details: status.details().to_vec(),
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|_| {
        Vec::from("{{\"code\":13,\"message\":\"failed to serialize JSON error envelope\",\"details\":[]}}")
    })
}

#[inline(always)]
fn response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> http::Response<tonic::body::Body> {
    let body = tonic::body::Body::new(Full::new(Bytes::from(body)));
    response_with_body(status, content_type, body)
}

#[inline(always)]
fn response_with_body(
    status: StatusCode, content_type: &'static str, body: tonic::body::Body,
) -> http::Response<tonic::body::Body> {
    let mut response = http::Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[inline(always)]
fn json_line<T>(value: Result<T, Status>) -> Result<Bytes, Bytes>
where
    T: Serialize,
{
    let message = value.map_err(json_error_body)?;
    let mut bytes = serde_json::to_vec(&message).map_err(|err| {
        json_error_body(Status::internal(format!(
            "failed to serialize stream JSON message: {err}"
        )))
    })?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

/// Maps a tonic gRPC code to the HTTP status used by the JSON adapter.
#[inline(always)]
#[allow(
    clippy::match_same_arms,
    reason = "Unknown/Internal/DataLoss are listed explicitly even though the non-exhaustive `_` arm maps the same"
)]
pub fn grpc_code_to_http_status(code: Code) -> StatusCode {
    match code {
        Code::Cancelled => StatusCode::from_u16(499).expect("499 should be valid"),
        Code::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => StatusCode::BAD_REQUEST,
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::AlreadyExists | Code::Aborted => StatusCode::CONFLICT,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

mod grpc_code_as_i32 {
    /// Serializes a tonic code as its protobuf integer value.
    #[inline(always)]
    pub(super) fn serialize<S>(code: &tonic::Code, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(*code as i32)
    }
}
