use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    engine::{
        pipeline::{Pipeline, PipelineStep, Status},
        Engine, EngineError,
    },
    metrics::metrics_handler,
};

#[derive(Debug)]
pub enum EngineMessage {
    AddPipeline {
        pipeline: Pipeline,
        response_tx: oneshot::Sender<Result<(), EngineError>>,
    },
    GetPipeline {
        pipeline_id: Uuid,
        response_tx: oneshot::Sender<Result<Pipeline, EngineError>>,
    },
    DeletePipeline {
        pipeline_id: Uuid,
        response_tx: oneshot::Sender<Result<(), EngineError>>,
    },
}

pub struct AppState {
    engine_bridge_tx: mpsc::Sender<EngineMessage>,
}

pub async fn run() -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel(1000);
    let mut engine = match Engine::from_env().await {
        Ok(engine) => engine,
        Err(e) => {
            tracing::error!("Failed to create engine: {}", e);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Failed to create engine",
            ));
        }
    };

    // Create a shutdown signal handler
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
    let shutdown_tx_clone = shutdown_tx.clone();

    // Set up ctrl-c handler
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            let _ = shutdown_tx_clone.send(()).await;
        }
    });

    // Main application server with metrics endpoint
    let server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(AppState {
                engine_bridge_tx: tx.clone(),
            }))
            .wrap(middleware::Logger::default())
            .service(
                web::scope("/api")
                    .route("/healthz", web::get().to(healthz))
                    .route("/pipeline", web::post().to(create_pipeline)),
            )
            .route("/metrics", web::get().to(metrics_handler))
    })
    .bind(("0.0.0.0", 6966))?
    .run();

    tokio::select! {
        result = server => {
            let _ = shutdown_tx.send(()).await;
            if let Err(e) = result {
                tracing::error!("Server error: {}", e);
            }
        }
        result = engine.run(rx) => {
            let _ = shutdown_tx.send(()).await;
            if let Err(e) = result {
                tracing::error!("Engine error: {}", e);
            }
        }
        _ = shutdown_rx.recv() => {
            tracing::info!("Shutdown signal received, starting graceful shutdown");
        }
    }

    tracing::info!("Server shutdown complete");
    Ok(())
}

async fn healthz() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy"
    }))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreatePipelineRequest {
    pub user_id: String,
    pub current_steps: Vec<Uuid>,
    pub steps: HashMap<Uuid, PipelineStep>,
}

impl From<CreatePipelineRequest> for Pipeline {
    fn from(req: CreatePipelineRequest) -> Self {
        Self {
            id: Uuid::new_v4(),
            user_id: req.user_id,
            current_steps: req.current_steps,
            steps: req.steps,
            status: Status::Pending,
            created_at: Utc::now(),
        }
    }
}

async fn create_pipeline(
    state: Data<AppState>,
    req: web::Json<CreatePipelineRequest>,
) -> impl Responder {
    let start = std::time::Instant::now();
    metrics::counter!("pipeline_creation_attempts", 1);

    let pipeline: Pipeline = req.into_inner().into();

    // Create oneshot channel for response
    let (response_tx, response_rx) = oneshot::channel();

    // Send message to engine
    if let Err(e) = state
        .engine_bridge_tx
        .send(EngineMessage::AddPipeline {
            pipeline,
            response_tx,
        })
        .await
    {
        metrics::counter!("pipeline_creation_errors", 1);
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "status": "error",
            "message": format!("Failed to communicate with engine: {}", e)
        }));
    }

    // Wait for response with timeout
    let result = match tokio::time::timeout(std::time::Duration::from_secs(5), response_rx).await {
        Ok(response) => match response {
            Ok(Ok(_)) => {
                metrics::counter!("pipeline_creation_success", 1);
                HttpResponse::Created().json(serde_json::json!({
                    "status": "success",
                    "message": "Pipeline created successfully"
                }))
            }
            Ok(Err(e)) => {
                metrics::counter!("pipeline_creation_errors", 1);
                HttpResponse::InternalServerError().json(serde_json::json!({
                    "status": "error",
                    "message": format!("Failed to create pipeline: {}", e)
                }))
            }
            Err(e) => {
                metrics::counter!("pipeline_creation_errors", 1);
                HttpResponse::InternalServerError().json(serde_json::json!({
                    "status": "error",
                    "message": format!("Failed to receive response from engine: {}", e)
                }))
            }
        },
        Err(_) => {
            metrics::counter!("pipeline_creation_errors", 1);
            HttpResponse::GatewayTimeout().json(serde_json::json!({
                "status": "error",
                "message": "Pipeline creation timed out"
            }))
        }
    };

    metrics::histogram!("pipeline_creation_duration", start.elapsed());
    result
}
