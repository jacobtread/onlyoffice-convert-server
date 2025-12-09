use anyhow::Context;
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Response, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use axum_typed_multipart::{FieldData, TryFromMultipart, TypedMultipart};
use bytes::Bytes;
use clap::Parser;
use serde::Serialize;
use std::{
    env::temp_dir,
    path::{Path, PathBuf, absolute},
    sync::Arc,
};
use tokio::{process::Command, signal::ctrl_c, try_join};
use tracing::{debug, error};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::encrypted::{FileCondition, get_file_condition};

mod encrypted;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the x2t installation (Omit to determine automatically)
    #[arg(long)]
    x2t_path: Option<String>,

    /// Path to the converter fonts folder
    #[arg(long)]
    fonts_path: Option<String>,

    /// Port to bind the server to, defaults to 8080
    #[arg(long)]
    port: Option<u16>,

    /// Host to bind the server to, defaults to 0.0.0.0
    #[arg(long)]
    host: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    _ = dotenvy::dotenv();

    // Start configuring a `fmt` subscriber
    let subscriber = tracing_subscriber::fmt()
        // Use the logging options from env variables
        .with_env_filter(EnvFilter::from_default_env())
        // Display source code file paths
        .with_file(true)
        // Display source code line numbers
        .with_line_number(true)
        // Don't display the event's target (module path)
        .with_target(false)
        // Build the subscriber
        .finish();

    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    let args = Args::parse();

    let mut x2t_path: Option<PathBuf> = None;
    let mut fonts_path: Option<PathBuf> = None;

    // Try loading paths from command line
    if let Some(path) = args.x2t_path {
        x2t_path = Some(PathBuf::from(&path));
    }

    if let Some(path) = args.fonts_path {
        fonts_path = Some(PathBuf::from(&path));
    }

    // Try loading paths from environment variables
    if x2t_path.is_none()
        && let Ok(path) = std::env::var("X2T_PATH")
    {
        x2t_path = Some(PathBuf::from(&path));
    }

    if fonts_path.is_none()
        && let Ok(path) = std::env::var("X2T_FONTS_PATH")
    {
        fonts_path = Some(PathBuf::from(&path));
    }

    // Try determine default path
    if x2t_path.is_none() {
        let default_path = Path::new("/var/www/onlyoffice/documentserver/server/FileConverter/bin");

        if default_path.is_dir() {
            x2t_path = Some(default_path.to_path_buf());
        }
    }

    if fonts_path.is_none() {
        let default_path = Path::new("/var/www/onlyoffice/documentserver/fonts");
        fonts_path = Some(default_path.to_path_buf());
    }

    // Check a path was provided
    let x2t_path = match x2t_path {
        Some(value) => absolute(value).context("failed to make x2t path absolute")?,
        None => {
            error!("no x2t install path provided, cannot start server");
            panic!();
        }
    };

    let fonts_path = match fonts_path {
        Some(value) => absolute(value).context("failed to make fonts path absolute")?,
        None => {
            error!("no fonts path provided, cannot start server");
            panic!();
        }
    };

    tracing::debug!("using x2t install from: {}", x2t_path.display());

    let runtime_config = Arc::new(RuntimeConfig {
        x2t_path,
        fonts_path,
    });

    // Determine the address to run the server on
    let server_address = if args.host.is_some() || args.port.is_some() {
        let host = args.host.unwrap_or_else(|| "0.0.0.0".to_string());
        let port = args.port.unwrap_or(8080);

        format!("{host}:{port}")
    } else {
        std::env::var("SERVER_ADDRESS").context("missing SERVER_ADDRESS")?
    };

    // Create the router
    let app = Router::new()
        .route("/convert", post(convert))
        .layer(Extension(runtime_config))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024));

    // Create a TCP listener
    let listener = tokio::net::TcpListener::bind(&server_address)
        .await
        .context("failed to bind http server")?;

    debug!("server started on: {server_address}");

    // Serve the app from the listener
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            _ = ctrl_c().await;
            tracing::debug!("server shutting down");
        })
        .await
        .context("failed to serve")?;

    Ok(())
}

struct RuntimeConfig {
    x2t_path: PathBuf,
    fonts_path: PathBuf,
}

/// Request to convert a file
#[derive(TryFromMultipart)]
struct UploadAssetRequest {
    /// The file to convert
    #[form_data(limit = "unlimited")]
    file: FieldData<Bytes>,
}

struct ConvertTempPaths {
    config_path: PathBuf,
    input_path: PathBuf,
    output_path: PathBuf,
}

fn create_convert_temp_paths(temp_dir: &Path) -> std::io::Result<ConvertTempPaths> {
    // Generate random unique ID
    let random_id = Uuid::new_v4().simple();

    // Create paths in temp directory
    let config_path = temp_dir.join(format!("tmp_native_config_{random_id}.xml"));
    let input_path = temp_dir.join(format!("tmp_native_input_{random_id}"));
    let output_path = temp_dir.join(format!("tmp_native_output_{random_id}.pdf"));

    // Make paths absolute
    let config_path = absolute(config_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (config)"))?;
    let input_path = absolute(input_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (input)"))?;
    let output_path = absolute(output_path)
        .inspect_err(|err| tracing::error!(?err, "failed to make file path absolute (output)"))?;

    Ok(ConvertTempPaths {
        config_path,
        input_path,
        output_path,
    })
}

/// Determine the temporary directory to use and ensure it exists
async fn setup_temp_dir() -> std::io::Result<PathBuf> {
    let temp_dir = temp_dir();
    let temp_dir = temp_dir.join("onlyoffice-convert-server");

    // Ensure the temporary directory exists
    if !temp_dir.exists() {
        tokio::fs::create_dir_all(&temp_dir).await?;
    }

    Ok(temp_dir)
}

/// POST /convert
///
/// Converts the provided file to PDF format responding with the PDF file
async fn convert(
    Extension(runtime_config): Extension<Arc<RuntimeConfig>>,
    TypedMultipart(UploadAssetRequest { file }): TypedMultipart<UploadAssetRequest>,
) -> Result<Response<Body>, ErrorResponse> {
    // Setup temporary directory
    let temp_dir = setup_temp_dir().await.map_err(|err| {
        tracing::error!(?err, "failed to create temporary directory");
        ErrorResponse {
            code: None,
            message: "failed to create temporary directory".to_string(),
        }
    })?;

    // Create temporary path
    let ConvertTempPaths {
        config_path,
        input_path,
        output_path,
    } = create_convert_temp_paths(&temp_dir).map_err(|err| {
        tracing::error!(?err, "failed to setup temporary paths");
        ErrorResponse {
            code: None,
            message: "failed to setup temporary paths".to_string(),
        }
    })?;

    let config = format!(
        r#"
        <?xml version="1.0" encoding="utf-8"?>
        <TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                              xmlns:xsd="http://www.w3.org/2001/XMLSchema">
          <m_sFileFrom>{}</m_sFileFrom>
          <m_sFileTo>{}</m_sFileTo>
          <m_sFontDir>{}</m_sFontDir>
          <m_nFormatTo>513</m_nFormatTo>
        </TaskQueueDataConvert>
        "#,
        input_path.display(),
        output_path.display(),
        runtime_config.fonts_path.display(),
    );

    let result = x2t(
        &input_path,
        &config_path,
        &output_path,
        &runtime_config.x2t_path,
        &file.contents,
        config.as_bytes(),
    )
    .await;

    // Spawn a cleanup task
    tokio::spawn(async move {
        if input_path.exists()
            && let Err(err) = tokio::fs::remove_file(input_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }

        if config_path.exists()
            && let Err(err) = tokio::fs::remove_file(config_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }

        if output_path.exists()
            && let Err(err) = tokio::fs::remove_file(output_path).await
        {
            tracing::error!(?err, "failed to delete config file");
        }
    });

    let converted = match result {
        Ok(value) => value,
        Err(err) => return Err(err),
    };

    // Build the response
    let response = Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/pdf"),
        )
        .body(Body::from(converted))
        .map_err(|err| {
            tracing::error!(?err, "failed to make response");
            ErrorResponse {
                code: None,
                message: "failed to make response".to_string(),
            }
        })?;

    Ok(response)
}

#[cfg(not(windows))]
const X2T_BIN: &str = "x2t";
#[cfg(windows)]
const X2T_BIN: &str = "x2t.exe";

async fn x2t(
    input_path: &Path,
    config_path: &Path,
    output_path: &Path,
    x2t_path: &Path,
    input_bytes: &[u8],
    config_bytes: &[u8],
) -> Result<Vec<u8>, ErrorResponse> {
    let file_condition = get_file_condition(input_bytes);
    let write_file = tokio::fs::write(input_path, input_bytes);
    let write_config = tokio::fs::write(config_path, config_bytes);

    let x2t = x2t_path.join(X2T_BIN);
    let x2t = x2t.to_string_lossy();

    try_join!(write_config, write_file).map_err(|err| {
        tracing::error!(?err, "failed to write files");
        ErrorResponse {
            code: None,
            message: "failed to write files".to_string(),
        }
    })?;

    // Update the library path to include the x2t bin directory, fixes a bug where some of the requires
    // .so libraries aren't loaded when they need to be
    let ld_library_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let ld_library_path = format!("{}:{}", x2t_path.display(), ld_library_path);

    let output = Command::new(x2t.as_ref())
        .arg(config_path.display().to_string())
        .env("LD_LIBRARY_PATH", &ld_library_path)
        .output()
        .await
        .map_err(|err| {
            tracing::error!(?err, "failed to run x2t");
            ErrorResponse {
                code: None,
                message: "failed to run x2t".to_string(),
            }
        })?;

    if !output.status.success() {
        let error_code = output.status.code();
        let message = error_code
            .and_then(get_error_code_message)
            .unwrap_or("unknown error occurred");

        let stderr = String::from_utf8_lossy(&output.stderr);

        tracing::error!(
            "error processing file (stderr = {stderr}, exit code = {error_code:?}, file_condition = {file_condition:?})"
        );

        // Assume encryption for out of range crashes
        if stderr.contains("std::out_of_range") {
            return Err(ErrorResponse {
                code: error_code,
                message: "file is encrypted".to_string(),
            });
        }

        return Err(match file_condition {
            FileCondition::LikelyCorrupted => ErrorResponse {
                code: error_code,
                message: "file is corrupted".to_string(),
            },
            FileCondition::LikelyEncrypted => ErrorResponse {
                code: error_code,
                message: "file is encrypted".to_string(),
            },
            _ => ErrorResponse {
                code: error_code,
                message: message.to_string(),
            },
        });
    }

    // Read the output file back
    tokio::fs::read(output_path).await.map_err(|err| {
        tracing::error!(?err, "failed to read output");
        ErrorResponse {
            code: None,
            message: "failed to read output".to_string(),
        }
    })
}

/// Translate a x2t error code to the common x2t error messages
fn get_error_code_message(code: i32) -> Option<&'static str> {
    Some(match code {
        0x0001 => "AVS_FILEUTILS_ERROR_UNKNOWN",
        0x0050 => "AVS_FILEUTILS_ERROR_CONVERT",
        0x0051 => "AVS_FILEUTILS_ERROR_CONVERT_DOWNLOAD",
        0x0052 => "AVS_FILEUTILS_ERROR_CONVERT_UNKNOWN_FORMAT",
        0x0053 => "AVS_FILEUTILS_ERROR_CONVERT_TIMEOUT",
        0x0054 => "AVS_FILEUTILS_ERROR_CONVERT_READ_FILE",
        0x0055 => "AVS_FILEUTILS_ERROR_CONVERT_DRM_UNSUPPORTED",
        0x0056 => "AVS_FILEUTILS_ERROR_CONVERT_CORRUPTED",
        0x0057 => "AVS_FILEUTILS_ERROR_CONVERT_LIBREOFFICE",
        0x0058 => "AVS_FILEUTILS_ERROR_CONVERT_PARAMS",
        0x0059 => "AVS_FILEUTILS_ERROR_CONVERT_NEED_PARAMS",
        0x005a => "AVS_FILEUTILS_ERROR_CONVERT_DRM",
        0x005b => "AVS_FILEUTILS_ERROR_CONVERT_PASSWORD",
        0x005c => "AVS_FILEUTILS_ERROR_CONVERT_ICU",
        0x005d => "AVS_FILEUTILS_ERROR_CONVERT_LIMITS",
        0x005e => "AVS_FILEUTILS_ERROR_CONVERT_ROWLIMITS",
        0x005f => "AVS_FILEUTILS_ERROR_CONVERT_DETECT",
        0x0060 => "AVS_FILEUTILS_ERROR_CONVERT_CELLLIMITS",
        _ => return None,
    })
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub code: Option<i32>,
    pub message: String,
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
    }
}
