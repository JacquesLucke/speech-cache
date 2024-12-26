use actix_web::{web::Bytes, HttpResponse, HttpServer, Responder};
use config;
use lru_mem::{HeapSize, LruCache};
use parking_lot::Mutex;
use std::{hash::Hash, sync::Arc};

struct AppState {
    secrets: Secrets,
    shared: Arc<Mutex<SharedState>>,
}

struct SharedState {
    speech_cache: LruCache<OpenaiSpeechRequestInfo, Vec<u8>>,
}

#[derive(serde::Deserialize, Debug, Clone)]
struct Secrets {
    openai_key: String,
}

#[actix_web::get("/")]
async fn get_index() -> impl Responder {
    HttpResponse::Ok().body("A simple wrapper around a text-to-speech API for short pieces of text")
}

#[derive(serde::Deserialize, Debug)]
struct SpeechRequestParams {
    text: String,
    voice: Option<String>,
}

#[derive(serde::Serialize, Debug, Hash, PartialEq, Eq)]
struct OpenaiSpeechRequestInfo {
    model: String,
    voice: String,
    input: String,
    response_format: String,
}

impl HeapSize for OpenaiSpeechRequestInfo {
    fn heap_size(&self) -> usize {
        return self.model.capacity()
            + self.voice.capacity()
            + self.input.capacity()
            + self.response_format.capacity();
    }
}

#[actix_web::get("/speak")]
async fn get_speech(
    state: actix_web::web::Data<AppState>,
    info: actix_web::web::Query<SpeechRequestParams>,
) -> impl Responder {
    if info.text.len() > 100 {
        return HttpResponse::BadRequest().body("text too long");
    }

    let openai_params = OpenaiSpeechRequestInfo {
        model: "tts-1".to_string(),
        voice: info.voice.clone().unwrap_or("echo".to_string()),
        input: info.text.clone(),
        response_format: "mp3".to_string(),
    };

    match state.shared.lock().speech_cache.get(&openai_params) {
        Some(cached) => {
            return HttpResponse::Ok()
                .content_type("audio/mpeg")
                .body(Bytes::from(cached.clone()));
        }
        None => {}
    }

    let client = reqwest::Client::new();
    match client
        .post("https://api.openai.com/v1/audio/speech")
        .bearer_auth(state.secrets.openai_key.clone())
        .json(&openai_params)
        .send()
        .await
    {
        Err(err) => HttpResponse::InternalServerError().body(format!("Error: {:?}", err)),
        Ok(res) => {
            if res.status() != 200 {
                return HttpResponse::InternalServerError().body(format!("Invalid: {:?}", res));
            }
            let result_bytes = res.bytes().await.unwrap();
            let _ = state
                .shared
                .lock()
                .speech_cache
                .insert(openai_params, result_bytes.clone().into());
            HttpResponse::Ok()
                .content_type("audio/mpeg")
                .body(result_bytes)
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("secrets.toml"))
        .build()
        .unwrap();

    let secrets: Secrets = settings.try_deserialize().unwrap();
    let shared = Arc::new(Mutex::new(SharedState {
        speech_cache: LruCache::new(16 * 1024 * 1024),
    }));

    HttpServer::new(move || {
        actix_web::App::new()
            .app_data(actix_web::web::Data::new(AppState {
                secrets: secrets.clone(),
                shared: shared.clone(),
            }))
            .service(get_index)
            .service(get_speech)
            .wrap(actix_cors::Cors::permissive())
    })
    .workers(1)
    .bind("127.0.0.1:8080")?
    .run()
    .await
}
