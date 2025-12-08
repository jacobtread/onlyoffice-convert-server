use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::DefaultBodyLimit,
    http::{HeaderValue, Response, header},
    routing::post,
};
use axum_typed_multipart::{FieldData, TryFromMultipart, TypedMultipart};
use bytes::Bytes;
use clap::Parser;
use error::DynHttpError;
use rand::{Rng, distributions::Alphanumeric};
use std::{
    env::temp_dir,
    path::{Path, PathBuf, absolute},
};
use tokio::{process::Command, signal::ctrl_c};
use tracing::{debug, error};
use tracing_subscriber::EnvFilter;

mod encrypted;
mod error;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the x2t installation (Omit to determine automatically)
    #[arg(long)]
    x2t_path: Option<String>,

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

    // Try loading office path from command line
    if let Some(path) = args.x2t_path {
        x2t_path = Some(PathBuf::from(&path));
    }

    // Try loading x2t path from environment variables
    if x2t_path.is_none()
        && let Ok(path) = std::env::var("X2T_PATH")
    {
        x2t_path = Some(PathBuf::from(&path));
    }

    // Try determine default office path
    if x2t_path.is_none() {
        let default_path = Path::new("/var/www/onlyoffice/documentserver/server/FileConverter/bin");

        if default_path.is_dir() {
            x2t_path = Some(default_path.to_path_buf());
        }
    }

    // Check a path was provided
    let office_path = match x2t_path {
        Some(value) => value,
        None => {
            error!("no x2t install path provided, cannot start server");
            panic!();
        }
    };

    tracing::debug!("using x2t install from: {}", office_path.display());

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

/// Request to convert a file
#[derive(TryFromMultipart)]
struct UploadAssetRequest {
    /// The file to convert
    #[form_data(limit = "unlimited")]
    file: FieldData<Bytes>,
}

/// POST /convert
///
/// Converts the provided file to PDF format responding with the PDF file
async fn convert(
    TypedMultipart(UploadAssetRequest { file }): TypedMultipart<UploadAssetRequest>,
) -> Result<Response<Body>, DynHttpError> {
    let bytes = file.contents;
    let tmp_dir = temp_dir();

    // Generate random ID for the path name
    let random_id = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(|value| value as char)
        .collect::<String>();

    // Use our own special temp directory
    let tmp_dir = tmp_dir.join("onlyoffice-convert-server");

    // Delete the temp directory if it already exists
    if !tmp_dir.exists() {
        std::fs::create_dir_all(&tmp_dir).context("failed to create temporary directory")?;
    }

    // Create input and output paths
    let temp_config = tmp_dir.join(format!("tmp_native_config_{random_id}.xml"));
    let temp_in = tmp_dir.join(format!("tmp_native_input_{random_id}"));
    let temp_out = tmp_dir.join(format!("tmp_native_output_{random_id}.pdf"));

    let config_abs_path = absolute(temp_config).context("failed to get config path")?;
    let in_abs_path = absolute(temp_in).context("failed to get in path")?;
    let out_abs_path = absolute(temp_out).context("failed to get out path")?;

    let font_path = Path::new("/var/www/onlyoffice/documentserver/fonts");

    let config = format!(
        r#"
        <?xml version="1.0" encoding="utf-8"?>
        <TaskQueueDataConvert xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                              xmlns:xsd="http://www.w3.org/2001/XMLSchema">
          <m_sKey>{}</m_sKey>
          <m_sFileFrom>{}</m_sFileFrom>
          <m_sFileTo>{}</m_sFileTo>
          <m_sFontDir>{}</m_sFontDir>
          <m_nFormatTo>513</m_nFormatTo>
          <m_bEmbeddedFonts>false</m_bEmbeddedFonts>
        </TaskQueueDataConvert>
        "#,
        random_id,
        in_abs_path.display(),
        out_abs_path.display(),
        font_path.display(),
    );

    tokio::fs::write(&config_abs_path, config.as_bytes())
        .await
        .unwrap();

    tokio::fs::write(in_abs_path, bytes.as_ref()).await.unwrap();

    let ld_library_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    let output = Command::new("/var/www/onlyoffice/documentserver/server/FileConverter/bin/x2t")
        .arg(config_abs_path.display().to_string())
        .env(
            "LD_LIBRARY_PATH",
            &format!(
                "/var/www/onlyoffice/documentserver/server/FileConverter/bin/:{ld_library_path}"
            ),
        )
        .output()
        .await
        .unwrap();

    if !output.status.success() {
        tracing::error!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(anyhow::anyhow!("err").into());
    }

    let converted = tokio::fs::read(out_abs_path).await.unwrap();

    // Build the response
    let response = Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/pdf"),
        )
        .body(Body::from(converted))
        .context("failed to create response")?;

    Ok(response)
}

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "djvu", "doc", "docm", "docx", "dotx", "epub", "fb2", "html", "mhtml", "jpg", "odt", "ott",
    "png", "rtf", "txt", "stw", "sxw", "wps", "wpt", "xml", "xps",
];
