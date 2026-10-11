//! Public item lookups with an isolated world preference. FFXIAH gives its
//! browser cookie precedence over a link's `sid`, so an external link alone
//! cannot reliably select a world. These requests never use browser cookies.
use crate::i18n::{t, tf};
use scraper::{Html, Selector};
use std::time::Duration;

const HOST: &str = "www.ffxiah.com";
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Quote {
    pub name: String,
    pub url: String,
    pub stack: Option<u64>,
    pub price: Option<u64>,
    pub stock: Option<u64>,
    pub last_sale: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Results {
    pub items: Vec<Quote>,
    pub truncated: bool,
}

#[derive(Default)]
pub(crate) struct Lookup {
    pub open: bool,
    pub query: String,
    pub world: &'static str,
    pub generation: u64,
    pub result: Option<Result<Results, String>>,
}
impl Lookup {
    pub fn begin(&mut self, query: &str, world: &'static str) -> u64 {
        self.open = true;
        self.query = query.trim().to_owned();
        self.world = world;
        self.generation += 1;
        self.result = None;
        self.generation
    }
    pub fn finish(&mut self, generation: u64, result: Result<Results, String>) {
        if self.open && generation == self.generation { self.result = Some(result); }
    }
}

fn request(client: &reqwest::Client, query: &str, sid: u8) -> reqwest::RequestBuilder {
    client.get("https://www.ffxiah.com/search/item")
        .query(&[("q", query.trim())])
        .header(reqwest::header::COOKIE, format!("sid={sid}"))
}

pub(crate) async fn search(query: String, world: &'static str) -> Result<Results, String> {
    let sid = crate::ui::ah_server_id(world).ok_or_else(|| t("Choose a valid FFXIAH world.").to_owned())?;
    if query.trim().is_empty() || query.chars().count() > 160 {
        return Err(t("Enter an item name of up to 160 characters.").into());
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("Crystal-Launcher/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 8 || attempt.url().scheme() != "https" || attempt.url().host_str() != Some(HOST) {
                attempt.error("Unexpected FFXIAH redirect")
            } else { attempt.follow() }
        }))
        .build().map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))?;
    let mut response = request(&client, &query, sid).send().await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))?;
    let url = response.url().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| tf("FFXIAH lookup failed: {error}", &[("error", &e)]))? {
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(t("FFXIAH returned too much data. Try a more specific item name.").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse(&String::from_utf8_lossy(&bytes), &url, sid)
}

fn selector(s: &str) -> Selector { Selector::parse(s).expect("static selector") }
fn plain(element: scraper::ElementRef<'_>) -> String {
    element.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}
fn number(s: &str) -> Option<u64> {
    s.trim().replace(',', "").parse().ok()
}
fn item_url(href: &str, base: &reqwest::Url, sid: u8) -> Option<String> {
    let mut url = base.join(href).ok()?;
    if url.host_str() != Some(HOST) || url.port().is_some() || !url.username().is_empty() || url.password().is_some() { return None; }
    if !matches!(url.scheme(), "http" | "https") { return None; }
    let id = url.path().strip_prefix("/item/")?.split('/').next()?;
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit()) { return None; }
    url.set_scheme("https").ok()?;
    url.set_fragment(None);
    let stack = url.query_pairs().any(|(k, v)| k == "stack" && v == "1");
    url.set_query(None);
    if stack { url.query_pairs_mut().append_pair("stack", "1"); }
    url.query_pairs_mut().append_pair("sid", &sid.to_string());
    Some(url.into())
}

fn parse(html: &str, url: &reqwest::Url, sid: u8) -> Result<Results, String> {
    let doc = Html::parse_document(html);
    // Never label fallback/default-world data as the requested world's data.
    let selected = doc.select(&selector("#ffxi-main-server-select option[selected]"))
        .next().and_then(|e| e.value().attr("value")).and_then(|v| v.parse::<u8>().ok());
    if selected != Some(sid) { return Err(t("FFXIAH did not confirm the selected world. Retry or use the website.").into()); }
    let table = selector("table.stdlist");
    if let Some(table) = doc.select(&table).next() {
        let row_sel = selector("tr");
        let cell_sel = selector("td");
        let link_sel = selector("a[href]");
        let mut items = Vec::new();
        let mut truncated = false;
        for row in table.select(&row_sel) {
            let cells: Vec<_> = row.select(&cell_sel).collect();
            if cells.len() < 8 { continue; }
            let Some(link) = cells[1].select(&link_sel).next() else { continue; };
            let Some(url) = link.value().attr("href").and_then(|h| item_url(h, url, sid)) else { continue; };
            if items.len() == MAX_RESULTS { truncated = true; break; }
            let name = plain(link);
            if name.is_empty() || name.len() > 512 { continue; }
            let label = plain(cells[1]);
            let stack = label.strip_prefix(&name).map(str::trim).and_then(|s| s.strip_prefix('x')).and_then(number);
            items.push(Quote { name, url, stack, price: number(&plain(cells[5])), stock: number(&plain(cells[6])), last_sale: false });
        }
        return Ok(Results { items, truncated });
    }
    if let Some(item_url) = item_url(url.as_str(), url, sid) {
        let name = doc.select(&selector(".item-name .ucwords, #tbl-header h1")).next().map(plain)
            .filter(|n| !n.is_empty() && n.len() <= 512)
            .ok_or_else(|| t("FFXIAH's item page could not be read.").to_owned())?;
        // Exact-name searches redirect to the item page. Read its public sales
        // data as JSON, never execute website scripts or import their code.
        let sales = doc.select(&selector("script")).find_map(|e| {
            let text = e.text().collect::<String>();
            let tail = text.split_once("Item.sales")?.1.trim_start().strip_prefix('=')?.trim_start();
            <Option<Vec<Sale>> as serde::Deserialize>::deserialize(&mut serde_json::Deserializer::from_str(tail)).ok()
        }).ok_or_else(|| t("FFXIAH's item prices could not be read.").to_owned())?;
        return Ok(Results { items: vec![Quote { name, url: item_url, stack: None,
            price: sales.and_then(|s| s.first().map(|s| s.price)), stock: None, last_sale: true }], truncated: false });
    }
    let title = doc.select(&selector("title")).next().map(plain).unwrap_or_default();
    if title == "Item Search - FFXIAH.com" { return Ok(Results::default()); }
    Err(t("FFXIAH's search results could not be read.").into())
}

#[derive(serde::Deserialize)]
struct Sale { price: u64 }

#[cfg(test)]
mod tests {
    use super::*;
    fn page(world: u8, body: &str) -> String {
        format!("<html><select id='ffxi-main-server-select'><option selected value='{world}'>World</option></select>{body}</html>")
    }
    #[test]
    fn lookup_sends_the_world_cookie_that_the_site_uses() {
        let request = request(&reqwest::Client::new(), "  Ore & ingot  ", 25).build().unwrap();
        assert_eq!(request.headers()[reqwest::header::COOKIE], "sid=25");
        assert_eq!(request.url().as_str(), "https://www.ffxiah.com/search/item?q=Ore+%26+ingot");
    }
    #[test]
    fn list_prices_and_stacks_belong_to_the_confirmed_world() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/search/item?q=ore").unwrap();
        let html = page(25, "<table class='stdlist'><tr><td></td><td><a href='http://www.ffxiah.com/item/9001//?stack=1'><span>Test</span> Ore</a> x12</td><td>0</td><td>0</td><td>Description</td><td><span>12,345</span></td><td>7</td><td>2.5</td></tr></table>");
        let results = parse(&html, &url, 25).unwrap();
        assert_eq!((results.items[0].name.as_str(), results.items[0].stack, results.items[0].price, results.items[0].stock), ("Test Ore", Some(12), Some(12345), Some(7)));
        assert_eq!(results.items[0].url, "https://www.ffxiah.com/item/9001//?stack=1&sid=25");
        assert!(parse(&html, &url, 28).is_err());
    }
    #[test]
    fn exact_search_reads_the_last_sale_without_executing_scripts() {
        let url = reqwest::Url::parse("https://www.ffxiah.com/item/9001/test").unwrap();
        let html = page(1, "<span class='item-name'><span class='ucwords'>Test Ore</span></span><script>Item.sales = [{\"price\":4321,\"seller_name\":\"ignored\"}]; throw new Error();</script>");
        let results = parse(&html, &url, 1).unwrap();
        assert_eq!(results.items[0].price, Some(4321));
        assert!(results.items[0].last_sale);
        assert_eq!(parse(&html.replace("[{\"price\":4321,\"seller_name\":\"ignored\"}]", "null"), &url, 1).unwrap().items[0].price, None);
    }
    #[test]
    fn unexpected_links_and_unconfirmed_worlds_are_rejected() {
        let base = reqwest::Url::parse("https://www.ffxiah.com/search/item").unwrap();
        for link in ["https://example.com/item/9001", "javascript:alert(1)", "/login", "https://user@www.ffxiah.com/item/9001"] {
            assert!(item_url(link, &base, 28).is_none());
        }
        assert!(parse("<title>Blocked</title>", &base, 28).is_err());
    }
    #[test]
    fn old_requests_cannot_replace_a_new_world_or_reopen_a_closed_lookup() {
        let mut lookup = Lookup::default();
        let old = lookup.begin("ore", "Asura");
        let current = lookup.begin("ore", "Bismarck");
        lookup.finish(old, Ok(Results::default()));
        assert!(lookup.result.is_none());
        lookup.finish(current, Ok(Results::default()));
        assert!(lookup.result.is_some());
        lookup.open = false;
        lookup.finish(current, Err("old".into()));
        assert!(lookup.result.as_ref().unwrap().is_ok());
    }
    #[tokio::test]
    #[ignore = "Read-only public FFXIAH lookup; requires internet"]
    async fn live_lookup_confirms_two_worlds_and_an_exact_item() {
        for world in ["Asura", "Bismarck"] {
            let list = search("copper".into(), world).await.unwrap();
            assert!(!list.items.is_empty());
            println!("{world}: {} matches, first price {:?}", list.items.len(), list.items[0].price);
            let exact = search("Copper Ore".into(), world).await.unwrap();
            assert!(!exact.items.is_empty());
            println!("{world}: exact lookup {}, price {:?}", exact.items[0].name, exact.items[0].price);
        }
    }
}
