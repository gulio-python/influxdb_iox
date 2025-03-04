//! gRPC service implementations for `router`.

pub mod sharder;

use self::sharder::ShardService;
use crate::shard::Shard;
use ::sharder::Sharder;
use generated_types::influxdata::iox::{
    catalog::v1::*, object_store::v1::*, schema::v1::*, sharder::v1::*,
};
use iox_catalog::interface::Catalog;
use object_store::DynObjectStore;
use service_grpc_catalog::CatalogService;
use service_grpc_object_store::ObjectStoreService;
use service_grpc_schema::SchemaService;
use std::sync::Arc;

/// This type is responsible for managing all gRPC services exposed by `router`.
#[derive(Debug)]
pub struct GrpcDelegate<S> {
    catalog: Arc<dyn Catalog>,
    object_store: Arc<DynObjectStore>,
    shard_service: ShardService<S>,
}

impl<S> GrpcDelegate<S> {
    /// Initialise a new gRPC handler, dispatching DML operations to `dml_handler`.
    pub fn new(
        catalog: Arc<dyn Catalog>,
        object_store: Arc<DynObjectStore>,
        shard_service: ShardService<S>,
    ) -> Self {
        Self {
            catalog,
            object_store,
            shard_service,
        }
    }
}

impl<S> GrpcDelegate<S>
where
    S: Sharder<(), Item = Arc<Shard>> + Clone + 'static,
{
    /// Acquire a [`SchemaService`] gRPC service implementation.
    ///
    /// [`SchemaService`]: generated_types::influxdata::iox::schema::v1::schema_service_server::SchemaService.
    pub fn schema_service(&self) -> schema_service_server::SchemaServiceServer<SchemaService> {
        schema_service_server::SchemaServiceServer::new(SchemaService::new(Arc::clone(
            &self.catalog,
        )))
    }

    /// Acquire a [`CatalogService`] gRPC service implementation.
    ///
    /// [`CatalogService`]: generated_types::influxdata::iox::catalog::v1::catalog_service_server::CatalogService.
    pub fn catalog_service(
        &self,
    ) -> catalog_service_server::CatalogServiceServer<impl catalog_service_server::CatalogService>
    {
        catalog_service_server::CatalogServiceServer::new(CatalogService::new(Arc::clone(
            &self.catalog,
        )))
    }

    /// Acquire a [`ObjectStoreService`] gRPC service implementation.
    ///
    /// [`ObjectStoreService`]: generated_types::influxdata::iox::object_store::v1::object_store_service_server::ObjectStoreService.
    pub fn object_store_service(
        &self,
    ) -> object_store_service_server::ObjectStoreServiceServer<
        impl object_store_service_server::ObjectStoreService,
    > {
        object_store_service_server::ObjectStoreServiceServer::new(ObjectStoreService::new(
            Arc::clone(&self.catalog),
            Arc::clone(&self.object_store),
        ))
    }

    /// Return a gRPC [`ShardService`] handler.
    ///
    /// [`ShardService`]: generated_types::influxdata::iox::sharder::v1::shard_service_server::ShardService
    pub fn shard_service(
        &self,
    ) -> shard_service_server::ShardServiceServer<impl shard_service_server::ShardService> {
        shard_service_server::ShardServiceServer::new(self.shard_service.clone())
    }
}
