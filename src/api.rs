use crate::models::*;
use crate::{estimate_attempts, generate_vanity_keypair, validate_suffix, VanityConfig};
use actix_web::{web, HttpResponse};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

pub struct AppState {
    pub jobs: DashMap<String, JobStatusResponse>,
    pub max_suffix_len_sync: usize,
    pub max_suffix_len_async: usize,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            jobs: DashMap::new(),
            max_suffix_len_sync: 4,
            max_suffix_len_async: 7,
        }
    }
    pub fn active_job_count(&self) -> usize {
    self.jobs
        .iter()
        .filter(|entry| {
            let status = entry.value().status.clone();
            status == JobStatus::Running || status == JobStatus::Pending
        })
        .count()
}
    
}

// ─── Health Check ───────────────────────────────────────────────

pub async fn health(state: web::Data<Arc<AppState>>) -> HttpResponse {
    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        available_threads: num_cpus::get(),
        active_jobs: state.active_job_count(),
    })
}

// ─── Synchronous Generate ───────────────────────────────────────

pub async fn generate_sync(
    body: web::Json<GenerateRequest>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let suffix = body.suffix.trim().to_string();

    if let Err(e) = validate_suffix(&suffix) {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid suffix".to_string(),
            details: Some(e),
        });
    }

    if suffix.len() > state.max_suffix_len_sync {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: format!(
                "Suffix too long for synchronous generation (max {} chars). Use POST /api/generate/async for longer suffixes.",
                state.max_suffix_len_sync
            ),
            details: Some(format!(
                "Expected ~{:.0} attempts. Use the async endpoint instead.",
                estimate_attempts(suffix.len(), body.case_insensitive)
            )),
        });
    }

    let num_threads = body
        .threads
        .unwrap_or_else(num_cpus::get)
        .min(num_cpus::get());

    let config = VanityConfig {
        suffix: suffix.clone(),
        case_insensitive: body.case_insensitive,
        num_threads,
    };

    let result: Result<Option<solana_vanity::VanityResult>, actix_web::error::BlockingError> =
        web::block(move || {
            let found = Arc::new(AtomicBool::new(false));
            let counter = Arc::new(AtomicU64::new(0));
            generate_vanity_keypair(&config, found, counter, None)
        })
        .await;

    match result {
        Ok(Some(vanity_result)) => HttpResponse::Ok().json(GenerateResponse {
            public_key: vanity_result.public_key_base58,
            secret_key: vanity_result.full_keypair_bytes,
        }),
        Ok(None) => HttpResponse::InternalServerError().json(ErrorResponse {
            error: "Generation failed unexpectedly".to_string(),
            details: None,
        }),
        Err(e) => HttpResponse::InternalServerError().json(ErrorResponse {
            error: "Internal error".to_string(),
            details: Some(e.to_string()),
        }),
    }
}

// ─── Async Generate (Submit Job) ────────────────────────────────

pub async fn generate_async(
    body: web::Json<GenerateRequest>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let suffix = body.suffix.trim().to_string();

    if let Err(e) = validate_suffix(&suffix) {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid suffix".to_string(),
            details: Some(e),
        });
    }

    if suffix.len() > state.max_suffix_len_async {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: format!(
                "Suffix too long (max {} chars for async generation)",
                state.max_suffix_len_async
            ),
            details: None,
        });
    }

    if state.active_job_count() >= 5 {
        return HttpResponse::TooManyRequests().json(ErrorResponse {
            error: "Too many active jobs. Please wait for existing jobs to complete.".to_string(),
            details: None,
        });
    }

    let job_id = Uuid::new_v4().to_string();
    let num_threads = body
        .threads
        .unwrap_or_else(num_cpus::get)
        .min(num_cpus::get());
    let case_insensitive = body.case_insensitive;

    let config = VanityConfig {
        suffix: suffix.clone(),
        case_insensitive,
        num_threads,
    };

    state.jobs.insert(
        job_id.clone(),
        JobStatusResponse {
            job_id: job_id.clone(),
            status: JobStatus::Running,
            result: None,
            progress: Some(JobProgress {
                attempts: 0,
                elapsed_secs: 0.0,
                rate: "0 keys/s".to_string(),
            }),
            error: None,
        },
    );

    let state_clone = state.clone();
    let job_id_clone = job_id.clone();

    tokio::task::spawn_blocking(move || {
        let found = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = Arc::clone(&counter);
        let job_id_progress = job_id_clone.clone();
        let state_progress = state_clone.clone();
        let found_progress = Arc::clone(&found);

        let progress_handle = std::thread::spawn(move || {
            let start = Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));

                if found_progress.load(Ordering::Relaxed) {
                    break;
                }

                let elapsed = start.elapsed().as_secs_f64();
                let attempts = counter_clone.load(Ordering::Relaxed);
                let rate = if elapsed > 0.0 {
                    attempts as f64 / elapsed
                } else {
                    0.0
                };

                let rate_str = format_rate(rate);

                if let Some(mut entry) = state_progress.jobs.get_mut(&job_id_progress) {
                    entry.progress = Some(JobProgress {
                        attempts,
                        elapsed_secs: elapsed,
                        rate: rate_str,
                    });
                }
            }
        });

        let start = Instant::now();
        let result = generate_vanity_keypair(&config, found, counter, None);
        let elapsed = start.elapsed().as_secs_f64();

        let _ = progress_handle.join();

        match result {
            Some(vanity_result) => {
                if let Some(mut entry) = state_clone.jobs.get_mut(&job_id_clone) {
                    let rate = vanity_result.attempts as f64 / elapsed;
                    entry.status = JobStatus::Completed;
                    entry.result = Some(GenerateResultPayload {
                        public_key: vanity_result.public_key_base58,
                        secret_key: vanity_result.full_keypair_bytes,
                    });
                    entry.progress = Some(JobProgress {
                        attempts: vanity_result.attempts,
                        elapsed_secs: elapsed,
                        rate: format_rate(rate),
                    });
                    entry.error = None;
                }
            }
            None => {
                if let Some(mut entry) = state_clone.jobs.get_mut(&job_id_clone) {
                    entry.status = JobStatus::Failed;
                    entry.error = Some("Generation failed unexpectedly".to_string());
                }
            }
        }
    });

    HttpResponse::Accepted().json(JobSubmittedResponse {
        job_id,
        status: "running".to_string(),
        message: format!(
            "Job started. Expected ~{:.0} attempts. Poll GET /api/jobs/{{job_id}} for status.",
            estimate_attempts(suffix.len(), case_insensitive)
        ),
    })
}

// ─── Check Job Status ───────────────────────────────────────────

pub async fn get_job_status(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let job_id = path.into_inner();

    match state.jobs.get(&job_id) {
        Some(entry) => HttpResponse::Ok().json(entry.value().clone()),
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: "Job not found".to_string(),
            details: Some(format!("No job with ID: {}", job_id)),
        }),
    }
}

// ─── List All Jobs ──────────────────────────────────────────────

pub async fn list_jobs(state: web::Data<Arc<AppState>>) -> HttpResponse {
    let jobs: Vec<JobStatusResponse> = state
        .jobs
        .iter()
        .map(|entry| entry.value().clone())
        .collect();

    HttpResponse::Ok().json(jobs)
}

// ─── Delete Completed Job ───────────────────────────────────────

pub async fn delete_job(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let job_id = path.into_inner();

    match state.jobs.get(&job_id) {
        Some(entry) => {
            let status = entry.value().status.clone();
            drop(entry); // release the read lock before writing

            if status == JobStatus::Running || status == JobStatus::Pending {
                return HttpResponse::Conflict().json(ErrorResponse {
                    error: "Cannot delete a running job".to_string(),
                    details: None,
                });
            }

            state.jobs.remove(&job_id);
            HttpResponse::Ok().json(serde_json::json!({
                "message": "Job deleted",
                "jobId": job_id
            }))
        }
        None => HttpResponse::NotFound().json(ErrorResponse {
            error: "Job not found".to_string(),
            details: None,
        }),
    }
}

// ─── Estimate Difficulty ────────────────────────────────────────

pub async fn estimate(body: web::Json<GenerateRequest>) -> HttpResponse {
    let suffix = body.suffix.trim().to_string();

    if let Err(e) = validate_suffix(&suffix) {
        return HttpResponse::BadRequest().json(ErrorResponse {
            error: "Invalid suffix".to_string(),
            details: Some(e),
        });
    }

    let expected = estimate_attempts(suffix.len(), body.case_insensitive);
    let keys_per_sec = 400_000.0_f64;
    let est_seconds = expected / (keys_per_sec * num_cpus::get() as f64);

    HttpResponse::Ok().json(serde_json::json!({
        "suffix": suffix,
        "caseInsensitive": body.case_insensitive,
        "expectedAttempts": expected as u64,
        "estimatedSeconds": est_seconds,
        "estimatedHuman": format_duration(est_seconds),
        "difficulty": format!("1 in {:.0}", expected),
        "threads": num_cpus::get(),
    }))
}

// ─── Helpers ────────────────────────────────────────────────────

fn format_rate(rate: f64) -> String {
    if rate >= 1_000_000.0 {
        format!("{:.2}M keys/s", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.2}K keys/s", rate / 1_000.0)
    } else {
        format!("{:.0} keys/s", rate)
    }
}

fn format_duration(secs: f64) -> String {
    if secs < 1.0 {
        "< 1 second".to_string()
    } else if secs < 60.0 {
        format!("{:.1} seconds", secs)
    } else if secs < 3600.0 {
        format!("{:.1} minutes", secs / 60.0)
    } else if secs < 86400.0 {
        format!("{:.1} hours", secs / 3600.0)
    } else {
        format!("{:.1} days", secs / 86400.0)
    }
}