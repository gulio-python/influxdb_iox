use client_util::connection::GrpcConnection;

use self::generated_types::{namespace_service_client::NamespaceServiceClient, *};
use crate::connection::Connection;
use crate::error::Error;

/// Re-export generated_types
pub mod generated_types {
    pub use generated_types::influxdata::iox::namespace::v1::*;
}

/// A basic client for fetching the Schema for a Namespace.
#[derive(Debug, Clone)]
pub struct Client {
    inner: NamespaceServiceClient<GrpcConnection>,
}

impl Client {
    /// Creates a new client with the provided connection
    pub fn new(connection: Connection) -> Self {
        Self {
            inner: NamespaceServiceClient::new(connection.into_grpc_connection()),
        }
    }

    /// Get the available namespaces
    pub async fn get_namespaces(&mut self) -> Result<Vec<Namespace>, Error> {
        let response = self.inner.get_namespaces(GetNamespacesRequest {}).await?;

        Ok(response.into_inner().namespaces)
    }
}
