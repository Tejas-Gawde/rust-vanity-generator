mod api;
mod models;

use actix_web::{middleware, web, App, HttpServer};
use api::AppState;
use std::sync::Arc;

// Re-export lib items so `crate::` works from api.rs
pub use solana_vanity::{estimate_attempts, generate_vanity_keypair, validate_suffix, VanityConfig};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8282".to_string())
        .parse()
        .expect("PORT must be a valid u16");

    let state = Arc::new(AppState::new());

    log::info!("Starting Solana Vanity Address Generator API");
    log::info!("Listening on {}:{}", host, port);
    log::info!("Available CPU threads: {}", num_cpus::get());

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .wrap(middleware::Logger::default())
            .app_data(web::JsonConfig::default().limit(4096))
            .route("/health", web::get().to(api::health))
            .route("/api/generate", web::post().to(api::generate_sync))
            .route("/api/generate/async", web::post().to(api::generate_async))
            .route("/api/estimate", web::post().to(api::estimate))
            .route("/api/jobs", web::get().to(api::list_jobs))
            .route("/api/jobs/{job_id}", web::get().to(api::get_job_status))
            .route("/api/jobs/{job_id}", web::delete().to(api::delete_job))
    })
    .bind((host.as_str(), port))?
    .workers(4)
    .run()
    .await
}