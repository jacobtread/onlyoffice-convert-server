pub mod client;
pub mod load;

use std::sync::Arc;

use bytes::Bytes;
pub use client::{ClientOptions, CreateError, OfficeConvertClient, RequestError};
pub use load::OfficeConvertLoadBalancer;

/// Office converter
#[derive(Clone)]
pub enum OfficeConverter {
    /// Recommended: Load balanced client that can handle blocking of the server
    LoadBalanced(Arc<OfficeConvertLoadBalancer>),
    /// Office client without any additional logic for handling a server
    /// being unavailable.
    Client(OfficeConvertClient),
}

impl OfficeConverter {
    /// Create a new converter from a client
    pub fn from_client(client: OfficeConvertClient) -> Self {
        Self::Client(client)
    }

    /// Create a new converter from a load balancer
    pub fn from_load_balancer(client: OfficeConvertLoadBalancer) -> Self {
        Self::LoadBalanced(Arc::new(client))
    }

    /// Converts the provided office file format bytes into a
    /// PDF returning the PDF file bytes
    ///
    /// ## Arguments
    /// * `file` - The file bytes to convert
    pub async fn convert(&self, file: Bytes) -> Result<Bytes, RequestError> {
        match self {
            OfficeConverter::LoadBalanced(inner) => inner.convert(file).await,
            OfficeConverter::Client(inner) => inner.convert(file).await,
        }
    }
}
