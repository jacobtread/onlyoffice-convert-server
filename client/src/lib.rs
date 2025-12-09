use bytes::Bytes;
use reqwest::{
    Body,
    multipart::{Form, Part},
};
use serde::Deserialize;
use std::{fmt::Display, sync::Arc, time::Duration};
use thiserror::Error;

#[derive(Clone)]
pub struct OnlyOfficeConvertClient {
    /// HTTP client to connect to the server with
    http: reqwest::Client,
    /// Host the office convert server is running on
    host: Arc<str>,
}

/// Errors that can occur during setup
#[derive(Debug, Error)]
pub enum CreateError {
    /// Builder failed to create HTTP client
    #[error(transparent)]
    Builder(reqwest::Error),
}

/// Errors that can occur during a request
#[derive(Debug, Error)]
pub enum RequestError {
    /// Failed to request the server
    #[error(transparent)]
    RequestFailed(reqwest::Error),

    /// Response from the server was invalid
    #[error(transparent)]
    InvalidResponse(reqwest::Error),

    /// Reached timeout when trying to connect
    #[error("server connection timed out")]
    ServerConnectTimeout,

    /// Error message from the convert server reply
    #[error("{0}")]
    ErrorResponse(ErrorResponse),
}

impl RequestError {
    // Whether a retry attempt should be made
    pub fn is_retry(&self) -> bool {
        matches!(
            self,
            RequestError::RequestFailed(_)
                | RequestError::InvalidResponse(_)
                | RequestError::ServerConnectTimeout
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    /// Error code from x2t if available
    pub code: Option<i32>,
    /// Server reason for the error
    pub reason: String,
    /// Server backtrace if available
    pub backtrace: Option<String>,
}

impl Display for ErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.code {
            write!(f, "{} (error_code = {})", self.reason, code)
        } else {
            self.reason.fmt(f)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Connection timeout used when checking the status of the server
    pub connect_timeout: Option<Duration>,

    /// Timeout when reading responses from the server
    pub read_timeout: Option<Duration>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            // Allow the connection to fail if not established in 700ms
            connect_timeout: Some(Duration::from_millis(700)),
            read_timeout: None,
        }
    }
}

impl OnlyOfficeConvertClient {
    /// Creates a new office convert client using the default options
    ///
    /// ## Arguments
    /// * `host` - The host where the server is located
    pub fn new<T>(host: T) -> Result<Self, CreateError>
    where
        T: Into<Arc<str>>,
    {
        Self::new_with_options(host, ClientOptions::default())
    }

    /// Creates a new office convert client using the provided options
    ///
    /// ## Arguments
    /// * `host` - The host where the server is located
    /// * `options` - The configuration options for the client
    pub fn new_with_options<T>(host: T, options: ClientOptions) -> Result<Self, CreateError>
    where
        T: Into<Arc<str>>,
    {
        let mut builder = reqwest::Client::builder();

        if let Some(connect_timeout) = options.connect_timeout {
            builder = builder.connect_timeout(connect_timeout);
        }

        if let Some(connect_timeout) = options.read_timeout {
            builder = builder.read_timeout(connect_timeout);
        }

        let client = builder.build().map_err(CreateError::Builder)?;
        Ok(Self::from_client(host, client))
    }

    /// Create an office convert client from an existing [reqwest::Client] if
    /// your setup is more advanced than the default configuration
    ///
    /// ## Arguments
    /// * `host` - The host where the server is located
    /// * `client` - The request HTTP client to use
    pub fn from_client<T>(host: T, client: reqwest::Client) -> Self
    where
        T: Into<Arc<str>>,
    {
        Self {
            http: client,
            host: host.into(),
        }
    }

    /// Converts the provided office file format bytes into a
    /// PDF returning the PDF file bytes
    ///
    /// ## Arguments
    /// * `file` - The file bytes to convert
    pub async fn convert(&self, file: impl Into<Body>) -> Result<Bytes, RequestError> {
        let route = format!("{}/convert", self.host);
        let form = Form::new().part("file", Part::stream(file));
        let response = self
            .http
            .post(route)
            .multipart(form)
            .send()
            .await
            .map_err(RequestError::RequestFailed)?;

        let status = response.status();

        // Handle error responses
        if status.is_client_error() || status.is_server_error() {
            let body: ErrorResponse = response
                .json()
                .await
                .map_err(RequestError::InvalidResponse)?;

            return Err(RequestError::ErrorResponse(body));
        }

        let response = response
            .bytes()
            .await
            .map_err(RequestError::InvalidResponse)?;

        Ok(response)
    }
}
