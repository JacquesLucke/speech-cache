use actix_web::{web::Bytes, HttpResponse, HttpServer, Responder};
use clap::Parser;
use config;
use lru_mem::{HeapSize, LruCache};
use parking_lot::Mutex;
use std::io::Cursor;
use std::net::TcpListener;
use std::{hash::Hash, sync::Arc};
use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::codecs::DecoderOptions;

struct AppState {
    secrets: Secrets,
    shared: Arc<Mutex<SharedState>>,
}

struct SharedState {
    speech_cache: LruCache<CacheKey, Vec<u8>>,
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
    volume: Option<ordered_float::NotNan<f32>>,
}

#[derive(serde::Serialize, Debug, Hash, PartialEq, Eq, Clone)]
struct OpenaiSpeechRequestInfo {
    model: String,
    voice: String,
    input: String,
    response_format: String,
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    request: OpenaiSpeechRequestInfo,
    volume_factor: ordered_float::NotNan<f32>,
}

impl HeapSize for OpenaiSpeechRequestInfo {
    fn heap_size(&self) -> usize {
        return self.model.capacity()
            + self.voice.capacity()
            + self.input.capacity()
            + self.response_format.capacity();
    }
}

impl HeapSize for CacheKey {
    fn heap_size(&self) -> usize {
        return self.request.heap_size() + self.volume_factor.heap_size();
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

    let volume_factor = info
        .volume
        .unwrap_or(ordered_float::NotNan::new(1.0).unwrap());

    let openai_params = OpenaiSpeechRequestInfo {
        model: "tts-1".to_string(),
        voice: info.voice.clone().unwrap_or("echo".to_string()),
        input: info.text.clone(),
        response_format: "mp3".to_string(),
    };

    let cache_key = CacheKey {
        request: openai_params.clone(),
        volume_factor,
    };

    const CACHE_DURATION: u64 = 60 * 60 * 24 * 7;

    match state.shared.lock().speech_cache.get(&cache_key) {
        Some(cached) => {
            return HttpResponse::Ok()
                .content_type("audio/mpeg")
                .insert_header((
                    actix_web::http::header::CACHE_CONTROL,
                    format!("max-age={}", CACHE_DURATION),
                ))
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
            let result_bytes = apply_volume_factor(result_bytes, volume_factor);
            let _ = state
                .shared
                .lock()
                .speech_cache
                .insert(cache_key, result_bytes.clone().into());
            HttpResponse::Ok()
                .content_type("audio/mpeg")
                .insert_header((
                    actix_web::http::header::CACHE_CONTROL,
                    format!("max-age={}", CACHE_DURATION),
                ))
                .body(result_bytes)
        }
    }
}

fn apply_volume_factor(audio_file: Bytes, volume_factor: ordered_float::NotNan<f32>) -> Vec<u8> {
    let mss = symphonia::core::io::MediaSourceStream::new(
        Box::new(Cursor::new(audio_file)),
        Default::default(),
    );
    let probe = symphonia::default::get_probe()
        .format(
            &Default::default(),
            mss,
            &Default::default(),
            &Default::default(),
        )
        .expect("Unsupported format");
    let mut format = probe.format;
    let track = &format.tracks()[0];
    let sample_rate = track.codec_params.sample_rate.unwrap();
    let channels = 1;

    // Create a decoder for the audio track.
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("Failed to create decoder");

    let mut all_samples: Vec<f32> = Vec::new();

    // Decode and process the audio packets.
    while let Ok(packet) = format.next_packet() {
        // Decode the packet into audio frames.
        if let Ok(decoded) = decoder.decode(&packet) {
            let mut converted =
                AudioBuffer::<f32>::new(decoded.capacity() as u64, decoded.spec().clone());
            decoded.convert(&mut converted);
            all_samples.extend(converted.chan(0));
        }
    }

    for sample in all_samples.iter_mut() {
        *sample *= volume_factor.into_inner();
    }

    let mut mp3_encoder = mp3lame_encoder::Builder::new().expect("Create LAME builder");
    mp3_encoder
        .set_num_channels(channels as u8)
        .expect("set channels");
    mp3_encoder
        .set_sample_rate(sample_rate as u32)
        .expect("set sample rate");
    mp3_encoder
        .set_brate(mp3lame_encoder::Bitrate::Kbps192)
        .expect("set brate");
    mp3_encoder
        .set_quality(mp3lame_encoder::Quality::Best)
        .expect("set quality");
    let mut mp3_encoder = mp3_encoder.build().expect("To initialize LAME encoder");

    let input = mp3lame_encoder::MonoPcm(&all_samples);

    let mut mp3_out_buffer = Vec::new();
    mp3_out_buffer.reserve(mp3lame_encoder::max_required_buffer_size(all_samples.len()));
    let encoded_size = mp3_encoder
        .encode(input, mp3_out_buffer.spare_capacity_mut())
        .expect("To encode");
    unsafe {
        mp3_out_buffer.set_len(mp3_out_buffer.len().wrapping_add(encoded_size));
    }
    let encoded_size = mp3_encoder
        .flush::<mp3lame_encoder::FlushNoGap>(mp3_out_buffer.spare_capacity_mut())
        .expect("to flush");
    unsafe {
        mp3_out_buffer.set_len(mp3_out_buffer.len().wrapping_add(encoded_size));
    }
    mp3_out_buffer
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    #[arg(long, default_value = "9001")]
    port: u16,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("secrets.toml"))
        .build()
        .unwrap();

    let args = Args::parse();

    let listener = TcpListener::bind((args.host.clone(), args.port)).expect("Cannot bind to port");
    let actual_port = listener.local_addr().unwrap().port();
    println!("Start server on http://{}:{}", args.host, actual_port);

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
    .listen(listener)?
    .run()
    .await
}
