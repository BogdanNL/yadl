//! yadl — скачивание файлов с Яндекс.Диска по публичной ссылке без браузера.
//!
//! Реализованы две стратегии получения прямой ссылки на файл:
//!   * `cloud` — публичный REST API cloud-api.yandex.net (по умолчанию);
//!   * `web`   — тот же путь, которым идёт кнопка «Скачать» на disk.yandex.ru
//!               (парсинг store-prefetch + POST /public/api/download-url).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
const CLOUD_API: &str = "https://cloud-api.yandex.net/v1/disk/public/resources";
const WEB_ORIGIN: &str = "https://disk.yandex.ru";

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// Публичный REST API Яндекс.Диска (стабильный контракт).
    Cloud,
    /// Внутренний web-эндпоинт страницы (как кнопка «Скачать»).
    Web,
}

#[derive(Parser)]
#[command(name = "yadl", about = "Скачивание файлов Яндекс.Диска по публичной ссылке")]
struct Args {
    /// Публичная ссылка (https://disk.yandex.ru/d/... или https://yadi.sk/d/...)
    url: String,

    /// Путь для сохранения (по умолчанию — имя файла из метаданных)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Путь внутри публичной папки, например "/subdir/file.bin"
    #[arg(long)]
    path: Option<String>,

    /// Способ получения прямой ссылки
    #[arg(long, value_enum, default_value_t = Backend::Cloud)]
    backend: Backend,

    /// Докачивать существующий файл (HTTP Range)
    #[arg(long)]
    resume: bool,

    /// Показать метаданные и прямую ссылку, ничего не скачивая
    #[arg(long)]
    info: bool,
}

// ---------------------------------------------------------------- модели

#[derive(Debug, Deserialize)]
struct PublicMeta {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    md5: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DownloadHref {
    href: String,
}

#[derive(Debug, Deserialize)]
struct WebApiResponse {
    #[serde(default)]
    error: bool,
    #[serde(default, rename = "wrongSk")]
    wrong_sk: bool,
    #[serde(default, rename = "newSk")]
    new_sk: Option<String>,
    #[serde(default)]
    data: Option<WebApiData>,
}

#[derive(Debug, Deserialize)]
struct WebApiData {
    url: String,
}

// ---------------------------------------------------------------- main

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client = reqwest::Client::builder()
        .user_agent(UA)
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let meta = fetch_meta(&client, &args.url, args.path.as_deref()).await?;
    let href = match args.backend {
        Backend::Cloud => cloud_href(&client, &args.url, args.path.as_deref()).await?,
        Backend::Web => web_href(&client, &args.url).await?,
    };

    if args.info {
        println!("name:   {}", meta.name);
        println!("type:   {}", meta.kind);
        if let Some(s) = meta.size {
            println!("size:   {s} bytes");
        }
        if let Some(h) = &meta.md5 {
            println!("md5:    {h}");
        }
        if let Some(h) = &meta.sha256 {
            println!("sha256: {h}");
        }
        println!("href:   {href}");
        return Ok(());
    }

    let out = args.output.unwrap_or_else(|| {
        let mut name = meta.name.clone();
        if meta.kind == "dir" && !name.ends_with(".zip") {
            name.push_str(".zip"); // папка отдаётся архивом
        }
        PathBuf::from(sanitize(&name))
    });

    download(&client, &href, &out, args.resume, meta.size).await?;
    println!("saved: {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------- метаданные

async fn fetch_meta(client: &reqwest::Client, public_url: &str, path: Option<&str>) -> Result<PublicMeta> {
    let mut url = format!("{CLOUD_API}?public_key={}", enc(public_url));
    if let Some(p) = path {
        url.push_str(&format!("&path={}", enc(p)));
    }
    let resp = client.get(&url).send().await.context("запрос метаданных")?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("cloud-api вернул {status}: {}", truncate(&body, 300));
    }
    serde_json::from_str(&body).context("разбор метаданных")
}

// ---------------------------------------------------------------- backend: cloud-api

async fn cloud_href(client: &reqwest::Client, public_url: &str, path: Option<&str>) -> Result<String> {
    let mut url = format!("{CLOUD_API}/download?public_key={}", enc(public_url));
    if let Some(p) = path {
        url.push_str(&format!("&path={}", enc(p)));
    }
    let resp = client.get(&url).send().await.context("запрос download-ссылки")?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("cloud-api /download вернул {status}: {}", truncate(&body, 300));
    }
    Ok(serde_json::from_str::<DownloadHref>(&body)?.href)
}

// ---------------------------------------------------------------- backend: web

/// Повторяет ровно то, что делает кнопка «Скачать»:
/// GET страницы -> достать `environment.sk` и `resources[*].hash` из
/// `<script id="store-prefetch">` -> POST /public/api/download-url.
async fn web_href(client: &reqwest::Client, public_url: &str) -> Result<String> {
    let html = client
        .get(public_url)
        .header("Accept-Language", "ru-RU,ru;q=0.9")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let state = extract_store_prefetch(&html)?;
    let sk = state["environment"]["sk"]
        .as_str()
        .ok_or_else(|| anyhow!("в store-prefetch нет environment.sk"))?
        .to_string();
    let hash = state["resources"]
        .as_object()
        .and_then(|m| m.values().find_map(|r| r["hash"].as_str()))
        .ok_or_else(|| anyhow!("в store-prefetch нет resources[].hash"))?
        .to_string();

    // первая попытка + одна повторная с newSk (как в бандле страницы)
    let mut sk = sk;
    for attempt in 0..2 {
        let payload = serde_json::json!({ "hash": hash, "inline": false, "sk": sk });
        let resp: WebApiResponse = client
            .post(format!("{WEB_ORIGIN}/public/api/download-url"))
            .header("Content-Type", "text/plain")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", WEB_ORIGIN)
            .header("Referer", public_url)
            .body(serde_json::to_string(&payload)?)
            .send()
            .await?
            .json()
            .await?;

        if let Some(d) = resp.data {
            return Ok(d.url);
        }
        if resp.error && resp.wrong_sk && attempt == 0 {
            sk = resp.new_sk.ok_or_else(|| anyhow!("wrongSk без newSk"))?;
            continue;
        }
        bail!("download-url отказал (error={}, wrongSk={})", resp.error, resp.wrong_sk);
    }
    unreachable!()
}

fn extract_store_prefetch(html: &str) -> Result<serde_json::Value> {
    const MARK: &str = r#"id="store-prefetch">"#;
    let start = html
        .find(MARK)
        .ok_or_else(|| anyhow!("на странице нет store-prefetch (ссылка невалидна или требует пароль?)"))?
        + MARK.len();
    let end = html[start..]
        .find("</script>")
        .ok_or_else(|| anyhow!("повреждённый store-prefetch"))?;
    serde_json::from_str(html[start..start + end].trim()).context("разбор store-prefetch")
}

// ---------------------------------------------------------------- загрузка

async fn download(
    client: &reqwest::Client,
    href: &str,
    out: &Path,
    resume: bool,
    expected: Option<u64>,
) -> Result<()> {
    let mut offset = 0u64;
    if resume {
        if let Ok(md) = tokio::fs::metadata(out).await {
            offset = md.len();
        }
    }

    let mut req = client.get(href);
    if offset > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let resp = req.send().await.context("запрос тела файла")?.error_for_status()?;

    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if offset > 0 && !partial {
        offset = 0; // сервер проигнорировал Range — качаем заново
    }
    let total = resp.content_length().map(|n| n + offset).or(expected);

    let bar = match total {
        Some(t) => ProgressBar::new(t),
        None => ProgressBar::new_spinner(),
    };
    bar.set_style(
        ProgressStyle::with_template(
            "{bar:40.cyan/blue} {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
        )
        .unwrap(),
    );
    bar.set_position(offset);

    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            tokio::fs::create_dir_all(dir).await.ok();
        }
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(out)
        .await
        .with_context(|| format!("открытие {}", out.display()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await?;
    }

    let mut written = offset;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("обрыв потока")?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        bar.set_position(written);
    }
    file.flush().await?;
    bar.finish_and_clear();

    if let Some(t) = expected {
        if written != t {
            bail!("размер не совпал: получено {written}, ожидалось {t}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- утилиты

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn sanitize(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' { '_' } else { c })
        .collect();
    match n.trim().trim_matches('.') {
        "" => "download.bin".to_string(),
        _ => n,
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
