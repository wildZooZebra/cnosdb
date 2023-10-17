use std::collections::HashSet;
use std::convert::Infallible as StdInfallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::info;
use warp::{hyper, Filter};

use crate::error::{MetaError, MetaResult};
use crate::store::command::*;
use crate::store::storage::StateMachine;

pub async fn start_singe_meta_server(path: String, cluster_name: String, addr: String) {
    let db_path = format!("{}/meta/{}.data", path, 0);
    let storage = StateMachine::open(db_path).unwrap();

    let init_data = crate::store::config::MetaInit {
        cluster_name,
        admin_user: models::auth::user::ROOT.to_string(),
        system_tenant: models::schema::DEFAULT_CATALOG.to_string(),
        default_database: vec![
            models::schema::USAGE_SCHEMA.to_string(),
            models::schema::DEFAULT_DATABASE.to_string(),
        ],
    };
    super::init::init_meta(&storage, init_data).await;

    let storage = Arc::new(RwLock::new(storage));
    let server = SingleServer { storage };
    tracing::info!("single meta http server start addr: {}", addr);
    tokio::spawn(async move { server.start(addr).await });
}

pub struct SingleServer {
    pub storage: Arc<RwLock<StateMachine>>,
}

impl SingleServer {
    pub async fn start(&self, addr: String) {
        let addr: SocketAddr = addr.parse().unwrap();
        warp::serve(self.routes()).run(addr).await;
    }

    fn routes(
        &self,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        self.read()
            .or(self.write())
            .or(self.watch())
            .or(self.debug())
    }

    fn with_storage(
        &self,
    ) -> impl Filter<Extract = (Arc<RwLock<StateMachine>>,), Error = StdInfallible> + Clone {
        let storage = self.storage.clone();
        warp::any().map(move || storage.clone())
    }

    fn read(&self) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("read")
            .and(warp::body::bytes())
            .and(self.with_storage())
            .and_then(
                |req: hyper::body::Bytes, storage: Arc<RwLock<StateMachine>>| async move {
                    let req: ReadCommand = serde_json::from_slice(&req)
                        .map_err(MetaError::from)
                        .map_err(warp::reject::custom)?;

                    let rsp = storage.read().await.process_read_command(&req);
                    let res: Result<String, warp::Rejection> = Ok(rsp);
                    res
                },
            )
    }

    fn write(&self) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("write")
            .and(warp::body::bytes())
            .and(self.with_storage())
            .and_then(
                |req: hyper::body::Bytes, storage: Arc<RwLock<StateMachine>>| async move {
                    let req: WriteCommand = serde_json::from_slice(&req)
                        .map_err(MetaError::from)
                        .map_err(warp::reject::custom)?;

                    let rsp = storage.write().await.process_write_command(&req);
                    let res: Result<String, warp::Rejection> = Ok(rsp);
                    res
                },
            )
    }

    fn watch(&self) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("watch")
            .and(warp::body::bytes())
            .and(self.with_storage())
            .and_then(
                |req: hyper::body::Bytes, storage: Arc<RwLock<StateMachine>>| async move {
                    let data = Self::process_watch(req, storage)
                        .await
                        .map_err(warp::reject::custom)?;

                    let res: Result<String, warp::Rejection> = Ok(data);
                    res
                },
            )
    }

    fn debug(&self) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("debug").and(self.with_storage()).and_then(
            |storage: Arc<RwLock<StateMachine>>| async move {
                let data = storage
                    .read()
                    .await
                    .dump()
                    .await
                    .map_err(warp::reject::custom)?;

                let res: Result<String, warp::Rejection> = Ok(data);
                res
            },
        )
    }

    pub async fn process_watch(
        req: hyper::body::Bytes,
        storage: Arc<RwLock<StateMachine>>,
    ) -> MetaResult<String> {
        let req: (String, String, HashSet<String>, u64) = serde_json::from_slice(&req)?;
        let (client, cluster, tenants, base_ver) = req;
        info!(
            "watch all  args: client-id: {}, cluster: {}, tenants: {:?}, version: {}",
            client, cluster, tenants, base_ver
        );

        let mut notify = {
            let storage = storage.read().await;
            let watch_data = storage.read_change_logs(&cluster, &tenants, base_ver);
            if watch_data.need_return(base_ver) {
                return Ok(crate::store::storage::response_encode(Ok(watch_data)));
            }

            storage.watch.subscribe()
        };

        let mut follow_ver = base_ver;
        let now = std::time::Instant::now();
        loop {
            let _ = tokio::time::timeout(Duration::from_secs(20), notify.recv()).await;

            let watch_data = storage
                .read()
                .await
                .read_change_logs(&cluster, &tenants, follow_ver);
            info!("watch notify {} {}.{}", client, base_ver, follow_ver);
            if watch_data.need_return(base_ver) || now.elapsed() > Duration::from_secs(30) {
                return Ok(crate::store::storage::response_encode(Ok(watch_data)));
            }

            if follow_ver < watch_data.max_ver {
                follow_ver = watch_data.max_ver;
            }
        }
    }
}