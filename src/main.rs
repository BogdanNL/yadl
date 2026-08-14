//! yadl — скачивание файлов с Яндекс.Диска по публичной ссылке без браузера.
//!
//! Реализованы две стратегии получения прямой ссылки на файл:
//!   * `cloud` — публичный REST API cloud-api.yandex.net (по умолчанию);
//!   * `web`   — тот же путь, которым идёт кнопка «Скачать» на disk.yandex.ru
//!               (парсинг store-prefetch + POST /public/api/download-url).
//!
//! Ссылки, защищённые паролем, поддерживаются только web-путём: у публичного
//! REST API канала для пароля нет, он отвечает 403 DiskSymlinkPasswordRequiredError.
//! Поэтому `--backend cloud` для такой ссылки автоматически переключается на web.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36";
const CLOUD_API: &str = "https://cloud-api.yandex.net/v1/disk/public/resources";
const WEB_ORIGIN: &str = "https://disk.yandex.ru";

/// id ресурса в store-prefetch, когда страница отдана без доступа к содержимому.
const PASSWORD_STUB: &str = "password-protected";
/// data.code в ответе web-API: неверный пароль.
const ERR_INVALID_PASSWORD: i64 = 309;
/// data.code в ответе web-API: ресурс защищён паролем, passToken не предъявлен.
const ERR_PASSWORD_REQUIRED: i64 = 317;

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

    /// Пароль публичной ссылки (можно передать через YADL_PASSWORD)
    #[arg(short = 'p', long, env = "YADL_PASSWORD", hide_env_values = true)]
    password: Option<String>,

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

/// Результат работы любого из backend'ов: метаданные + готовая прямая ссылка.
struct Resolved {
    name: String,
    kind: String,
    size: Option<u64>,
    md5: Option<String>,
    sha256: Option<String>,
    href: String,
    via: &'static str,
}

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

// ---------------------------------------------------------------- main

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client = reqwest::Client::builder()
        .user_agent(UA)
        .cookie_store(true) // sk на web-пути привязан к cookies
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(20))
        .build()?;

    let res = match args.backend {
        Backend::Cloud => resolve_cloud(&client, &args).await?,
        Backend::Web => resolve_web(&client, &args).await?,
    };

    if args.info {
        println!("name:   {}", res.name);
        println!("type:   {}", res.kind);
        if let Some(s) = res.size {
            println!("size:   {s} bytes");
        }
        if let Some(h) = &res.md5 {
            println!("md5:    {h}");
        }
        if let Some(h) = &res.sha256 {
            println!("sha256: {h}");
        }
        println!("via:    {}", res.via);
        println!("href:   {}", res.href);
        return Ok(());
    }

    let out = args.output.unwrap_or_else(|| {
        let mut name = res.name.clone();
        if res.kind == "dir" && !name.ends_with(".zip") {
            name.push_str(".zip"); // папка отдаётся архивом
        }
        PathBuf::from(sanitize(&name))
    });

    download(&client, &res.href, &out, args.resume, res.size).await?;
    println!("saved: {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------- backend: cloud-api

/// cloud-api не умеет пароли: на защищённой ссылке отдаёт 403
/// DiskSymlinkPasswordRequiredError. В этом случае переключаемся на web-путь.
async fn resolve_cloud(client: &reqwest::Client, args: &Args) -> Result<Resolved> {
    let meta_url = cloud_url("", &args.url, args.path.as_deref());
    let resp = client.get(&meta_url).send().await.context("запрос метаданных")?;
    let status = resp.status();
    let body = resp.text().await?;

    if status == reqwest::StatusCode::FORBIDDEN && body.contains("PasswordRequired") {
        if args.password.is_none() {
            bail!("ссылка защищена паролем — укажите --password");
        }
        eprintln!("note: публичный REST API не принимает пароль, переключаюсь на web-путь");
        return resolve_web(client, args).await;
    }
    if !status.is_success() {
        bail!("cloud-api вернул {status}: {}", truncate(&body, 300));
    }
    let meta: PublicMeta = serde_json::from_str(&body).context("разбор метаданных")?;

    let dl_url = cloud_url("/download", &args.url, args.path.as_deref());
    let resp = client.get(&dl_url).send().await.context("запрос download-ссылки")?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("cloud-api /download вернул {status}: {}", truncate(&body, 300));
    }
    let href = serde_json::from_str::<Value>(&body)?["href"]
        .as_str()
        .ok_or_else(|| anyhow!("в ответе cloud-api нет href"))?
        .to_string();

    Ok(Resolved {
        name: meta.name,
        kind: meta.kind,
        size: meta.size,
        md5: meta.md5,
        sha256: meta.sha256,
        href,
        via: "cloud-api",
    })
}

fn cloud_url(suffix: &str, public_url: &str, path: Option<&str>) -> String {
    let mut u = format!("{CLOUD_API}{suffix}?public_key={}", enc(public_url));
    if let Some(p) = path {
        u.push_str(&format!("&path={}", enc(p)));
    }
    u
}

// ---------------------------------------------------------------- backend: web

/// Повторяет то, что делает страница:
///   GET страницы -> `sk` и `hash` из <script id="store-prefetch">
///   [если стоит пароль] POST /public/api/check-password -> passToken
///   POST /public/api/download-url -> прямая ссылка
async fn resolve_web(client: &reqwest::Client, args: &Args) -> Result<Resolved> {
    if args.path.is_some() {
        bail!("--path поддерживается только backend'ом cloud");
    }

    let html = client
        .get(&args.url)
        .header("Accept-Language", "ru-RU,ru;q=0.9")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let state = extract_store_prefetch(&html)?;
    let mut sk = state["environment"]["sk"]
        .as_str()
        .ok_or_else(|| anyhow!("в store-prefetch нет environment.sk"))?
        .to_string();
    let resource = state["resources"]
        .as_object()
        .and_then(|m| m.values().next())
        .ok_or_else(|| anyhow!("в store-prefetch нет resources"))?;
    let hash = resource["hash"]
        .as_str()
        .ok_or_else(|| anyhow!("в store-prefetch нет resources[].hash"))?
        .to_string();

    // Ссылка под паролем: вместо ресурса отдаётся заглушка password-protected.
    let locked = state["rootResourceId"].as_str() == Some(PASSWORD_STUB)
        || resource["id"].as_str() == Some(PASSWORD_STUB);

    let pass_token = if locked {
        let password = args
            .password
            .as_deref()
            .ok_or_else(|| anyhow!("ссылка защищена паролем — укажите --password"))?;
        let short_url = short_url_of(&args.url);
        Some(check_password(client, &args.url, &hash, password, &short_url, &mut sk).await?)
    } else {
        None
    };

    let href = download_url(client, &args.url, &hash, &sk, pass_token.as_deref()).await?;

    // Имя и размер: из store-prefetch, если он раскрыт, иначе из query прямой ссылки
    // (в ней есть filename= и fsize=).
    let name = resource["name"]
        .as_str()
        .map(str::to_string)
        .or_else(|| query_param(&href, "filename"))
        .unwrap_or_else(|| "download.bin".to_string());
    let kind = resource["type"].as_str().unwrap_or("file").to_string();
    let size = resource["meta"]["size"]
        .as_u64()
        .or_else(|| query_param(&href, "fsize").and_then(|s| s.parse().ok()));

    Ok(Resolved {
        name,
        kind,
        size,
        md5: None, // web-путь хеши не отдаёт
        sha256: None,
        href,
        via: if locked { "web (с паролем)" } else { "web" },
    })
}

/// POST /public/api/check-password -> {"token": "..."} — он же passToken.
async fn check_password(
    client: &reqwest::Client,
    public_url: &str,
    hash: &str,
    password: &str,
    short_url: &str,
    sk: &mut String,
) -> Result<String> {
    for attempt in 0..2 {
        let payload = serde_json::json!({
            "hash": hash,
            "password": password,
            "short_url": short_url,
            "sk": sk.as_str(),
        });
        let v = post_web_api(client, "check-password", public_url, &payload).await?;

        if let Some(token) = v["token"].as_str() {
            return Ok(token.to_string());
        }
        if let Some(new_sk) = retry_sk(&v, attempt) {
            *sk = new_sk;
            continue;
        }
        match v["data"]["code"].as_i64() {
            Some(ERR_INVALID_PASSWORD) => bail!("неверный пароль"),
            _ => bail!("check-password отказал: {}", truncate(&v.to_string(), 300)),
        }
    }
    unreachable!()
}

/// POST /public/api/download-url -> {"data": {"url": "..."}}.
async fn download_url(
    client: &reqwest::Client,
    public_url: &str,
    hash: &str,
    sk: &str,
    pass_token: Option<&str>,
) -> Result<String> {
    let mut sk = sk.to_string();
    for attempt in 0..2 {
        let mut payload = serde_json::json!({ "hash": hash, "inline": false, "sk": sk });
        if let Some(t) = pass_token {
            payload["passToken"] = Value::String(t.to_string());
        }
        let v = post_web_api(client, "download-url", public_url, &payload).await?;

        if let Some(url) = v["data"]["url"].as_str() {
            return Ok(url.to_string());
        }
        if let Some(new_sk) = retry_sk(&v, attempt) {
            sk = new_sk;
            continue;
        }
        match v["data"]["code"].as_i64() {
            Some(ERR_PASSWORD_REQUIRED) => bail!("ссылка защищена паролем — укажите --password"),
            _ => bail!("download-url отказал: {}", truncate(&v.to_string(), 300)),
        }
    }
    unreachable!()
}

/// Общий вызов внутреннего API страницы. Тело — JSON, но с Content-Type
/// text/plain: именно так его шлёт браузер.
async fn post_web_api(
    client: &reqwest::Client,
    method: &str,
    public_url: &str,
    payload: &Value,
) -> Result<Value> {
    let body = client
        .post(format!("{WEB_ORIGIN}/public/api/{method}"))
        .header("Content-Type", "text/plain")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Origin", WEB_ORIGIN)
        .header("Referer", public_url)
        .body(serde_json::to_string(payload)?)
        .send()
        .await
        .with_context(|| format!("запрос {method}"))?
        .text()
        .await?;
    serde_json::from_str(&body).with_context(|| format!("разбор ответа {method}"))
}

/// Страница при ответе {"wrongSk": true, "newSk": "..."} повторяет запрос
/// с новым sk — делаем ровно один такой ретрай.
fn retry_sk(v: &Value, attempt: usize) -> Option<String> {
    if attempt == 0 && v["wrongSk"].as_bool() == Some(true) {
        return v["newSk"].as_str().map(str::to_string);
    }
    None
}

fn extract_store_prefetch(html: &str) -> Result<Value> {
    const MARK: &str = r#"id="store-prefetch">"#;
    let start = html
        .find(MARK)
        .ok_or_else(|| anyhow!("на странице нет store-prefetch (ссылка невалидна?)"))?
        + MARK.len();
    let end = html[start..]
        .find("</script>")
        .ok_or_else(|| anyhow!("повреждённый store-prefetch"))?;
    serde_json::from_str(html[start..start + end].trim()).context("разбор store-prefetch")
}

/// "https://disk.yandex.ru/d/abc?x=1" -> "/d/abc"
fn short_url_of(public_url: &str) -> String {
    let no_scheme = public_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(public_url);
    let path = match no_scheme.find('/') {
        Some(i) => &no_scheme[i..],
        None => "/",
    };
    path.split(['?', '#']).next().unwrap_or(path).to_string()
}

/// Достаёт значение query-параметра из прямой ссылки (filename, fsize, ...).
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == key {
            return Some(
                percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        }
    }
    None
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
