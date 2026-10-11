//! Public, credential-free official news and limited service observation.
use crate::i18n::{t, tf, Lang};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use quick_xml::{events::Event, Reader};
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// The official sources for one UI language. English reads the US site and
/// Japanese the Japanese site (`/ff11/`); both publish the same RSS 1.0 and
/// the same maintenance-list markup, so one parser serves both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sources {
    pub topics: &'static str,
    pub news: &'static str,
    pub maintenance: &'static str,
    /// The topics-latest component loaded by the official homepage: it maps
    /// each article link to its banner image.
    pub topic_index: &'static str,
    /// The official FFXI home page ("View all news").
    pub home: &'static str,
}

const SOURCES_EN: Sources = Sources {
    topics: "https://www.playonline.com/pcd2/topics/ff11us/topics.xml",
    news: "https://www.playonline.com/ff11us/polnews/news.xml",
    maintenance: "https://www.playonline.com/ff11us/info/list_mnt.shtml",
    topic_index: "https://www.playonline.com/pcd2/topics/ff11us/topics_latest.html",
    home: "https://www.playonline.com/ff11us/",
};
const SOURCES_JA: Sources = Sources {
    topics: "https://www.playonline.com/pcd2/topics/ff11/topics.xml",
    news: "https://www.playonline.com/ff11/polnews/news.xml",
    maintenance: "https://www.playonline.com/ff11/info/list_mnt.shtml",
    topic_index: "https://www.playonline.com/pcd2/topics/ff11/topics_latest.html",
    home: "https://www.playonline.com/ff11/",
};

pub fn sources(lang: Lang) -> Sources {
    match lang {
        Lang::En => SOURCES_EN,
        Lang::Ja => SOURCES_JA,
    }
}

/// The world-status page is machine-read only (sixteen world names and a
/// status picture each); every sentence the launcher shows about it is worded
/// by `t()`. The Japanese page lists the same servers under the same names, so
/// both languages read this one US page rather than a second parser.
pub const WORLD_STATUS_URL: &str = "https://www.playonline.com/pcd/service/ff11usindex.html";
const MAX_BODY: usize = 1_048_576;
const MAX_IMAGE_BODY: usize = 4 * 1_048_576;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NewsItem {
    pub title: String,
    pub url: String,
    pub date: String,
    pub summary: String,
    pub image_url: String,
    /// Decoded once on a worker; painting the launcher never decodes network bytes.
    #[serde(skip)]
    pub image: Option<iced::widget::image::Handle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Official status page reports all sixteen production worlds up.
    Operational,
    /// Official status page reports at least one production world disrupted.
    PartialOutage,
    /// Official status page reports all sixteen production worlds down.
    Outage,
    /// An official all-world maintenance notice covers the current instant.
    Maintenance,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ServiceSnapshot {
    pub news: Vec<NewsItem>,
    pub notices: Vec<NewsItem>,
    pub news_note: String,
    pub availability: Availability,
    pub availability_note: String,
    pub checked_at: String,
}

impl ServiceSnapshot {
    /// Headlines may be cached; downtime must be based on a recent status read.
    pub fn recent_availability(&self, now: DateTime<Utc>) -> Availability {
        let Ok(checked) = chrono::NaiveDateTime::parse_from_str(&self.checked_at, "%Y-%m-%d %H:%M:%S UTC") else {
            return Availability::Unknown;
        };
        let age = now.signed_duration_since(checked.and_utc());
        if age < chrono::Duration::zero() || age > chrono::Duration::seconds(360) {
            return Availability::Unknown;
        }
        let maintenance_active = self.notices.iter().any(|notice| {
            is_all_worlds_notice(&notice.title) && maintenance_window_active(&notice.summary, now)
        });
        match (self.availability, maintenance_active) {
            // Keep the underlying outage: the scheduled end does not establish recovery.
            (Availability::Outage, true) => Availability::Maintenance,
            (Availability::Maintenance, false) => Availability::Unknown,
            _ => self.availability,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Update {
    Headlines { items: Vec<NewsItem>, note: String },
    Image(NewsItem),
    Status(ServiceSnapshot),
}
type Publisher = dyn Fn(Update) + Send + Sync;

fn empty_snapshot() -> ServiceSnapshot {
    ServiceSnapshot {
        news: Vec::new(),
        notices: Vec::new(),
        news_note: String::new(),
        availability: Availability::Unknown,
        availability_note: t("Checking official world status…").into(),
        checked_at: String::new(),
    }
}

/// Each source updates only its part of the screen; slow status/images cannot erase headlines.
pub fn apply(current: &mut Option<ServiceSnapshot>, update: Update) {
    let current = current.get_or_insert_with(empty_snapshot);
    match update {
        Update::Headlines { mut items, note } => {
            if items.is_empty() && !current.news.is_empty() {
                current.news_note = tf("Showing previous headlines. {note}", &[("note", &note)]);
                return;
            }
            for item in &mut items {
                if item.image.is_none() {
                    if let Some(old) = current.news.iter().find(|old| old.url == item.url) {
                        item.image = old.image.clone();
                    }
                }
            }
            current.news = items;
            current.news_note = note;
        }
        Update::Image(mut item) => {
            if let Some(current) = current.news.iter_mut().find(|n| n.url == item.url) {
                if item.image.is_none() {
                    item.image = current.image.clone();
                }
                *current = item;
            }
        }
        Update::Status(status) => {
            current.notices = status.notices;
            current.availability = status.availability;
            current.availability_note = status.availability_note;
            current.checked_at = status.checked_at;
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct NewsCache {
    items: Vec<NewsItem>,
    saved_at: String,
    /// Language of the headlines. Files from before the Japanese feeds have no
    /// tag and hold English.
    #[serde(default)]
    language: Option<Lang>,
}

/// Where `lang`'s headlines are cached beside `base` (the English file, whose
/// name never changed): `news-cache.json` for English, `news-cache.ja.json` for
/// Japanese. A language never reads another's file.
pub fn cache_path(base: &Path, lang: Lang) -> PathBuf {
    match lang {
        Lang::En => base.to_path_buf(),
        Lang::Ja => base.with_extension("ja.json"),
    }
}

/// The saved headlines for `lang`, or none when the file is missing, damaged
/// or holds the other language.
pub fn cached(path: &Path, lang: Lang) -> Option<ServiceSnapshot> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BODY {
        return None;
    }
    let mut cache: NewsCache = serde_json::from_slice(&bytes).ok()?;
    if cache.language.unwrap_or(Lang::En) != lang {
        return None;
    }
    cache.items.retain(|n| {
        official_url(&n.url).is_some() && n.title.len() <= 4096 && n.summary.len() <= MAX_BODY
    });
    cache.items.truncate(24);
    if cache.items.is_empty() {
        return None;
    }
    let mut snapshot = empty_snapshot();
    snapshot.news = cache.items;
    snapshot.news_note = tf(
        "Saved headlines from {saved_at}. Refreshing official feeds…",
        &[("saved_at", &cache.saved_at)],
    );
    Some(snapshot)
}

fn save_cache(path: &Path, lang: Lang, items: &[NewsItem]) {
    if items.is_empty() {
        return;
    }
    let cache = NewsCache {
        items: items.to_vec(),
        saved_at: Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        language: Some(lang),
    };
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        if bytes.len() > MAX_BODY {
            return;
        }
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(temporary, path);
        }
    }
}

type FeedTasks = tokio::task::JoinSet<(&'static str, Result<Vec<NewsItem>, String>)>;
async fn collect_feeds(mut feeds: FeedTasks, publish: &Publisher) -> (Vec<NewsItem>, String) {
    let mut news = Vec::new();
    let mut notes = Vec::new();
    while let Some(result) = feeds.join_next().await {
        match result {
            Ok((label, Ok(items))) => {
                let newest = items.iter().filter_map(|i| rss_date(&i.date)).max();
                match newest {
                    Some(date) if Utc::now().signed_duration_since(date).num_days() >= 7 => notes
                        .push(tf(
                            "{label}: newest item is {days} days old.",
                            &[("label", &label), ("days", &Utc::now().signed_duration_since(date).num_days())],
                        )),
                    None => notes.push(tf("{label}: freshness could not be determined.", &[("label", &label)])),
                    _ => {}
                }
                news.extend(items);
            }
            Ok((label, Err(error))) => notes.push(tf("{label} unavailable: {error}", &[("label", &label), ("error", &error)])),
            Err(_) => notes.push(t("An official feed reader could not finish.").into()),
        }
        news.sort_by(|a, b| rss_date(&b.date).cmp(&rss_date(&a.date)));
        let mut seen = HashSet::new();
        news.retain(|n| seen.insert(n.url.clone()));
        news.truncate(24);
        publish(Update::Headlines {
            items: news.clone(),
            note: notes.join(" "),
        });
    }
    if notes.is_empty() {
        notes.push(t("Official Topics and Information RSS. Feeds may lag announcements; maintenance index checked separately.").into());
    }
    (news, notes.join(" "))
}

/// Run through iced's async task executor. All requests have finite deadlines.
/// Reads the feeds of the language showing now.
pub async fn refresh() -> Result<ServiceSnapshot, String> {
    refresh_with_progress(crate::i18n::current(), None, |_| {}).await
}

fn web_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("FFXI-Iced-Launcher/0.1 (public news reader)")
        .build()
        .map_err(|e| e.to_string())
}

/// A periodic status read does not reload RSS images or contact login/patch servers.
pub async fn refresh_status(lang: Lang) -> Result<ServiceSnapshot, String> {
    Ok(read_status(&web_client()?, lang).await)
}

fn availability_with_notice(
    worlds: &Result<(Availability, String), String>, active_notice: Option<&str>,
) -> (Availability, String) {
    match (worlds, active_notice) {
        (Ok(status), _) => status.clone(),
        (Err(_), Some(title)) => (Availability::Maintenance, tf(
            "Official scheduled all-world maintenance window is active: {title}. Completion may change.",
            &[("title", &title)],
        )),
        (Err(_), None) => (Availability::Unknown, t("Official world status is unavailable. This does not establish a game outage. Check official notices.").into()),
    }
}

async fn read_status(client: &reqwest::Client, lang: Lang) -> ServiceSnapshot {
    let source = sources(lang);
    let (maintenance, world_status) = tokio::join!(
        fetch_page(client, source.maintenance, lang), fetch(client, WORLD_STATUS_URL),
    );
    let mut notes = Vec::new();
    let mut notices = match maintenance.and_then(|body| parse_notices(&body)) {
        Ok(items) => items,
        Err(e) => {
            notes.push(tf("Maintenance notices unavailable: {error}", &[("error", &e)]));
            Vec::new()
        }
    };
    let mut active_notice = None;
    for item in notices.iter_mut().filter(|n| is_all_worlds_notice(&n.title)).take(3) {
        match fetch_page(client, &item.url, lang).await {
            Ok(body) => {
                item.summary = notice_body(&body);
                if maintenance_window_active(&item.summary, Utc::now()) {
                    active_notice = Some(item.title.clone());
                }
                item.summary = shorten(&item.summary, 1800);
            }
            Err(e) => notes.push(tf("Maintenance detail unavailable: {error}", &[("error", &e)])),
        }
    }
    let worlds = world_status.and_then(|body| parse_world_status(&body));
    let (availability, mut availability_note) = availability_with_notice(&worlds, active_notice.as_deref());
    if let Err(error) = worlds {
        availability_note = tf("{note} Official world status unavailable: {error}.",
            &[("note", &availability_note), ("error", &error)]);
    } else if let Some(title) = active_notice {
        availability_note = tf("{note} Scheduled maintenance notice also active: {title}.",
            &[("note", &availability_note), ("title", &title)]);
    }
    ServiceSnapshot {
        news: Vec::new(), notices, news_note: notes.join(" "), availability, availability_note,
        checked_at: Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    }
}

/// Fetches `lang`'s official feeds. `lang` is fixed for the whole refresh, so a
/// language change while it runs cannot mix the two languages' headlines.
pub async fn refresh_with_progress(
    lang: Lang,
    cache: Option<PathBuf>,
    progress: impl Fn(Update) + Send + Sync + 'static,
) -> Result<ServiceSnapshot, String> {
    let source = sources(lang);
    let publish: Arc<Publisher> = Arc::new(move |update| {
        progress(update.clone());
        if let (Some(path), Update::Headlines { items, .. }) = (&cache, &update) {
            save_cache(path, lang, items);
        }
    });
    let client = web_client()?;
    let news_task = async {
        let mut feeds = FeedTasks::new();
        for (label, url) in [(t("Topics RSS"), source.topics), (t("Information RSS"), source.news)] {
            let client = client.clone();
            feeds.spawn(async move {
                (
                    label,
                    fetch(&client, url).await.and_then(|body| parse_rss(&body)),
                )
            });
        }
        let ((mut news, note), image_index) = tokio::join!(
            collect_feeds(feeds, publish.as_ref()),
            fetch_page(&client, source.topic_index, lang)
        );
        let index_images = image_index
            .map(|body| topic_index_images(&body, source.topic_index))
            .unwrap_or_default();
        enrich_news_images(&client, &mut news, &index_images, lang, publish.as_ref()).await;
        (news, note)
    };
    let status_task = async {
        let snapshot = read_status(&client, lang).await;
        publish(Update::Status(snapshot.clone()));
        snapshot
    };
    let ((news, note), mut snapshot) = tokio::join!(news_task, status_task);
    snapshot.news = news;
    snapshot.news_note = if snapshot.news_note.is_empty() {
        note
    } else {
        format!("{note} {}", snapshot.news_note)
    };
    Ok(snapshot)
}

pub fn official_url(raw: &str) -> Option<String> {
    let base = reqwest::Url::parse("https://www.playonline.com/").ok()?;
    let mut url = base.join(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    match url.host_str()? {
        "www.playonline.com" | "playonline.com" | "sqex.to" => {}
        _ => return None,
    }
    url.set_scheme("https").ok()?;
    Some(url.to_string())
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let (_, body) = fetch_bytes(client, url, MAX_BODY).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// An HTML page in `lang`. English is read exactly as `fetch` does; Japanese
/// pages are not all UTF-8 (the maintenance list is Shift_JIS).
async fn fetch_page(client: &reqwest::Client, url: &str, lang: Lang) -> Result<String, String> {
    let (_, body) = fetch_bytes(client, url, MAX_BODY).await?;
    Ok(decode_page(&body, lang))
}

fn decode_page(body: &[u8], lang: Lang) -> String {
    match lang {
        Lang::En => String::from_utf8_lossy(body).into_owned(),
        Lang::Ja => decode_japanese_page(body),
    }
}

/// Text of a Japanese official page. The RSS feeds and `topics_latest.html`
/// are UTF-8; the older `/ff11/info/` and `/ff11/polnews/` pages declare
/// `charset=Shift_JIS` in a meta tag, and the response header names none.
/// Valid UTF-8 is taken as it is (Shift_JIS text with kana is never valid
/// UTF-8). Otherwise the page's own declaration decides, and an undeclared page
/// is tried as Shift_JIS, then EUC-JP, strictly, so a wrong guess is refused
/// rather than turned into mojibake.
fn decode_japanese_page(body: &[u8]) -> String {
    const SHIFT_JIS: u32 = 932; // Windows' Shift_JIS, a superset of the standard
    const EUC_JP: u32 = 20932;
    if let Ok(text) = std::str::from_utf8(body) {
        return text.to_owned();
    }
    let decoded = match declared_charset(body).as_deref() {
        Some("shift_jis" | "shift-jis" | "x-sjis" | "sjis" | "windows-31j" | "ms932" | "cp932") => {
            legacy_to_string(SHIFT_JIS, body, false)
        }
        Some("euc-jp" | "x-euc-jp" | "eucjp") => legacy_to_string(EUC_JP, body, false),
        Some(_) => None,
        None => legacy_to_string(SHIFT_JIS, body, true)
            .or_else(|| legacy_to_string(EUC_JP, body, true)),
    };
    decoded.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// The `charset=` a page declares in its first 2 KiB, lower-cased.
fn declared_charset(body: &[u8]) -> Option<String> {
    let head: String = body[..body.len().min(2048)]
        .iter()
        .map(|b| if b.is_ascii() { char::from(b.to_ascii_lowercase()) } else { ' ' })
        .collect();
    let after = &head[head.find("charset=")? + "charset=".len()..];
    let name: String = after
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `bytes` in a Windows code page as text. `strict` refuses invalid input
/// instead of substituting characters.
#[cfg(windows)]
fn legacy_to_string(code_page: u32, bytes: &[u8], strict: bool) -> Option<String> {
    use windows_sys::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};
    let length = i32::try_from(bytes.len()).ok()?;
    let flags = if strict { MB_ERR_INVALID_CHARS } else { 0 };
    // SAFETY: both calls read `length` bytes from `bytes`; the second writes at
    // most the `needed` u16 the first reported into a buffer of that size.
    unsafe {
        let needed = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), length, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(code_page, flags, bytes.as_ptr(), length, wide.as_mut_ptr(), needed);
        if written <= 0 {
            return None;
        }
        String::from_utf16(&wide[..written as usize]).ok()
    }
}
#[cfg(not(windows))]
fn legacy_to_string(_code_page: u32, _bytes: &[u8], _strict: bool) -> Option<String> {
    None
}

async fn fetch_bytes(
    client: &reqwest::Client,
    raw: &str,
    max_body: usize,
) -> Result<(String, Vec<u8>), String> {
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut url = official_url(raw).ok_or(t("Untrusted official URL"))?;
        for hop in 0..=4 {
            let mut response = client.get(&url).send().await.map_err(|e| e.to_string())?;
            if response.status().is_redirection() {
                if hop == 4 {
                    return Err(t("Official redirect limit reached").into());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|h| h.to_str().ok())
                    .ok_or(t("Redirect has no usable location"))?;
                url =
                    resolve_official_url(&url, location).ok_or(t("Redirect leaves official hosts"))?;
                continue;
            }
            response = response.error_for_status().map_err(|e| e.to_string())?;
            if response
                .content_length()
                .is_some_and(|n| n > max_body as u64)
            {
                return Err(t("Official response exceeds size limit").into());
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
                if body.len() + chunk.len() > max_body {
                    return Err(t("Official response exceeds size limit").into());
                }
                body.extend_from_slice(&chunk);
            }
            return Ok((url, body));
        }
        Err(t("Official redirect limit reached").into())
    })
    .await
    .map_err(|_| t("Official request timed out").to_owned())?
}

fn resolve_official_url(base: &str, raw: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    official_url(base.join(raw.trim()).ok()?.as_str())
}

fn article_image_url(base: &str, raw: &str) -> Option<String> {
    let url = resolve_official_url(base, raw)?;
    let parsed = reqwest::Url::parse(&url).ok()?;
    if !matches!(parsed.host_str()?, "www.playonline.com" | "playonline.com") {
        return None;
    }
    let path = parsed.path().to_ascii_lowercase();
    let name = path.rsplit('/').next()?;
    if ![".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| name.ends_with(ext))
        || [
            "logo", "icon", "spacer", "tracking", "pixel", "ogp-", "x-card", "button", "nav_",
            "status_",
        ]
        .iter()
        .any(|part| name.contains(part))
        || path.contains("/static/")
        || path.contains("/common/")
        // Heading, rule and icon graphics of the Information pages, which an
        // announcement page's content zone otherwise offers as its picture.
        || path.contains("/imgs/info/")
    {
        return None;
    }
    Some(url)
}

fn image_candidate(node: scraper::ElementRef<'_>, base: &str) -> Option<String> {
    for dimension in ["width", "height"] {
        if node
            .value()
            .attr(dimension)
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n < 32)
        {
            return None;
        }
    }
    article_image_url(base, node.value().attr("src")?)
}

fn topic_index_images(html: &str, base: &str) -> HashMap<String, String> {
    let document = Html::parse_document(html);
    let links = Selector::parse(".summary-banner a[href], a:has(img.tpimgL)").unwrap();
    let images = Selector::parse("img").unwrap();
    document
        .select(&links)
        .filter_map(|link| {
            let article = resolve_official_url(base, link.value().attr("href")?)?;
            let image = link
                .select(&images)
                .find_map(|node| image_candidate(node, base))?;
            Some((article, image))
        })
        .collect()
}

fn html_article_image(html: &str, base: &str) -> Option<String> {
    let document = Html::parse_document(html);
    // Prefer actual article content over site-wide social metadata.
    let images = Selector::parse(".topics-detail img, #bc_zone img, .mdl-img__normal img").unwrap();
    if let Some(url) = document
        .select(&images)
        .find_map(|node| image_candidate(node, base))
    {
        return Some(url);
    }
    let meta = Selector::parse("meta[property='og:image'], meta[name='twitter:image']").unwrap();
    document
        .select(&meta)
        .find_map(|node| article_image_url(base, node.value().attr("content")?))
}

async fn enrich_news_images(
    client: &reqwest::Client,
    news: &mut [NewsItem],
    index: &HashMap<String, String>,
    lang: Lang,
    publish: &Publisher,
) {
    let mut tasks = tokio::task::JoinSet::<(usize, NewsItem)>::new();
    for position in 0..news.len().min(12) {
        while tasks.len() >= 3 {
            if let Some(Ok((position, item))) = tasks.join_next().await {
                publish(Update::Image(item.clone()));
                news[position] = item;
            }
        }
        let mut item = news[position].clone();
        if let Some(url) = index.get(&item.url) {
            item.image_url = url.clone();
        }
        let client = client.clone();
        tasks.spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                if item.image_url.is_empty() {
                    if let Ok((base, body)) = fetch_bytes(&client, &item.url, MAX_BODY).await {
                        item.image_url = html_article_image(&decode_page(&body, lang), &base)
                            .unwrap_or_default();
                    }
                }
                if !item.image_url.is_empty() {
                    if let Ok((_, bytes)) =
                        fetch_bytes(&client, &item.image_url, MAX_IMAGE_BODY).await
                    {
                        item.image =
                            tokio::task::spawn_blocking(move || decode_article_image(bytes))
                                .await
                                .ok()
                                .flatten();
                    }
                }
            })
            .await;
            (position, item)
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let Ok((position, item)) = result {
            publish(Update::Image(item.clone()));
            news[position] = item;
        }
    }
}

fn decode_article_image(bytes: Vec<u8>) -> Option<iced::widget::image::Handle> {
    let mut reader = imagecrate::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = imagecrate::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1_048_576);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    if image.width() < 64 || image.height() < 32 {
        return None;
    }
    let rgba = image.thumbnail(1200, 700).to_rgba8();
    Some(iced::widget::image::Handle::from_rgba(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
    ))
}

fn plain_html(html: &str) -> String {
    let with_spaces = html
        .replace("<br>", " ")
        .replace("<br/>", " ")
        .replace("<br />", " ");
    let mut text = String::new();
    for piece in Html::parse_fragment(&with_spaces).root_element().text() {
        // Neighbouring text runs are separate words, except in Japanese, which
        // has no word spaces: "<b>無期限</b>に変更" must stay one phrase.
        let glue = text.chars().next_back().zip(piece.chars().next());
        if !glue.is_some_and(|(before, after)| is_cjk(before) || is_cjk(after)) {
            text.push(' ');
        }
        text.push_str(piece);
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// Kana, kanji, CJK punctuation and full-width forms.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{ff00}'..='\u{ffef}')
}
fn shorten(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn rss_date(date: &str) -> Option<DateTime<Utc>> {
    // Square Enix uses JST, which RFC2822 parsers do not consistently recognize.
    DateTime::parse_from_rfc2822(&date.replace(" JST", " +0900"))
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn parse_rss(xml: &str) -> Result<Vec<NewsItem>, String> {
    let mut reader = Reader::from_str(xml);
    let mut items = Vec::new();
    let mut item: Option<NewsItem> = None;
    let mut field = String::new();
    loop {
        match reader
            .read_event()
            .map_err(|e| tf("Invalid RSS: {error}", &[("error", &e)]))?
        {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "item" {
                    item = Some(NewsItem::default());
                } else if item.is_some() {
                    capture_rss_image(&mut item, &e)?;
                    field = name;
                }
            }
            Event::Empty(e) => capture_rss_image(&mut item, &e)?,
            Event::Text(e) => {
                let value = e.decode().map_err(|e| e.to_string())?;
                append_field(&mut item, &field, &value);
            }
            Event::CData(e) => {
                append_field(&mut item, &field, &e.decode().map_err(|e| e.to_string())?)
            }
            Event::GeneralRef(e) => {
                let encoded = format!("&{};", e.decode().map_err(|e| e.to_string())?);
                let decoded = quick_xml::escape::unescape(&encoded).map_err(|e| e.to_string())?;
                append_field(&mut item, &field, &decoded);
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"item" {
                    if let Some(mut n) = item.take() {
                        if let Some(url) = official_url(&n.url) {
                            n.url = url;
                            n.title = shorten(n.title.trim(), 240);
                            n.image_url =
                                article_image_url(&n.url, &n.image_url).unwrap_or_default();
                            if n.image_url.is_empty() {
                                let fragment = Html::parse_fragment(&n.summary);
                                let images = Selector::parse("img").unwrap();
                                n.image_url = fragment
                                    .select(&images)
                                    .find_map(|node| image_candidate(node, &n.url))
                                    .unwrap_or_default();
                            }
                            n.summary = shorten(&plain_html(&n.summary), 1800);
                            if !n.title.is_empty() {
                                items.push(n);
                            }
                        }
                    }
                    if items.len() >= 50 {
                        break;
                    }
                }
                field.clear();
            }
            Event::DocType(_) => return Err(t("RSS document types are unsupported").into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if item.is_some() {
        return Err(t("Incomplete RSS item").into());
    }
    if items.is_empty() {
        return Err(t("RSS contains no usable official items").into());
    }
    Ok(items)
}
fn capture_rss_image(
    item: &mut Option<NewsItem>,
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<(), String> {
    if !matches!(
        element.local_name().as_ref(),
        b"enclosure" | b"content" | b"thumbnail"
    ) {
        return Ok(());
    }
    if let Some(item) = item.as_mut().filter(|n| n.image_url.is_empty()) {
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|e| e.to_string())?;
            if attribute.key.as_ref() == b"url" {
                item.image_url = attribute
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| e.to_string())?
                    .into_owned();
            }
        }
    }
    Ok(())
}
fn append_field(item: &mut Option<NewsItem>, field: &str, value: &str) {
    if let Some(n) = item {
        match field {
            "title" => n.title.push_str(value),
            "link" => n.url.push_str(value),
            "date" | "pubDate" => n.date.push_str(value),
            "description" => n.summary.push_str(value),
            _ => {}
        }
    }
}

fn parse_world_status(html: &str) -> Result<(Availability, String), String> {
    const WORLDS: [&str; 16] = [
        "Bahamut",
        "Shiva",
        "Phoenix",
        "Carbuncle",
        "Fenrir",
        "Sylph",
        "Valefor",
        "Leviathan",
        "Odin",
        "Quetzalcoatl",
        "Siren",
        "Ragnarok",
        "Cerberus",
        "Bismarck",
        "Lakshmi",
        "Asura",
    ];
    let document = Html::parse_document(html);
    let selector = Selector::parse("#bc_zone2 h2, #bc_zone2 img.img_status").unwrap();
    let mut pending = None;
    let mut states = std::collections::BTreeMap::new();
    for node in document.select(&selector) {
        if node.value().name() == "h2" {
            let name = node.text().collect::<String>().trim().to_owned();
            pending = WORLDS.contains(&name.as_str()).then_some(name);
        } else if let Some(name) = pending.take() {
            let state = match node.value().attr("src") {
                Some("/ff11us/server/imgs/status_00.gif") => 0,
                Some("/ff11us/server/imgs/status_01.gif") => 1,
                Some("/ff11us/server/imgs/status_02.gif") => 2,
                _ => return Err(t("Unrecognized official world status indicator").into()),
            };
            if states.insert(name, state).is_some() {
                return Err(t("Duplicate official world status entry").into());
            }
        }
    }
    if states.len() != WORLDS.len() {
        return Err(t("Official status page did not contain all sixteen production worlds").into());
    }
    let affected: Vec<_> = states
        .iter()
        .filter(|(_, state)| **state != 0)
        .map(|(name, state)| {
            if *state == 1 {
                tf("{name} (partially down)", &[("name", name)])
            } else {
                tf("{name} (down)", &[("name", name)])
            }
        })
        .collect();
    let (state, text) = if affected.is_empty() {
        (
            Availability::Operational,
            t("Official status page reports all 16 production worlds up.").to_owned(),
        )
    } else if states.values().all(|state| *state == 2) {
        (
            Availability::Outage,
            t("Official status page reports all 16 production worlds down.").to_owned(),
        )
    } else {
        (
            Availability::PartialOutage,
            tf(
                "Official status page reports world disruption: {worlds}.",
                &[("worlds", &affected.join(t(", ")))],
            ),
        )
    };
    Ok((
        state,
        tf("{text} Status is published every five minutes; Atomos test server excluded.", &[("text", &text)]),
    ))
}

fn parse_notices(html: &str) -> Result<Vec<NewsItem>, String> {
    let document = Html::parse_document(html);
    let tables = Selector::parse("table.ta_info2").unwrap();
    let links = Selector::parse("a.info").unwrap();
    let dates = Selector::parse("span.tx_info2").unwrap();
    let mut items = Vec::new();
    for table in document.select(&tables) {
        if let Some(link) = table.select(&links).next() {
            if let Some(url) = link.value().attr("href").and_then(official_url) {
                items.push(NewsItem {
                    title: shorten(&link.text().collect::<String>(), 240),
                    url,
                    date: table
                        .select(&dates)
                        .next()
                        .map(|d| d.text().collect::<String>())
                        .unwrap_or_default(),
                    summary: String::new(),
                    ..NewsItem::default()
                });
            }
        }
        if items.len() >= 12 {
            break;
        }
    }
    if items.is_empty() {
        Err(t("Official maintenance page format was not recognized").into())
    } else {
        Ok(items)
    }
}

/// The text of an announcement page's content zone.
fn notice_body(html: &str) -> String {
    let document = Html::parse_document(html);
    let content = Selector::parse("#bc_zone").unwrap();
    document
        .select(&content)
        .next()
        .map(|n| n.text().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// An all-worlds maintenance announcement, by its official title in either
/// language ("All Worlds Maintenance (Sep. 28)", "全ワールド メンテナンス作業のお知らせ(9/29)").
fn is_all_worlds_notice(title: &str) -> bool {
    title.starts_with("All Worlds Maintenance")
        || (title.starts_with("全ワールド") && title.contains("メンテナンス"))
}

/// Whether an announcement's stated window covers `now`, in whichever
/// language the page is in.
fn maintenance_window_active(text: &str, now: DateTime<Utc>) -> bool {
    maintenance_active(text, now) || maintenance_active_ja(text, now)
}

/// The Japanese counterpart of `maintenance_active`: only the explicit window
/// the notice states after "日　時：" (JST), as one start and one end,
/// "2026年9月29日(火) 14:00より15:30頃まで" or with the end's own date. Anything
/// else (no window, no year, an end before the start) stays unknown.
fn maintenance_active_ja(text: &str, now: DateTime<Utc>) -> bool {
    let compact: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '頃')
        .collect();
    let Some(rest) = compact
        .split_once("日時：")
        .or_else(|| compact.split_once("日時:"))
        .map(|(_, rest)| rest)
    else {
        return false;
    };
    let Some((window, _)) = rest.split_once("まで") else {
        return false;
    };
    // Weekday marks "(火)" / "（火）".
    let mut plain = String::new();
    let mut depth = 0;
    for c in window.chars() {
        match c {
            '(' | '（' => depth += 1,
            ')' | '）' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let Some((from, to)) = plain
        .split_once("より")
        .or_else(|| plain.split_once("から"))
        .or_else(|| plain.split_once(['～', '〜', '~']))
    else {
        return false;
    };
    let (Some(start), Some(end)) = (ja_clock(from), ja_clock(to)) else {
        return false;
    };
    let (Some(year), Some((month, day))) = (start.year, start.month_day) else {
        return false;
    };
    let jst = FixedOffset::east_opt(9 * 3600).expect("valid offset");
    let at = |year: i32, (month, day): (u32, u32), (hour, minute): (u32, u32)| {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_opt(hour, minute, 0))
            .and_then(|local| local.and_local_timezone(jst).single())
            .map(|moment| moment.with_timezone(&Utc))
    };
    let start_at = at(year, (month, day), start.time);
    let end_at = at(
        end.year.unwrap_or(year),
        end.month_day.unwrap_or((month, day)),
        end.time,
    );
    match (start_at, end_at) {
        (Some(start), Some(end)) if end > start => start <= now && now < end,
        _ => false,
    }
}

/// `[Y年][M月D日]H:MM`, whole string.
struct JaClock {
    year: Option<i32>,
    month_day: Option<(u32, u32)>,
    time: (u32, u32),
}
fn ja_clock(text: &str) -> Option<JaClock> {
    /// A run of ASCII digits followed by `separator`.
    fn number_before<'a>(text: &'a str, separators: &[char]) -> Option<(u32, &'a str)> {
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let (number, rest) = text.split_at(digits);
        let mut chars = rest.chars();
        separators
            .contains(&chars.next()?)
            .then(|| number.parse().ok().map(|n| (n, chars.as_str())))?
    }
    let mut rest = text;
    let mut year = None;
    if let Some((n, after)) = number_before(rest, &['年']) {
        year = Some(i32::try_from(n).ok()?);
        rest = after;
    }
    let mut month_day = None;
    if let Some((month, after)) = number_before(rest, &['月']) {
        let (day, after) = number_before(after, &['日'])?;
        month_day = Some((month, day));
        rest = after;
    }
    let (hour, after) = number_before(rest, &[':', '：'])?;
    let minute_digits = after.chars().take_while(char::is_ascii_digit).count();
    let (minute, trailing) = after.split_at(minute_digits);
    if minute.is_empty() || !trailing.is_empty() {
        return None;
    }
    Some(JaClock { year, month_day, time: (hour, minute.parse().ok()?) })
}

fn maintenance_active(text: &str, now: DateTime<Utc>) -> bool {
    // Deliberately accept only the official explicit, same-day PDT/PST format.
    // Unsupported/multi-day windows stay unknown rather than inventing downtime.
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(rest) = normalized.split("[Date & Time]").nth(1) else {
        return false;
    };
    let (date_range, offset) = if let Some((range, _)) = rest.split_once("(PDT)") {
        (range.trim(), -7 * 3600)
    } else if let Some((range, _)) = rest.split_once("(PST)") {
        (range.trim(), -8 * 3600)
    } else {
        return false;
    };
    let Some((start, end)) = date_range.split_once(" to ") else {
        return false;
    };
    let start = start.replace('.', "");
    let Some((day, _)) = start.rsplit_once(' ') else {
        return false;
    };
    let end = format!("{day} {end}");
    let parse = |value: &str| {
        NaiveDateTime::parse_from_str(value, "%b %d, %Y %H:%M")
            .ok()
            .and_then(|d| {
                d.and_local_timezone(FixedOffset::east_opt(offset)?)
                    .single()
            })
            .map(|d| d.with_timezone(&Utc))
    };
    match (parse(&start), parse(&end)) {
        (Some(start), Some(end)) if end > start => start <= now && now < end,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "Manual public-network probe; contacts official feeds without credentials"]
    async fn live_headline_probe() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let started = std::time::Instant::now();
        let received = Arc::new(AtomicBool::new(false));
        let first = received.clone();
        let snapshot = refresh_with_progress(Lang::En, None, move |update| {
            if let Update::Headlines { items, .. } = update {
                if !items.is_empty() && !first.swap(true, Ordering::SeqCst) {
                    println!("First {} headlines: {} ms", items.len(), started.elapsed().as_millis());
                }
            }
        }).await.unwrap();
        assert!(received.load(Ordering::SeqCst), "Neither official feed supplied headlines: {}", snapshot.news_note);
        println!("Complete refresh: {} ms, {} headlines, {} images", started.elapsed().as_millis(), snapshot.news.len(), snapshot.news.iter().filter(|n| n.image.is_some()).count());
    }

    #[tokio::test]
    async fn first_feed_publishes_while_the_other_feed_is_still_pending() {
        let (release, stalled) = tokio::sync::oneshot::channel();
        let mut feeds = FeedTasks::new();
        feeds.spawn(async { ("Fast", Ok(vec![NewsItem { title: "Fresh headline".into(), url: "https://www.playonline.com/news/fast.html".into(), ..Default::default() }])) });
        feeds.spawn(async { let _ = stalled.await; ("Slow", Err("unavailable".into())) });
        let (sender, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let reader = tokio::spawn(async move { collect_feeds(feeds, &move |update| { let _ = sender.send(update); }).await });
        let Update::Headlines { items, .. } = updates.recv().await.unwrap() else { panic!("expected headlines") };
        assert_eq!(items[0].title, "Fresh headline");
        assert!(!reader.is_finished(), "the slow feed remains pending while the headline is visible");
        release.send(()).unwrap();
        let (items, note) = reader.await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(note.contains("Slow unavailable"));
    }

    #[test]
    fn cached_headlines_load_without_cached_world_status_and_survive_feed_failure() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("news-cache-{}", crate::backend::unique_id().unwrap()));
        let path = root.join("news.json");
        let news = vec![NewsItem { title: "Cached headline".into(), url: "https://www.playonline.com/news/one.html".into(), ..Default::default() }];
        save_cache(&path, Lang::En, &news);
        let mut state = cached(&path, Lang::En);
        assert_eq!(state.as_ref().unwrap().news[0].title, "Cached headline");
        assert_eq!(state.as_ref().unwrap().availability, Availability::Unknown);
        assert!(state.as_ref().unwrap().checked_at.is_empty());
        apply(&mut state, Update::Headlines { items: Vec::new(), note: "Offline".into() });
        assert_eq!(state.as_ref().unwrap().news.len(), 1);
        assert!(state.as_ref().unwrap().news_note.contains("Offline"));
        save_cache(&path, Lang::En, &[]);
        assert!(cached(&path, Lang::En).is_some(), "an outage does not erase the last successful feed");
    }

    #[test]
    fn late_status_and_image_results_preserve_the_headline_list() {
        let item = NewsItem { title: "Headline".into(), url: "https://www.playonline.com/news/one.html".into(), ..Default::default() };
        let mut state = None;
        apply(&mut state, Update::Headlines { items: vec![item.clone()], note: "Fresh".into() });
        let mut status = empty_snapshot();
        status.availability = Availability::Maintenance;
        apply(&mut state, Update::Status(status));
        apply(&mut state, Update::Image(NewsItem { image: Some(iced::widget::image::Handle::from_rgba(1, 1, vec![255; 4])), ..item.clone() }));
        apply(&mut state, Update::Image(item)); // failed subsequent image fetch
        let state = state.unwrap();
        assert_eq!(state.news[0].title, "Headline");
        assert!(state.news[0].image.is_some());
        assert_eq!(state.availability, Availability::Maintenance);
        assert_eq!(state.news_note, "Fresh");
    }
    #[test]
    fn rss_description_and_media_images_survive_plain_text_conversion() {
        let xml = r#"<rss xmlns:media="urn:media"><item><title>Inline</title><link>https://www.playonline.com/pcd2/topics/ff11us/detail/40432/detail.html</link><description><![CDATA[Trial<img src="40432_0.png">]]></description></item><item><title>Media</title><link>https://sqex.to/abc</link><media:content url="https://www.playonline.com/pcd2/topics/ff11us/detail/40435/40435.jpg"/></item><item><title>Enclosure</title><link>/news/article.html</link><enclosure url="/news/article.jpg" type="image/jpeg"/></item><item><title>Bad image</title><link>/news/article2.html</link><description><![CDATA[Hello<img src="https://evil.test/image.jpg">]]></description></item></rss>"#;
        let items = parse_rss(xml).unwrap();
        assert_eq!(items[0].summary, "Trial");
        assert_eq!(
            items[0].image_url,
            "https://www.playonline.com/pcd2/topics/ff11us/detail/40432/40432_0.png"
        );
        assert!(items[1].image_url.ends_with("/40435/40435.jpg"));
        assert!(items[2].image_url.ends_with("/news/article.jpg"));
        assert!(items[3].image_url.is_empty());
    }
    #[test]
    fn topic_banners_are_associated_by_exact_article_link() {
        let html = r#"<div class="summary-banner"><a href="https://sqex.to/oEx3Q"><img src="/pcd2/topics/ff11us/detail/40435/40435.jpg" width="415" height="100"></a></div><div class="summary-banner"><a href="/pcd2/topics/ff11us/detail/40428/detail.html"><img src="/pcd2/topics/ff11us/detail/40428/40428.jpg"></a></div><a href="/other"><img src="/logo.png"></a>"#;
        let images = topic_index_images(html, SOURCES_EN.topic_index);
        assert_eq!(images.len(), 2);
        assert!(images["https://sqex.to/oEx3Q"].ends_with("/40435/40435.jpg"));
        assert!(
            images["https://www.playonline.com/pcd2/topics/ff11us/detail/40428/detail.html"]
                .ends_with("/40428/40428.jpg")
        );
        assert!(!images.contains_key("https://sqex.to/other"));
    }
    #[test]
    fn article_images_reject_site_logos_tracking_and_external_hosts() {
        for image in [
            "/ff11/static/img/OGP-en.png",
            "/imgs/logo.jpg",
            "/imgs/tracking.gif",
            "https://evil.test/banner.jpg",
            "https://sqex.to/banner.jpg",
            "data:image/png;base64,abc",
        ] {
            assert!(
                article_image_url(SOURCES_EN.topic_index, image).is_none(),
                "{image}"
            );
        }
        let page = r#"<meta property="og:image" content="/ff11/static/img/OGP-en.png"><img src="/nav/header.jpg"><div class="topics-detail"><img src="pixel.gif" width="1" height="1"><span class="mdl-img__normal"><img src="40428_0.jpg"></span></div>"#;
        let article = "https://www.playonline.com/pcd2/topics/ff11us/detail/40428/detail.html";
        assert_eq!(
            html_article_image(page, article).unwrap(),
            "https://www.playonline.com/pcd2/topics/ff11us/detail/40428/40428_0.jpg"
        );
        assert!(resolve_official_url(article, "https://questant.jp/q/example").is_none());
        assert!(resolve_official_url("https://sqex.to/abc", article).is_some());
    }
    #[test]
    fn downloaded_images_are_decoded_and_capped_before_painting() {
        let encode = |width, height| {
            let mut output = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut output, width, height);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer
                    .write_image_data(&vec![255; (width * height * 4) as usize])
                    .unwrap();
            }
            output
        };
        assert!(decode_article_image(vec![0, 1, 2]).is_none());
        assert!(decode_article_image(encode(1, 1)).is_none());
        assert!(decode_article_image(encode(4097, 32)).is_none());
        let handle = decode_article_image(encode(1300, 800)).unwrap();
        let iced::widget::image::Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = handle
        else {
            panic!("worker must return ready RGBA pixels");
        };
        assert!(width <= 1200 && height <= 700);
        assert_eq!(pixels.len(), (width * height * 4) as usize);
    }
    #[test]
    fn official_world_states_require_complete_production_worlds() {
        let worlds = [
            "Bahamut",
            "Shiva",
            "Phoenix",
            "Carbuncle",
            "Fenrir",
            "Sylph",
            "Valefor",
            "Leviathan",
            "Odin",
            "Quetzalcoatl",
            "Siren",
            "Ragnarok",
            "Cerberus",
            "Bismarck",
            "Lakshmi",
            "Asura",
        ];
        let page = |status: &str| {
            format!("<div id='bc_zone2'>{}<h2>Atomos (Test Server)</h2><img class='img_status' src='/ff11us/server/imgs/status_02.gif'></div>", worlds.iter().map(|world| format!("<h2>{world}</h2><img class='img_status' src='/ff11us/server/imgs/status_{status}.gif'>")).collect::<String>())
        };
        assert_eq!(
            parse_world_status(&page("00")).unwrap().0,
            Availability::Operational
        );
        assert_eq!(
            parse_world_status(&page("01")).unwrap().0,
            Availability::PartialOutage
        );
        assert_eq!(
            parse_world_status(&page("02")).unwrap().0,
            Availability::Outage
        );
        let partial = page("00").replacen("status_00.gif", "status_02.gif", 1);
        let (state, note) = parse_world_status(&partial).unwrap();
        assert_eq!(state, Availability::PartialOutage);
        assert!(note.contains("Bahamut (down)"));
        assert!(parse_world_status(&page("03")).is_err());
        assert!(parse_world_status("<div id='bc_zone2'><h2>Asura</h2></div>").is_err());
    }
    #[test]
    fn rdf_entities_cdata_and_official_links() {
        let xml = r#"<rdf:RDF xmlns:rdf="urn:rdf" xmlns:dc="urn:dc"><item><title>It&apos;s &amp; news</title><link>http://www.playonline.com/ff11us/polnews/news1.shtml</link><description><![CDATA[Hello<br><b>world &amp; friends</b>]]></description><dc:date>Fri, 02 Oct 2026 17:00:00 JST</dc:date></item><item><title>Untrusted</title><link>https://evil.test/</link></item></rdf:RDF>"#;
        let items = parse_rss(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "It's & news");
        assert_eq!(items[0].summary, "Hello world & friends");
        assert!(rss_date(&items[0].date).is_some());
        assert!(items[0].url.starts_with("https://"));
    }
    #[test]
    fn maintenance_notice_is_not_current_downtime() {
        let text = "[Date & Time] Sep. 28, 2026 22:00 to 23:30 (PDT) [Details]";
        let instant = |s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert!(maintenance_active(text, instant("2026-09-29T05:30:00Z")));
        assert!(!maintenance_active(text, instant("2026-10-02T05:30:00Z")));
        assert!(!maintenance_active(text, instant("2026-09-29T06:30:00Z")));
        assert!(!maintenance_active(
            "Sep. 28 All Worlds Maintenance",
            Utc::now()
        ));
    }

    #[test]
    fn downtime_requires_a_recent_read_and_an_unexpired_maintenance_window() {
        let now = instant("2026-09-29T06:29:59Z");
        let mut snapshot = empty_snapshot();
        snapshot.availability = Availability::Maintenance;
        snapshot.checked_at = "2026-09-29 06:29:00 UTC".into();
        snapshot.notices.push(NewsItem {
            title: "All Worlds Maintenance (Sep. 28)".into(),
            summary: "[Date & Time] Sep. 28, 2026 22:00 to 23:30 (PDT) [Details]".into(),
            ..Default::default()
        });
        assert_eq!(snapshot.recent_availability(now), Availability::Maintenance);
        assert_eq!(snapshot.recent_availability(instant("2026-09-29T06:30:00Z")), Availability::Unknown);
        snapshot.availability = Availability::Outage;
        assert_eq!(snapshot.recent_availability(now), Availability::Maintenance);
        assert_eq!(snapshot.recent_availability(instant("2026-09-29T06:30:00Z")), Availability::Outage,
            "A scheduled end cannot override a recent explicit all-world outage");
        snapshot.checked_at = "2026-09-29 06:23:58 UTC".into();
        assert_eq!(snapshot.recent_availability(now), Availability::Unknown);
        snapshot.checked_at = "2026-09-29 06:30:00 UTC".into();
        assert_eq!(snapshot.recent_availability(now), Availability::Unknown);
        snapshot.checked_at = "cached".into();
        assert_eq!(snapshot.recent_availability(now), Availability::Unknown);
    }

    #[test]
    fn live_world_status_wins_over_announcements_and_errors_do_not_imply_outage() {
        assert_eq!(availability_with_notice(&Ok((Availability::Operational, "up".into())), Some("Maintenance")).0,
            Availability::Operational);
        assert_eq!(availability_with_notice(&Ok((Availability::PartialOutage, "some down".into())), Some("Maintenance")).0,
            Availability::PartialOutage);
        assert_eq!(availability_with_notice(&Ok((Availability::Outage, "down".into())), Some("Maintenance")).0,
            Availability::Outage);
        assert_eq!(availability_with_notice(&Ok((Availability::Outage, "down".into())), None).0, Availability::Outage);
        assert_eq!(availability_with_notice(&Err("unavailable".into()), None).0, Availability::Unknown);
    }
    #[test]
    fn maintenance_index_and_url_boundaries() {
        let html = r#"<table class="ta_info2"><tr><td><span class="tx_info2">Sep. 26</span><a class="info" href="/ff11us/polnews/news1.shtml">All Worlds Maintenance</a></td></tr></table>"#;
        let items = parse_notices(html).unwrap();
        assert_eq!(items[0].date, "Sep. 26");
        assert_eq!(
            items[0].url,
            "https://www.playonline.com/ff11us/polnews/news1.shtml"
        );
        for url in [
            "https://www.playonline.com.evil.test/x",
            "javascript:alert(1)",
            "https://user@www.playonline.com/x",
            "https://www.playonline.com:8443/",
        ] {
            assert!(official_url(url).is_none());
        }
        assert!(official_url("https://sqex.to/abc").is_some());
        assert!(parse_rss("<rss><item>").is_err());
    }

    // ---- Japanese sources (independently authored, fictional parser fixtures) ----
    const JA_TOPICS: &str = include_str!("../fixtures/news/ja/topics.xml");
    const JA_NEWS: &str = include_str!("../fixtures/news/ja/news.xml");
    const JA_TOPICS_LATEST: &str = include_str!("../fixtures/news/ja/topics_latest.html");
    // These two deliberately exercise legacy Shift_JIS bytes.
    const JA_MAINTENANCE_LIST: &[u8] = include_bytes!("../fixtures/news/ja/list_mnt.sjis.html");
    const JA_NOTICE: &[u8] = include_bytes!("../fixtures/news/ja/notice_all_worlds.sjis.html");

    fn instant(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn each_language_reads_its_own_official_site() {
        assert_eq!(sources(Lang::En).topics, "https://www.playonline.com/pcd2/topics/ff11us/topics.xml");
        assert_eq!(sources(Lang::En).news, "https://www.playonline.com/ff11us/polnews/news.xml");
        assert_eq!(sources(Lang::En).maintenance, "https://www.playonline.com/ff11us/info/list_mnt.shtml");
        assert_eq!(sources(Lang::En).home, "https://www.playonline.com/ff11us/");
        let ja = sources(Lang::Ja);
        assert_eq!(ja.topics, "https://www.playonline.com/pcd2/topics/ff11/topics.xml");
        assert_eq!(ja.news, "https://www.playonline.com/ff11/polnews/news.xml");
        assert_eq!(ja.maintenance, "https://www.playonline.com/ff11/info/list_mnt.shtml");
        assert_eq!(ja.topic_index, "https://www.playonline.com/pcd2/topics/ff11/topics_latest.html");
        for url in [ja.topics, ja.news, ja.maintenance, ja.topic_index, ja.home] {
            assert!(!url.contains("ff11us"), "{url}");
            assert_eq!(official_url(url).as_deref(), Some(url), "{url} is on the allowlist unchanged");
        }
    }

    #[test]
    fn japanese_topics_feed_parses_with_text_images_and_dates() {
        let items = parse_rss(JA_TOPICS).unwrap();
        assert_eq!(items[0].title, "表示テスト & お知らせ");
        assert_eq!(items[0].url, "https://sqex.to/crystal-test-update");
        assert_eq!(
            items[0].summary,
            "表示テストです。 続きは \"案内\" を確認してください。"
        );
        assert_eq!(
            rss_date(&items[0].date).unwrap(),
            instant("2026-10-06T08:00:00Z"),
            "feed dates are JST"
        );
        // Article images inside the description are kept; only official hosts qualify.
        assert_eq!(
            items[1].image_url,
            "https://www.playonline.com/pcd2/topics/ff11/detail/fixture-image/fixture-image_0.png"
        );
        assert_eq!(rss_date(&items[1].date).unwrap(), instant("2026-10-05T00:30:00Z"));
        let period = items.iter().find(|i| i.url.ends_with("/ff11/detail/fixture-period/detail.html")).unwrap();
        assert_eq!(period.title, "表示期間の試験記事 【確認用】", "numeric entities are decoded");
        // No spaces invented between tags inside Japanese text: <span>二週間</span>に変更.
        assert!(period.summary.contains("二週間に変更"), "{}", period.summary);
        assert!(period.summary.contains("表示枠を20 → 30に拡張"), "{}", period.summary);
        assert!(items.iter().all(|i| !i.title.contains("&amp;") && !i.summary.contains('<')));
    }

    #[test]
    fn japanese_information_feed_links_are_upgraded_to_https() {
        let items = parse_rss(JA_NEWS).unwrap();
        assert_eq!(items[1].title, "全ワールド メンテナンス試験のお知らせ(10/30)");
        assert_eq!(items[1].url, "https://www.playonline.com/ff11/polnews/news-fixture-maintenance.shtml");
        assert!(items[1].summary.contains("日 時：2026年10月30日(金) 14:00より15:30頃まで"), "{}", items[1].summary);
        assert_eq!(
            rss_date(&items[1].date).unwrap(),
            instant("2026-10-28T05:00:31Z")
        );
        let all_worlds: Vec<_> = items.iter().filter(|i| is_all_worlds_notice(&i.title)).collect();
        assert_eq!(all_worlds.len(), 1, "monthly processing is not an all-worlds window");
    }

    #[test]
    fn japanese_banners_are_matched_by_article_link() {
        let images = topic_index_images(JA_TOPICS_LATEST, SOURCES_JA.topic_index);
        assert_eq!(
            images["https://sqex.to/crystal-test-update"],
            "https://www.playonline.com/pcd2/topics/ff11/detail/fixture-update/banner.jpg"
        );
        // A relative article href and the tpimgL selector find the RSS item.
        let rss = parse_rss(JA_TOPICS).unwrap();
        let period = rss.iter().find(|i| i.title.starts_with("表示期間")).unwrap();
        assert_eq!(
            images[&period.url],
            "https://www.playonline.com/pcd2/topics/ff11/detail/fixture-period/banner.jpg"
        );
        // The banner index does not pick up inline article images.
        assert!(images.values().all(|u| !u.contains("_0.png")));
    }

    #[test]
    fn japanese_allowlist_covers_exactly_the_official_hosts() {
        for ok in [
            "http://www.playonline.com/ff11/polnews/news27777.shtml",
            "/ff11/polnews/news27777.shtml",
            "/pcd2/topics/ff11/detail/40431/detail.html",
            "https://playonline.com/pcd2/topics/ff11/detail/40431/detail.html",
            "https://sqex.to/1Twec",
        ] {
            let url = official_url(ok).unwrap_or_else(|| panic!("{ok}"));
            assert!(url.starts_with("https://"), "{url}");
        }
        for bad in [
            "https://www.playonline.com.evil.test/ff11/polnews/news1.shtml",
            "https://evil.test/pcd2/topics/ff11/detail/40431/detail.html",
            "https://user@www.playonline.com/ff11/index.html",
            "https://www.playonline.com:8443/ff11/index.html",
            "ftp://www.playonline.com/ff11/index.html",
            "https://www.playonline.co.jp/ff11/index.html",
        ] {
            assert!(official_url(bad).is_none(), "{bad}");
        }
        let article = "https://www.playonline.com/pcd2/topics/ff11/detail/40431/detail.html";
        for ok in ["40431_0.png", "/pcd2/topics/ff11/detail/40431/40431.jpg", "https://www.playonline.com/ff11/polnews/imgs/news27774_0.jpg"] {
            assert!(article_image_url(article, ok).is_some(), "{ok}");
        }
        for bad in [
            "/ff11/imgs/spacer.gif",
            "/ff11/static/img/OGP-ja.png",
            "/ff11/imgs/info/infohd02.gif",
            "/ff11us/imgs/info/infohd02.gif",
            "/pcd2/topics/ff11/common/banner.jpg",
            "/ff11/imgs/logo.png",
            "https://evil.test/pcd2/topics/ff11/detail/40431/40431.jpg",
            "https://sqex.to/pcd2/topics/ff11/detail/40431/40431.jpg",
            "data:image/png;base64,abc",
        ] {
            assert!(article_image_url(article, bad).is_none(), "{bad}");
        }
        assert!(resolve_official_url(article, "https://questant.jp/q/example").is_none());
        // Site-wide OGP art on a Japanese article is never taken as its picture.
        let page = r#"<meta property="og:image" content="/ff11/static/img/OGP-ja.png"><div class="topics-detail"><img src="40431_0.png"></div>"#;
        assert_eq!(
            html_article_image(page, article).unwrap(),
            "https://www.playonline.com/pcd2/topics/ff11/detail/40431/40431_0.png"
        );
    }

    #[test]
    fn shift_jis_pages_decode_without_a_declaration_and_with_one() {
        // The maintenance list is a bare fragment: no meta tag, no header charset.
        assert_eq!(declared_charset(JA_MAINTENANCE_LIST), None);
        let list = decode_page(JA_MAINTENANCE_LIST, Lang::Ja);
        assert!(list.contains("全ワールド メンテナンス試験のお知らせ(10/30)"), "{list}");
        assert!(!list.contains('\u{fffd}'));
        let notices = parse_notices(&list).unwrap();
        assert_eq!(notices.len(), 3);
        assert_eq!(notices[0].title, "月次集計のテスト通知(10/31)");
        assert_eq!(notices[0].date, "2026.10.29(木) 09:15 From: テスト配信");
        assert_eq!(notices[0].url, "https://www.playonline.com/ff11/polnews/news-fixture-monthly.shtml");
        assert_eq!(notices[1].url, "https://www.playonline.com/ff11/polnews/news-fixture-maintenance.shtml");
        // The announcement page declares Shift_JIS in its meta tag.
        assert_eq!(declared_charset(JA_NOTICE).as_deref(), Some("shift_jis"));
        let body = notice_body(&decode_page(JA_NOTICE, Lang::Ja));
        assert!(body.contains("日　時：2026年10月30日(金) 14:00より15:30頃まで"), "{body}");
        assert!(!body.contains("本文外の識別文") && !body.contains("ここも本文ではありません"), "only the content zone is scraped");
        // English reads the same bytes as before (lossy UTF-8): never mojibake-corrected.
        assert!(decode_page(JA_MAINTENANCE_LIST, Lang::En).contains('\u{fffd}'));
    }

    #[test]
    fn japanese_decoding_handles_utf8_euc_jp_and_garbage() {
        let utf8 = "<p>メンテナンス</p>".as_bytes();
        assert_eq!(decode_page(utf8, Lang::Ja), "<p>メンテナンス</p>");
        // "プレイオンライン" in EUC-JP, declared.
        let mut euc = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=EUC-JP\"><p>".to_vec();
        euc.extend([0xA5, 0xD7, 0xA5, 0xEC, 0xA5, 0xA4, 0xA5, 0xAA, 0xA5, 0xF3, 0xA5, 0xE9, 0xA5, 0xA4, 0xA5, 0xF3]);
        assert!(decode_page(&euc, Lang::Ja).ends_with("<p>プレイオンライン"));
        // Undecodable bytes never panic and never come back empty.
        assert!(!decode_page(&[0xff, 0xfe, 0xfd, 0x80], Lang::Ja).is_empty());
        assert_eq!(decode_page(b"", Lang::Ja), "");
        assert_eq!(declared_charset(b"<meta charset='UTF-8'>").as_deref(), Some("utf-8"));
    }

    #[test]
    fn japanese_maintenance_windows_are_read_as_jst() {
        let notice = notice_body(&decode_page(JA_NOTICE, Lang::Ja));
        assert!(is_all_worlds_notice("全ワールド メンテナンス試験のお知らせ(10/30)"));
        assert!(!is_all_worlds_notice("アカウント画面 メンテナンス試験(10/29)"));
        assert!(is_all_worlds_notice("All Worlds Maintenance (Sep. 28)"));
        // 2026-10-30 14:00 to 15:30 JST = 05:00 to 06:30 UTC; the end is exclusive.
        assert!(!maintenance_window_active(&notice, instant("2026-10-30T04:59:59Z")));
        assert!(maintenance_window_active(&notice, instant("2026-10-30T05:00:00Z")));
        assert!(maintenance_window_active(&notice, instant("2026-10-30T06:29:59Z")));
        assert!(!maintenance_window_active(&notice, instant("2026-10-30T06:30:00Z")));
        // A window that crosses midnight gives the end its own date.
        let crossing = "日　時：2026年9月30日(水) 23:50頃より\n　　　　2026年10月1日(木) 2:00頃まで\n※終了予定時刻は変更する場合があります。";
        assert!(maintenance_active_ja(crossing, instant("2026-09-30T15:30:00Z")));
        assert!(!maintenance_active_ja(crossing, instant("2026-09-30T17:00:00Z")));
        // A range sign and an ASCII colon are accepted.
        assert!(maintenance_active_ja("日時：2026年10月13日(火) 14:00～16:30頃まで", instant("2026-10-13T06:00:00Z")));
        // Anything that is not one explicit start and end stays unknown.
        for unknown in [
            "ファイナルファンタジーXIのメンテナンスを実施します。",
            "日　時：9月29日(火) 14:00より15:30頃まで",
            "日　時：2026年9月29日(火) 15:30より14:00頃まで",
            "日　時：2026年9月29日(火) 14:00頃から順次",
            "日　時：2026年9月31日 14:00より15:30まで",
            "",
        ] {
            assert!(!maintenance_active_ja(unknown, instant("2026-09-29T05:30:00Z")), "{unknown}");
        }
        // English notices still work through the combined check.
        let english = "[Date & Time] Sep. 28, 2026 22:00 to 23:30 (PDT) [Details]";
        assert!(maintenance_window_active(english, instant("2026-09-29T05:30:00Z")));
    }

    #[test]
    fn plain_text_conversion_keeps_english_word_spaces() {
        assert_eq!(plain_html("<p>Hello</p><p>there</p>"), "Hello there");
        assert_eq!(plain_html("Update<b>d</b> today"), "Update d today", "unchanged English behaviour");
        assert_eq!(plain_html("<strong>無期限</strong>に変更<br>次の行"), "無期限に変更 次の行");
        assert_eq!(plain_html("Lv<b>75</b>に"), "Lv 75に", "a space only where neither side is Japanese");
    }

    #[test]
    fn headlines_are_cached_per_language() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("news-lang-{}", crate::backend::unique_id().unwrap()));
        let en_path = cache_path(&root.join("news-cache.json"), Lang::En);
        let ja_path = cache_path(&root.join("news-cache.json"), Lang::Ja);
        assert_eq!(en_path, root.join("news-cache.json"), "the English file keeps its name");
        assert_eq!(ja_path, root.join("news-cache.ja.json"));
        let english = vec![NewsItem { title: "English headline".into(), url: "https://www.playonline.com/ff11us/polnews/news1.shtml".into(), ..Default::default() }];
        let japanese = vec![NewsItem { title: "日本語の見出し".into(), url: "https://www.playonline.com/ff11/polnews/news2.shtml".into(), ..Default::default() }];
        save_cache(&en_path, Lang::En, &english);
        assert!(cached(&ja_path, Lang::Ja).is_none(), "no Japanese cache yet: the English one is not shown");
        save_cache(&ja_path, Lang::Ja, &japanese);
        assert_eq!(cached(&en_path, Lang::En).unwrap().news[0].title, "English headline");
        assert_eq!(cached(&ja_path, Lang::Ja).unwrap().news[0].title, "日本語の見出し");
        // Even a file in the wrong place is refused rather than shown in the wrong language.
        assert!(cached(&ja_path, Lang::En).is_none());
        assert!(cached(&en_path, Lang::Ja).is_none());
        // A cache written before the language tag existed holds English.
        std::fs::write(&en_path, br#"{"items":[{"title":"Old","url":"https://www.playonline.com/ff11us/polnews/news0.shtml","date":"","summary":"","image_url":""}],"saved_at":"2026-09-01 00:00:00 UTC"}"#).unwrap();
        assert_eq!(cached(&en_path, Lang::En).unwrap().news[0].title, "Old");
        assert!(cached(&en_path, Lang::Ja).is_none());
    }

    #[tokio::test]
    #[ignore = "Manual public-network probe; contacts the official Japanese feeds without credentials"]
    async fn live_japanese_headline_probe() {
        let snapshot = refresh_with_progress(Lang::Ja, None, |_| {}).await.unwrap();
        assert!(!snapshot.news.is_empty(), "{}", snapshot.news_note);
        println!(
            "{} headlines, {} images, {} notices; first: {}; availability {:?}; {}",
            snapshot.news.len(),
            snapshot.news.iter().filter(|n| n.image.is_some()).count(),
            snapshot.notices.len(),
            snapshot.news[0].title,
            snapshot.availability,
            snapshot.news_note,
        );
        assert!(snapshot.news.iter().any(|n| n.title.chars().any(is_cjk)));
        assert!(snapshot.notices.iter().all(|n| n.title.chars().any(is_cjk) || n.title.is_empty()));
    }
}
