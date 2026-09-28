use axum::{
    body::Body,
    extract::{Form, Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::net::TcpListener;

mod db;
mod metadata;
mod scraper;

use db::{
    count_pending_metadata, get_filtered_movies, get_movie_metadata, get_movie_poster,
    get_pending_metadata, get_stats, init_db, save_backfilled_metadata, save_movie_metadata,
    search_movies_page, toggle_watched, MovieMetadata, MoviePage,
};

const PAGE_SIZE: usize = 50;

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
}

#[derive(serde::Serialize)]
pub struct MovieResponse {
    id: i64,
    title: String,
    year: u16,
    is_watched: bool,
    wiki_url: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
}

#[derive(serde::Deserialize)]
struct ImdbSuggestionResponse {
    d: Option<Vec<ImdbSuggestion>>,
}

#[derive(serde::Deserialize)]
struct ImdbSuggestion {
    id: String,
    #[serde(rename = "l")]
    title: String,
    #[serde(rename = "y")]
    year: Option<u16>,
    #[serde(rename = "i")]
    image: Option<ImdbImage>,
}

#[derive(serde::Deserialize)]
struct ImdbImage {
    #[serde(rename = "imageUrl")]
    url: String,
}

#[derive(serde::Deserialize)]
struct WikipediaSummary {
    extract: Option<String>,
    thumbnail: Option<WikipediaThumbnail>,
}

#[derive(serde::Deserialize)]
struct WikipediaThumbnail {
    source: String,
}

fn for_render(page: MoviePage) -> String {
    let summary = if page.total == 0 {
        "<div class='results-summary' role='status' aria-live='polite'>No movies found</div>".into()
    } else {
        let first = (page.page - 1) * PAGE_SIZE + 1;
        let last = (first + page.movies.len() - 1).min(page.total);
        format!(
            "<div class='results-summary' role='status' aria-live='polite'>Showing {first}–{last} of {} movies</div>",
            page.total
        )
    };
    if page.movies.is_empty() {
        return format!("{summary}<div class='empty-state'><div class='icon'>🔎</div><h3>No movies found</h3><p>Try a different search term or filter to explore more titles.</p></div>");
    }
    let current_page = format!(
        "<input class='current-page-input' id='current-page' type='hidden' name='page' value='{}'>",
        page.page
    );
    let pagination = render_pagination(&page);
    let rows = page
        .movies
        .into_iter()
        .map(render_movie_item)
        .collect::<String>();
    format!("{summary}{current_page}{rows}{pagination}")
}

fn page_button(page: usize, current_page: usize, label: &str, title: &str) -> String {
    let current = if page == current_page {
        " aria-current='page'"
    } else {
        ""
    };
    format!(
        "<button class='page-button'{current} type='button' hx-get='/filter?page={page}' hx-include='#search-query,#active-filter,#sort-order,#year-filter' hx-target='#list' hx-indicator='#s' aria-label='{title}' title='{title}'>{label}</button>"
    )
}

fn render_pagination(page: &MoviePage) -> String {
    if page.page_count <= 1 {
        return String::new();
    }
    let start = page.page.saturating_sub(2).max(1);
    let end = page.page.saturating_add(2).min(page.page_count);
    let mut buttons = Vec::new();
    if page.page > 1 {
        buttons.push(page_button(1, page.page, "«", "First page"));
        buttons.push(page_button(page.page - 1, page.page, "‹", "Previous page"));
    }
    if start > 1 {
        buttons.push("<span class='page-ellipsis' aria-hidden='true'>…</span>".into());
    }
    for number in start..=end {
        buttons.push(page_button(
            number,
            page.page,
            &number.to_string(),
            &format!("Page {number}"),
        ));
    }
    if end < page.page_count {
        buttons.push("<span class='page-ellipsis' aria-hidden='true'>…</span>".into());
    }
    if page.page < page.page_count {
        buttons.push(page_button(page.page + 1, page.page, "›", "Next page"));
        buttons.push(page_button(page.page_count, page.page, "»", "Last page"));
    }
    format!(
        "<nav class='pagination' aria-label='Movie pages'><span class='page-position'>Page {} of {}</span><div class='page-links'>{}</div></nav>",
        page.page,
        page.page_count,
        buttons.join("")
    )
}

fn render_movie_item(movie: (i64, String, u16, bool, String)) -> String {
    let (id, title, year, watched, wiki_url) = movie;
    format!(
        "<div class='movie-item'><div class='info'><span class='status-badge {state_class}'>{state_label}</span><span class='title {watched_class}'>{title}</span><span class='movie-meta'><span class='year'>{year}</span>{wiki}</span><button class='details-button' type='button' hx-get='/metadata/{id}' hx-target='#movie-dialog-content' hx-swap='innerHTML' hx-indicator='this' onclick=\"document.getElementById('movie-dialog-content').innerHTML='<p class=metadata-loading>Loading details...</p>'; document.getElementById('movie-dialog').showModal();\">Movie details</button></div><label><input type='checkbox' {checked} hx-post='/toggle/{id}' hx-include='#search-query,#active-filter,#sort-order,#year-filter,#current-page' hx-target='#list' hx-swap='innerHTML'><span>Watched</span></label></div>",
        id = id, title = escape_html(&title), year = year,
        watched_class = if watched { "completed" } else { "" },
        state_class = if watched { "watched" } else { "unwatched" },
        state_label = if watched { "Watched" } else { "Unwatched" },
        wiki = if wiki_url.is_empty() { String::new() } else { format!("<a href='{}' class='wiki' target='_blank' rel='noopener noreferrer' aria-label='Read {title} on Wikipedia'>Wikipedia</a>", escape_html(&wiki_url), title = escape_html(&title)) },
        checked = if watched { "checked" } else { "" },
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn empty_movie_page() -> MoviePage {
    MoviePage {
        movies: Vec::new(),
        total: 0,
        page: 1,
        page_count: 1,
    }
}

fn response_movies(movies: Vec<(i64, String, u16, bool, String)>) -> Vec<MovieResponse> {
    movies
        .into_iter()
        .map(|(id, title, year, is_watched, wiki_url)| MovieResponse {
            id,
            title,
            year,

            is_watched,
            wiki_url: (!wiki_url.is_empty()).then_some(wiki_url),
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Init DB...");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("movies.db")?;
    let p = init_db("sqlite://./movies.db").await?;
    println!("Ready!");

    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!(
            "Usage: movie_tracker [--import-year YEAR] [--backfill-metadata [--contact EMAIL_OR_URL] [--limit N] [--delay-ms N]]\n\
             Run without arguments to start the web app. Metadata backfill is resumable;\n\
             use --contact or MOVIE_TRACKER_CONTACT for Wikimedia requests."
        );
        return Ok(());
    }
    if let Some(index) = args.iter().position(|argument| argument == "--import-year") {
        let year = args
            .get(index + 1)
            .ok_or("Missing year after --import-year")?
            .parse::<u16>()?;
        let movies = scraper::scrape_year(year)
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let (found, added) = db::add_movies_batch(&p, &movies).await?;
        println!("Year {year}: scraped {found} films, added {added} new records.");
        return Ok(());
    }
    if args
        .iter()
        .any(|argument| argument == "--backfill-metadata")
    {
        let limit = cli_option(&args, "--limit")?;
        let delay_ms = cli_option(&args, "--delay-ms")?.unwrap_or(1000);
        let contact = cli_string_option(&args, "--contact")?
            .or_else(|| std::env::var("MOVIE_TRACKER_CONTACT").ok())
            .filter(|contact| !contact.trim().is_empty())
            .ok_or("Set --contact or MOVIE_TRACKER_CONTACT before backfilling Wikimedia")?;
        run_metadata_backfill(&p, limit, delay_ms, &contact).await?;
        return Ok(());
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/toggle/:i", post(toggle))
        .route("/search", get(search))
        .route("/filter", get(filter))
        .route("/metadata/:id", get(movie_details))
        .route("/poster/:id", get(movie_poster))
        .route("/movies", get(list))
        .route("/stats", get(stats))
        .route("/seed", post(seed))
        .route("/htmx.min.js", get(htmx))
        .route("/style.css", get(scss))
        .fallback(not_found)
        .with_state(AppState { db: p });

    let a: SocketAddr = "127.0.0.1:3000".parse()?;
    println!("\nhttp://127.0.0.1:3000\n");
    axum::serve(TcpListener::bind(a).await?, app).await?;
    Ok(())
}

fn cli_option(args: &[String], name: &str) -> Result<Option<usize>, Box<dyn std::error::Error>> {
    args.iter()
        .position(|argument| argument == name)
        .map(|index| {
            args.get(index + 1)
                .ok_or_else(|| format!("Missing value after {name}"))?
                .parse::<usize>()
                .map_err(Into::into)
        })
        .transpose()
}

fn cli_string_option(
    args: &[String],
    name: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    args.iter()
        .position(|argument| argument == name)
        .map(|index| {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("Missing value after {name}").into())
        })
        .transpose()
}

async fn run_metadata_backfill(
    pool: &SqlitePool,
    limit: Option<usize>,
    delay_ms: usize,
    contact: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let pending_count = count_pending_metadata(pool).await? as usize;
    let (total_movies, _) = get_stats(pool).await?;
    let total_movies = total_movies as usize;
    let already_complete = total_movies.saturating_sub(pending_count);
    let limit = limit.unwrap_or(pending_count);
    let pending = get_pending_metadata(pool, limit).await?;
    if pending.is_empty() {
        println!("Movie metadata is already complete.");
        return Ok(());
    }

    let user_agent = format!("MovieTracker/0.1 ({contact}; offline movie metadata cache)");
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let delay = std::time::Duration::from_millis(delay_ms as u64);
    let mut completed = 0usize;
    let mut missing = 0usize;
    let mut image_failures = 0usize;

    println!(
        "Checkpoint: {already_complete}/{total_movies} movies complete; {pending_count} pending."
    );
    println!(
        "Resuming at ID {}: {} ({}). This batch covers {} pending movies; request delay: {} ms.",
        pending[0].id,
        pending[0].title,
        pending[0].year,
        pending.len(),
        delay_ms
    );

    for (index, movie) in pending.iter().enumerate() {
        println!(
            "[pending {}/{} | ID {}] {} ({})",
            index + 1,
            pending.len(),
            movie.id,
            movie.title,
            movie.year
        );
        if index > 0 {
            tokio::time::sleep(delay).await;
        }

        let summary = match metadata::fetch_wikipedia_summary(&client, &movie.wiki_url).await {
            Ok(summary) => summary,
            Err(error) => {
                eprintln!(
                    "Stopped at ID {}: {} ({}): {error}. Completed records are checkpointed; rerun to retry this title.",
                    movie.id, movie.title, movie.year
                );
                return Err(error.into());
            }
        };

        let Some(summary) = summary else {
            save_backfilled_metadata(pool, movie.id, None, None, None, None, true).await?;
            missing += 1;
            completed += 1;
            continue;
        };

        let plot = summary.extract.filter(|extract| !extract.trim().is_empty());
        let image_url = summary
            .thumbnail
            .and_then(|thumbnail| metadata::validated_image_url(&thumbnail.source));
        let (downloaded_image, image_failed) = if let Some(url) = image_url.as_deref() {
            tokio::time::sleep(delay).await;
            match metadata::download_image(&client, url).await {
                Ok(image) => (image, false),
                Err(error) => {
                    eprintln!("Poster download failed for {}: {error}", movie.title);
                    image_failures += 1;
                    (None, true)
                }
            }
        } else {
            (None, false)
        };
        let (image_data, image_mime) = downloaded_image
            .as_ref()
            .map(|(data, mime)| (Some(data.as_slice()), Some(mime.as_str())))
            .unwrap_or((None, None));

        save_backfilled_metadata(
            pool,
            movie.id,
            image_url.as_deref(),
            image_data,
            image_mime,
            plot.as_deref(),
            !image_failed,
        )
        .await?;
        completed += 1;
    }

    let remaining = count_pending_metadata(pool).await? as usize;
    let now_complete = total_movies.saturating_sub(remaining);
    println!(
        "Pass complete: {completed} checked, {missing} without a Wikipedia summary, {image_failures} poster downloads failed. Checkpoint: {now_complete}/{total_movies} complete; {remaining} pending."
    );
    if remaining > 0 {
        println!("Run the same command again to resume pending records.");
    }
    Ok(())
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Not Found")
}

async fn htmx() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../static/htmx.min.js"),
    )
}

async fn scss() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        r#"
        :root {
            --bg: #111615;
            --bg-elevated: rgba(27, 34, 32, 0.9);
            --panel: #1b2321;
            --panel-soft: #26302d;
            --panel-strong: #151b19;
            --border: rgba(180, 198, 190, 0.16);
            --text: #edf4f0;
            --text-muted: #a2b0a9;
            --text-soft: #c6e6da;
            --accent: #81d9bf;
            --accent-strong: #4eb89c;
            --accent-glow: rgba(91, 196, 163, 0.2);
            --success: #a7d984;
            --danger: #ee8b82;
            --shadow: 0 18px 38px rgba(4, 9, 7, 0.28);
        }

        * { box-sizing: border-box; }

        html { color-scheme: dark; }

        body {
            margin: 0;
            min-height: 100vh;
            font-family: Inter, "Segoe UI", sans-serif;
            background:
                radial-gradient(circle at 18% 0%, rgba(104, 190, 161, 0.12), transparent 32%),
                linear-gradient(180deg, #111615 0%, #151b19 100%);
            color: var(--text);
            line-height: 1.5;
        }

        .container {
            max-width: 1040px;
            margin: 0 auto;
            padding: 32px 20px 64px;
        }

        #list {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(230px, 1fr));
            gap: 12px;
        }

        #list > .results-summary,
        #list > .empty-state {
            grid-column: 1 / -1;
        }

        .app-header {
            position: sticky;
            top: 0;
            z-index: 10;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 18px;
            padding: 18px 20px;
            margin: 0 -20px 28px;
            background: linear-gradient(180deg, rgba(17, 22, 21, 0.94), rgba(20, 27, 25, 0.84));
            backdrop-filter: blur(12px);
            -webkit-backdrop-filter: blur(12px);
            border: 1px solid rgba(148, 163, 184, 0.14);
            border-radius: 18px;
            box-shadow: 0 10px 24px rgba(4, 9, 7, 0.2), inset 0 1px 0 rgba(200, 220, 210, 0.06);
        }

        .brand {
            display: flex;
            align-items: center;
            gap: 14px;
        }

        .brand-mark {
            display: inline-flex;
            align-items: center;
            justify-content: center;
            width: 42px;
            height: 42px;
            border-radius: 12px;
            background: linear-gradient(135deg, rgba(129, 217, 191, 0.18), rgba(78, 184, 156, 0.3));
            border: 1px solid rgba(129, 217, 191, 0.24);
            font-size: 1.4rem;
            box-shadow: 0 8px 18px rgba(30, 124, 98, 0.16);
        }

        .eyebrow {
            margin: 0 0 2px;
            font-size: 0.68rem;
            letter-spacing: 0.12em;
            text-transform: uppercase;
            color: var(--text-muted);
        }

        h1 {
            margin: 0;
            font-size: 1.3rem;
            letter-spacing: 0;
            font-weight: 750;
            color: #f8fbff;
        }

        .header-meta {
            display: inline-flex;
            align-items: center;
            gap: 10px;
            padding: 8px 12px;
            border-radius: 999px;
            background: rgba(17, 24, 39, 0.9);
            border: 1px solid var(--border);
            color: var(--text-muted);
            font-size: 0.76rem;
            font-weight: 600;
        }

        a {
            color: #dfe9ff;
        }

        a:hover,
        a:focus-visible {
            color: #ffffff;
        }

        button:focus-visible,
        input:focus-visible,
        a:focus-visible {
            outline: 3px solid rgba(129, 217, 191, 0.72);
            outline-offset: 2px;
        }

        .hero {
            margin: 4px 0 22px;
            padding: 2px 4px 0;
        }

        .hero-copy {
            margin: 0;
            font-size: 2.65rem;
            line-height: 1.08;
            letter-spacing: 0;
            font-weight: 800;
            color: #f8fbff;
        }

        .hero-subtitle {
            margin: 8px 0 0;
            max-width: 540px;
            font-size: 0.96rem;
            line-height: 1.55;
            color: var(--text-muted);
        }

        .stats {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
            gap: 12px;
            margin: 20px 0 18px;
            padding: 2px 0;
        }

        .stats > div {
            position: relative;
            overflow: hidden;
            background: linear-gradient(145deg, rgba(31, 40, 37, 0.96), rgba(24, 31, 29, 0.94));
            border: 1px solid rgba(180, 198, 190, 0.14);
            border-radius: 14px;
            box-shadow: 0 8px 20px rgba(4, 9, 7, 0.18), inset 0 1px 0 rgba(220, 240, 230, 0.04);
            padding: 16px 18px 14px;
            min-height: 100px;
        }

        .stats > div::before {
            content: "";
            position: absolute;
            inset: 0 auto auto 0;
            width: 100%;
            height: 3px;
            background: linear-gradient(90deg, rgba(129, 217, 191, 0.9), rgba(225, 196, 130, 0.82));
        }

        .stats .title {
            display: block;
            font-size: 0.72rem;
            text-transform: uppercase;
            letter-spacing: 0.12em;
            color: var(--text-muted);
            margin-bottom: 14px;
            font-weight: 700;
        }

        .stats .value {
            display: block;
            font-size: 2.4rem;
            font-weight: 800;
            letter-spacing: 0;
            color: var(--text);
            line-height: 1;
        }

        .stats .watched .value {
            color: var(--success);
        }

        .actions {
            display: flex;
            flex-wrap: wrap;
            align-items: center;
            justify-content: space-between;
            gap: 14px;
            margin-bottom: 20px;
            padding: 14px 16px;
            border: 1px solid rgba(180, 198, 190, 0.14);
            border-radius: 14px;
            background: linear-gradient(180deg, rgba(27, 34, 32, 0.92), rgba(24, 31, 29, 0.88));
            box-shadow: 0 8px 18px rgba(4, 9, 7, 0.16), inset 0 1px 0 rgba(200, 220, 210, 0.04);
        }

        .browse-toolbar {
            display: flex;
            align-items: flex-end;
            justify-content: space-between;
            gap: 18px;
            margin: 0 0 14px;
        }

        .filter-toolbar {
            display: flex;
            align-items: center;
            gap: 14px;
        }

        .filter-label {
            color: var(--text-muted);
            font-size: 0.76rem;
            font-weight: 700;
        }

        .sort-control {
            display: inline-flex;
            align-items: center;
            gap: 9px;
        }

        .sort-select {
            min-height: 42px;
            padding: 8px 34px 8px 12px;
            border: 1px solid rgba(148, 163, 184, 0.2);
            border-radius: 10px;
            background: rgba(27, 34, 32, 0.9);
            color: var(--text);
            font: inherit;
            font-size: 0.8rem;
            font-weight: 600;
            cursor: pointer;
        }

        .sort-select:hover {
            border-color: rgba(129, 217, 191, 0.46);
        }

        .filter-options {
            display: inline-flex;
            gap: 4px;
            padding: 4px;
            border: 1px solid rgba(148, 163, 184, 0.16);
            border-radius: 12px;
            background: rgba(27, 34, 32, 0.82);
        }

        .filter-button {
            min-height: 36px;
            padding: 7px 13px;
            border: 1px solid transparent;
            border-radius: 8px;
            background: transparent;
            color: var(--text-muted);
            box-shadow: none;
            font-size: 0.8rem;
            font-weight: 700;
            transition: color 0.15s ease, background 0.15s ease, border-color 0.15s ease;
        }

        .filter-button:hover {
            transform: none;
            filter: none;
            background: rgba(148, 163, 184, 0.1);
            color: var(--text);
            box-shadow: none;
        }

        .filter-button.active,
        .filter-button[aria-pressed="true"] {
            border-color: rgba(129, 217, 191, 0.34);
            background: rgba(95, 180, 150, 0.16);
            color: #e4f5ed;
            box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.06);
        }

        .archive-controls {
            display: flex;
            align-items: center;
            gap: 12px;
            flex-wrap: wrap;
        }

        .archive-controls .sort-control {
            flex: 1;
            justify-content: space-between;
        }

        form {
            display: inline-flex;
            align-items: center;
        }

        .search-wrap {
            display: flex;
            align-items: center;
            gap: 12px;
            flex: 1 1 300px;
            min-width: 220px;
            padding: 8px 12px;
            border-radius: 14px;
            border: 1px solid rgba(148, 163, 184, 0.18);
            background: linear-gradient(180deg, rgba(31, 39, 36, 0.94), rgba(25, 32, 30, 0.9));
            box-shadow: inset 0 1px 0 rgba(200, 220, 210, 0.04);
        }

        button {
            appearance: none;
            border: 1px solid transparent;
            background: linear-gradient(135deg, var(--accent) 0%, var(--accent-strong) 100%);
            color: #f8fbff;
            padding: 11px 16px;
            border-radius: 12px;
            font-weight: 700;
            letter-spacing: 0.01em;
            cursor: pointer;
            box-shadow: 0 8px 18px var(--accent-glow);
            transition: transform 0.18s ease, box-shadow 0.18s ease, filter 0.18s ease;
        }

        button:hover {
            transform: translateY(-1px);
            filter: brightness(1.04);
            box-shadow: 0 10px 20px rgba(45, 145, 115, 0.26);
        }

        button:active {
            transform: translateY(0);
            filter: brightness(0.96);
            box-shadow: 0 5px 14px rgba(45, 145, 115, 0.2);
        }

        input[type=text] {
            flex: 1;
            min-width: 220px;
            background: rgba(29, 36, 33, 0.94);
            border: 1px solid rgba(148, 163, 184, 0.25);
            border-radius: 12px;
            padding: 12px 14px;
            color: var(--text);
            font-size: 0.98rem;
            box-shadow: inset 0 1px 2px rgba(4, 9, 7, 0.18);
            transition: border-color 0.15s ease, box-shadow 0.15s ease, transform 0.15s ease;
        }

        input[type=text]:hover {
            border-color: rgba(148, 163, 184, 0.45);
        }

        input[type=text]:focus {
            transform: translateY(-1px);
        }

        input[type=text]::placeholder {
            color: #aab4c8;
        }

        input[type=text]:focus {
            outline: none;
            border-color: rgba(129, 217, 191, 0.88);
            box-shadow: 0 0 0 3px rgba(129, 217, 191, 0.16);
        }

        #s, #loading {
            opacity: 0.7;
            transition: opacity 0.2s ease;
        }

        .htmx-indicator {
            display: none;
        }

        .htmx-request.htmx-indicator {
            display: inline-flex;
            align-items: center;
        }

        .async-indicator {
            color: var(--text-muted);
            font-size: 0.74rem;
            white-space: nowrap;
        }

        .search-error {
            color: #ffaaaa;
            font-size: 0.74rem;
            white-space: nowrap;
        }

        .retry-button {
            padding: 2px 4px;
            border: 0;
            background: transparent;
            color: #dfe9ff;
            box-shadow: none;
            font-size: inherit;
            text-decoration: underline;
        }

        .retry-button:hover,
        .retry-button:active {
            transform: none;
            background: transparent;
            box-shadow: none;
        }

        .seed-form {
            align-items: flex-start;
            flex-direction: column;
            gap: 5px;
        }

        .seed-button {
            min-width: 168px;
            opacity: 1;
        }

        #loading.seed-button {
            opacity: 1;
        }

        .seed-detail,
        .seed-status {
            color: var(--text-muted);
            font-size: 0.74rem;
        }

        .seed-status {
            min-height: 1.2em;
        }

        .seed-form.htmx-request .seed-button {
            cursor: progress;
        }

        .seed-feedback.is-success {
            color: #8ef0c0;
        }

        .seed-feedback.is-error {
            color: #ffaaaa;
        }

        .details-button {
            align-self: flex-start;
            padding: 4px 0;
            border: 0;
            border-radius: 0;
            background: transparent;
            color: var(--text-soft);
            box-shadow: none;
            font-size: 0.78rem;
        }

        .details-button:hover,
        .details-button:active {
            transform: none;
            background: transparent;
            box-shadow: none;
            text-decoration: underline;
        }

        .details-button.htmx-request {
            opacity: 0.65;
            cursor: progress;
        }

        .movie-details:empty {
            display: none;
        }

        .movie-details:not(:empty) {
            display: flex;
            align-items: flex-start;
            gap: 14px;
            padding-top: 14px;
            border-top: 1px solid var(--border);
        }

        .metadata-poster {
            width: 76px;
            aspect-ratio: 2 / 3;
            flex: 0 0 auto;
            object-fit: cover;
            border-radius: 5px;
            background: var(--panel-soft);
        }

        .metadata-copy {
            min-width: 0;
            color: var(--text-muted);
            font-size: 0.8rem;
        }

        .metadata-copy p {
            margin: 0 0 8px;
        }

        .metadata-links {
            display: flex;
            gap: 12px;
            font-size: 0.75rem;
        }

        .metadata-empty {
            color: var(--text-muted);
            font-size: 0.78rem;
        }

        .movie-dialog {
            width: min(92vw, 760px);
            max-width: none;
            max-height: min(88vh, 760px);
            margin: auto;
            padding: 0;
            overflow: auto;
            border: 1px solid var(--border);
            border-radius: 16px;
            background: var(--panel);
            color: var(--text);
            box-shadow: var(--shadow);
        }

        .movie-dialog::backdrop {
            background: rgba(5, 9, 8, 0.76);
            backdrop-filter: blur(3px);
        }

        .movie-dialog-header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 16px;
            padding: 18px 22px;
            border-bottom: 1px solid var(--border);
        }

        .movie-dialog-header h2 {
            margin: 0;
            font-size: 1.05rem;
        }

        .dialog-close {
            width: 38px;
            height: 38px;
            padding: 0;
            border: 1px solid var(--border);
            border-radius: 50%;
            background: var(--panel-soft);
            color: var(--text);
            box-shadow: none;
            font-size: 1.35rem;
            line-height: 1;
        }

        .movie-dialog-content {
            padding: 22px;
        }

        .metadata-card {
            display: grid;
            grid-template-columns: minmax(130px, 190px) minmax(0, 1fr);
            align-items: start;
            gap: 24px;
        }

        .metadata-poster,
        .metadata-poster-placeholder {
            width: 100%;
            aspect-ratio: 2 / 3;
            object-fit: cover;
            border-radius: 8px;
            background: var(--panel-soft);
        }

        .metadata-poster-placeholder {
            display: grid;
            place-items: center;
            color: var(--text-muted);
            font-size: 0.85rem;
        }

        .metadata-heading {
            margin: 0 0 14px;
            color: var(--text);
            font-size: 1.35rem;
            line-height: 1.25;
        }

        .metadata-copy {
            min-width: 0;
            color: var(--text-muted);
            font-size: 0.9rem;
            line-height: 1.65;
        }

        .metadata-copy p {
            margin: 0 0 16px;
        }

        .metadata-links {
            display: flex;
            gap: 14px;
            font-size: 0.8rem;
        }

        .metadata-loading,
        .metadata-empty {
            margin: 0;
            color: var(--text-muted);
            font-size: 0.9rem;
        }

        @media (max-width: 640px) {
            .archive-controls {
                width: 100%;
            }

            .movie-dialog {
                width: calc(100vw - 24px);
                max-height: 90vh;
                border-radius: 12px;
            }

            .movie-dialog-header,
            .movie-dialog-content {
                padding: 16px;
            }

            .metadata-card {
                grid-template-columns: minmax(96px, 132px) minmax(0, 1fr);
                gap: 16px;
            }

            .metadata-heading {
                font-size: 1.1rem;
            }

            .metadata-copy {
                font-size: 0.82rem;
            }
        }

        @media (max-width: 380px) {
            .metadata-card {
                grid-template-columns: 1fr;
            }

            .metadata-poster,
            .metadata-poster-placeholder {
                width: 132px;
            }
        }

        .movie-item {
            display: flex;
            align-items: stretch;
            flex-direction: column;
            justify-content: space-between;
            gap: 18px;
            min-height: 176px;
            padding: 18px;
            margin-bottom: 0;
            border: 1px solid rgba(180, 198, 190, 0.12);
            border-radius: 12px;
            background: linear-gradient(180deg, rgba(28, 36, 33, 0.94), rgba(25, 32, 30, 0.92));
            box-shadow: 0 5px 14px rgba(4, 9, 7, 0.14), inset 0 1px 0 rgba(200, 220, 210, 0.035);
        }

        .movie-item .info {
            flex: 1;
            display: flex;
            flex-direction: column;
            align-items: flex-start;
            gap: 12px;
            min-width: 0;
        }

        .results-summary {
            grid-column: 1 / -1;
            display: flex;
            align-items: baseline;
            gap: 6px;
            margin: 0 2px 12px;
            color: var(--text-muted);
            font-size: 0.8rem;
        }

        .pagination {
            grid-column: 1 / -1;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 16px;
            margin: 6px 0 18px;
            padding: 12px 2px;
            border-top: 1px solid var(--border);
        }

        .page-position {
            color: var(--text-muted);
            font-size: 0.78rem;
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
        }

        .page-links {
            display: flex;
            align-items: center;
            gap: 4px;
        }

        .page-button {
            display: inline-grid;
            place-items: center;
            min-width: 36px;
            height: 36px;
            padding: 0 8px;
            border: 1px solid transparent;
            border-radius: 8px;
            background: transparent;
            color: var(--text-muted);
            box-shadow: none;
            font-size: 0.8rem;
            font-variant-numeric: tabular-nums;
        }

        .page-button:hover {
            transform: none;
            border-color: var(--border);
            background: var(--panel-soft);
            color: var(--text);
            box-shadow: none;
        }

        .page-button[aria-current='page'] {
            border-color: rgba(129, 217, 191, 0.32);
            background: rgba(95, 180, 150, 0.16);
            color: var(--text-soft);
            cursor: default;
            box-shadow: none;
        }

        .page-ellipsis {
            width: 18px;
            color: var(--text-muted);
            text-align: center;
        }

        .results-count {
            color: var(--text);
            font-size: 0.9rem;
            font-weight: 750;
            font-variant-numeric: tabular-nums;
        }

        .status-badge {
            align-self: flex-start;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            padding: 5px 9px;
            border-radius: 999px;
            font-size: 0.68rem;
            font-weight: 800;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            border: 1px solid transparent;
        }

        .status-badge.watched {
            background: rgba(52, 211, 153, 0.12);
            color: #8ef0c0;
            border-color: rgba(52, 211, 153, 0.28);
        }

        .status-badge.unwatched {
            background: rgba(148, 163, 184, 0.08);
            color: #d7def0;
            border-color: rgba(148, 163, 184, 0.2);
        }

        .title {
            display: inline-block;
            max-width: 100%;
            font-weight: 700;
            font-size: 1.05rem;
            color: var(--text);
            letter-spacing: 0;
            word-break: break-word;
        }

        .title.completed {
            text-decoration: line-through;
            color: var(--text-muted);
        }

        .year {
            display: inline-flex;
            align-items: center;
            justify-content: center;
            min-width: 52px;
            padding: 4px 8px;
            border-radius: 999px;
            background: rgba(95, 180, 150, 0.12);
            border: 1px solid rgba(129, 217, 191, 0.2);
            color: var(--text-soft);
            font-size: 0.75rem;
            font-weight: 700;
        }

        .movie-meta {
            display: inline-flex;
            align-items: center;
            gap: 10px;
            flex: 0 0 auto;
        }

        .wiki {
            display: inline-flex;
            align-items: center;
            gap: 5px;
            color: var(--text-muted);
            text-decoration: none;
            font-size: 0.75rem;
            font-weight: 600;
            padding: 4px 0 4px 10px;
            border-left: 1px solid rgba(148, 163, 184, 0.22);
            transition: color 0.15s ease;
        }

        .wiki::after {
            content: "↗";
            font-size: 0.85em;
        }

        .wiki:hover {
            color: #d9e4ff;
        }

        .wiki:active {
            color: #ffffff;
        }

        label {
            display: inline-flex;
            align-items: center;
            gap: 8px;
            min-width: 92px;
            padding: 7px 12px;
            border-radius: 999px;
            color: var(--text-muted);
            font-size: 0.8rem;
            font-weight: 700;
            letter-spacing: 0.02em;
            cursor: pointer;
            background: rgba(24, 31, 29, 0.92);
            border: 1px solid rgba(180, 198, 190, 0.14);
            transition: background 0.15s ease, border-color 0.15s ease, transform 0.15s ease;
        }

        label:hover {
            border-color: rgba(52, 211, 153, 0.42);
            transform: translateY(-1px);
        }

        input[type=checkbox] {
            appearance: none;
            position: relative;
            width: 36px;
            height: 20px;
            border-radius: 999px;
            background: rgba(148, 163, 184, 0.28);
            border: 1px solid rgba(148, 163, 184, 0.2);
            cursor: pointer;
            transition: all 0.2s ease;
        }

        input[type=checkbox]::before {
            content: "";
            position: absolute;
            top: 1px;
            left: 2px;
            width: 14px;
            height: 14px;
            border-radius: 50%;
            background: white;
            box-shadow: 0 2px 4px rgba(4, 9, 7, 0.24);
            transition: transform 0.2s ease;
        }

        input[type=checkbox]:checked {
            background: rgba(52, 211, 153, 0.5);
            border-color: rgba(52, 211, 153, 0.7);
        }

        input[type=checkbox]:checked::before {
            transform: translateX(16px);
        }

        .empty-state {
            display: flex;
            flex-direction: column;
            align-items: center;
            justify-content: center;
            gap: 10px;
            text-align: center;
            padding: 48px 24px;
            border: 1px solid rgba(148, 163, 184, 0.15);
            border-radius: 20px;
            background: linear-gradient(180deg, rgba(28, 36, 33, 0.9), rgba(24, 31, 29, 0.88));
            color: var(--text-muted);
            box-shadow: 0 12px 26px rgba(4, 9, 7, 0.2), inset 0 1px 0 rgba(200, 220, 210, 0.06);
        }

        .empty-state .icon {
            font-size: 2rem;
            line-height: 1;
            opacity: 0.9;
        }

        .empty-state h3 {
            margin: 0;
            color: var(--text);
            font-size: 1.2rem;
            letter-spacing: 0;
        }

        .empty-state p {
            margin: 0;
            font-size: 1rem;
            max-width: 420px;
        }

        @media (max-width: 640px) {
            .container {
                padding: 18px 14px 40px;
            }

            .app-header {
                position: relative;
                top: auto;
                gap: 12px;
                padding: 14px;
                margin: 0 -14px 22px;
                border-radius: 0 0 16px 16px;
            }

            .brand {
                gap: 10px;
            }

            .brand-mark {
                width: 38px;
                height: 38px;
                font-size: 1.2rem;
            }

            h1 {
                font-size: 1.45rem;
            }

            .header-meta {
                padding: 7px 9px;
                font-size: 0.68rem;
                white-space: nowrap;
            }

            .hero {
                margin: 0 0 22px;
                padding: 4px 0 0;
            }

            .hero-copy {
                font-size: 2rem;
                line-height: 1.1;
            }

            .hero-subtitle {
                margin-top: 10px;
                font-size: 0.96rem;
            }

            .stats {
                grid-template-columns: repeat(2, minmax(0, 1fr));
                gap: 10px;
                margin: 20px 0;
            }

            .stats > div {
                min-height: 96px;
                padding: 15px;
                border-radius: 15px;
            }

            .stats .title {
                margin-bottom: 10px;
                font-size: 0.64rem;
            }

            .stats .value {
                font-size: 2rem;
            }

            .actions {
                align-items: stretch;
                gap: 10px;
                padding: 12px;
                margin-bottom: 16px;
                border-radius: 16px;
            }

            .actions form,
            .search-wrap {
                width: 100%;
                min-width: 0;
            }

            .actions button {
                width: 100%;
                min-height: 44px;
            }

            .browse-toolbar {
                align-items: stretch;
                flex-direction: column;
                gap: 12px;
                margin-bottom: 12px;
            }

            .pagination {
                align-items: flex-start;
                flex-direction: column;
                gap: 8px;
            }

            .page-links {
                width: 100%;
                justify-content: space-between;
                gap: 1px;
            }

            .page-button {
                min-width: 32px;
                height: 38px;
                padding: 0 5px;
            }

            .filter-toolbar {
                align-items: flex-start;
                flex-direction: column;
                gap: 8px;
            }

            .filter-options {
                width: 100%;
            }

            .filter-button {
                flex: 1;
                min-height: 42px;
                padding: 7px 8px;
            }

            .sort-control {
                justify-content: space-between;
            }

            .sort-select {
                flex: 1;
                max-width: 72%;
            }

            .search-wrap {
                min-height: 48px;
            }

            input[type=text] {
                min-width: 0;
                min-height: 44px;
                padding: 10px 8px;
                font-size: 16px;
            }

            .movie-item {
                align-items: stretch;
                flex-direction: column;
                gap: 12px;
                padding: 14px;
                margin-bottom: 10px;
                border-radius: 14px;
            }

            .movie-item .info {
                display: flex;
                flex-direction: column;
                align-items: flex-start;
                gap: 10px;
            }

            .status-badge {
                justify-self: start;
            }

            .title {
                min-width: 0;
                align-self: flex-start;
            }

            .movie-meta {
                grid-column: auto;
            }

            .movie-item label {
                align-self: flex-start;
                min-height: 44px;
            }

            .empty-state {
                padding: 36px 18px;
            }
        }

        @media (max-width: 380px) {
            .app-header {
                align-items: flex-start;
                flex-direction: column;
            }

            .header-meta {
                margin-left: 48px;
            }

            .stats > div {
                padding: 13px 12px;
            }

            .stats .value {
                font-size: 1.75rem;
            }

            .hero-copy {
                font-size: 1.75rem;
            }
        }
    "#,
    )
}

fn normalized_title(title: &str) -> String {
    title
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn safe_https_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    (url.scheme() == "https").then(|| url.to_string())
}

async fn imdb_match(title: &str, year: u16) -> (Option<String>, Option<String>) {
    let Ok(mut url) = url::Url::parse("https://v2.sg.media-imdb.com/suggestion/x/") else {
        return (None, None);
    };
    let query = format!("{title} {year}.json");
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.push(&query);
    } else {
        return (None, None);
    }

    let response = reqwest::Client::builder()
        .user_agent("MovieTracker/1.0 (movie metadata lookup)")
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let Ok(client) = response else {
        return (None, None);
    };
    let result = client.get(url).send().await;
    let Ok(response) = result else {
        return (None, None);
    };
    let response = response.error_for_status();
    let Ok(response) = response else {
        return (None, None);
    };
    let result = response.json::<ImdbSuggestionResponse>().await;
    let Ok(result) = result else {
        return (None, None);
    };

    let target = normalized_title(title);
    let suggestion = result
        .d
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            item.id.starts_with("tt")
                && normalized_title(&item.title) == target
                && item
                    .year
                    .is_some_and(|item_year| item_year.abs_diff(year) <= 1)
        })
        .min_by_key(|item| item.year.unwrap_or(year).abs_diff(year));

    suggestion.map_or((None, None), |item| {
        (
            Some(item.id),
            item.image.and_then(|image| safe_https_url(&image.url)),
        )
    })
}

fn wikipedia_summary_url(wiki_url: &str) -> Option<url::Url> {
    metadata::wikipedia_summary_url(wiki_url)
}

async fn wikipedia_summary(wiki_url: &str) -> Option<WikipediaSummary> {
    let url = wikipedia_summary_url(wiki_url)?;
    reqwest::Client::builder()
        .user_agent("MovieTracker/1.0 (movie metadata lookup)")
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<WikipediaSummary>()
        .await
        .ok()
}

async fn download_poster(url: &str) -> Option<(Vec<u8>, String)> {
    let url = safe_https_url(url)?;
    let response = reqwest::Client::builder()
        .user_agent("MovieTracker/1.0 (movie metadata lookup)")
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;

    if response
        .content_length()
        .is_some_and(|length| length > 5_000_000)
    {
        return None;
    }

    let mime = response
        .headers()
        .get("content-type")?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/jpeg" | "image/png" | "image/webp" | "image/gif" | "image/avif"
    ) {
        return None;
    }

    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > 5_000_000 {
        return None;
    }
    Some((bytes.to_vec(), mime))
}

async fn movie_poster(Path(id): Path<i64>, State(state): State<AppState>) -> Response {
    let Ok(Some((bytes, mime))) = get_movie_poster(&state.db, id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(Body::from(bytes));
    if let Ok(value) = HeaderValue::from_str(&mime) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

fn render_movie_details(movie: &MovieMetadata) -> String {
    let poster = if movie.poster_data.is_some() {
        format!(
            "<img class='metadata-poster' src='/poster/{}' alt='Poster for {}' loading='lazy'>",
            movie.id,
            escape_html(&movie.title)
        )
    } else {
        "<div class='metadata-poster-placeholder'>No poster</div>".to_string()
    };
    let overview = movie
        .plot
        .as_deref()
        .map(|plot| {
            let excerpt = plot.chars().take(900).collect::<String>();
            format!("<p>{}</p>", escape_html(&excerpt))
        })
        .unwrap_or_default();
    let imdb_link = movie
        .imdb_id
        .as_deref()
        .filter(|id| id.starts_with("tt"))
        .map(|id| {
            format!(
                "<a href='https://www.imdb.com/title/{}' target='_blank' rel='noopener noreferrer'>IMDb</a>",
                escape_html(id)
            )
        })
        .unwrap_or_default();
    let wiki_link = safe_https_url(&movie.wiki_url)
        .map(|url| {
            format!(
                "<a href='{}' target='_blank' rel='noopener noreferrer'>Wikipedia</a>",
                escape_html(&url)
            )
        })
        .unwrap_or_default();

    format!(
        "<article class='metadata-card'>{poster}<div class='metadata-copy'><h3 class='metadata-heading'>{} ({})</h3>{}<div class='metadata-links'>{imdb_link}{wiki_link}</div></div></article>",
        escape_html(&movie.title),
        movie.year,
        if overview.is_empty() {
            "<p class='metadata-empty'>No overview available.</p>".to_string()
        } else {
            overview
        }
    )
}

async fn movie_details(Path(id): Path<i64>, State(state): State<AppState>) -> impl IntoResponse {
    let mut movie = match get_movie_metadata(&state.db, id).await {
        Ok(movie) => movie,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<p class='metadata-empty'>Movie not found.</p>".to_string()),
            )
        }
    };

    let metadata_needs_refresh = !movie.metadata_fetch_complete
        || (movie.poster_url.is_some() && movie.poster_data.is_none());
    if metadata_needs_refresh {
        let imdb_lookup = async {
            if movie.imdb_id.is_none() || movie.poster_url.is_none() {
                imdb_match(&movie.title, movie.year).await
            } else {
                (movie.imdb_id.clone(), movie.poster_url.clone())
            }
        };
        let wikipedia_lookup = async {
            if movie.plot.is_none() || movie.poster_url.is_none() {
                wikipedia_summary(&movie.wiki_url).await
            } else {
                None
            }
        };
        let (imdb, wikipedia) = tokio::join!(imdb_lookup, wikipedia_lookup);
        let (imdb_id, imdb_poster) = imdb;
        let wiki_poster = wikipedia
            .as_ref()
            .and_then(|summary| summary.thumbnail.as_ref())
            .and_then(|thumbnail| safe_https_url(&thumbnail.source));
        let poster_url = imdb_poster.or(wiki_poster).or(movie.poster_url.clone());
        let downloaded_poster = match poster_url.as_deref() {
            Some(url) if movie.poster_data.is_none() => download_poster(url).await,
            _ => None,
        };
        let poster_data = downloaded_poster
            .as_ref()
            .map(|(bytes, _)| bytes.as_slice());
        let poster_mime = downloaded_poster.as_ref().map(|(_, mime)| mime.as_str());
        let plot = wikipedia
            .and_then(|summary| summary.extract)
            .filter(|extract| !extract.trim().is_empty())
            .or(movie.plot.clone());

        if imdb_id.is_some() || poster_url.is_some() || poster_data.is_some() || plot.is_some() {
            if save_movie_metadata(
                &state.db,
                id,
                imdb_id.as_deref(),
                poster_url.as_deref(),
                poster_data,
                poster_mime,
                plot.as_deref(),
            )
            .await
            .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html("<p class='metadata-empty'>Could not save movie details.</p>".to_string()),
                );
            }
        }

        movie = match get_movie_metadata(&state.db, id).await {
            Ok(movie) => movie,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html("<p class='metadata-empty'>Could not load movie details.</p>".to_string()),
                )
            }
        };
    }

    (StatusCode::OK, Html(render_movie_details(&movie)))
}

async fn stats(State(s): State<AppState>) -> Json<ApiResponse<(u64, u64)>> {
    Json(ApiResponse {
        success: true,
        data: Some(get_stats(&s.db).await.unwrap_or((0, 0))),
    })
}

async fn index(State(s): State<AppState>) -> impl IntoResponse {
    let (t, w) = get_stats(&s.db).await.unwrap_or((0, 0));
    let m = search_movies_page(&s.db, "", "all", "title", None, 1, PAGE_SIZE)
        .await
        .unwrap_or_else(|_| empty_movie_page());
    let year_options = (1990..=2025)
        .rev()
        .map(|year| format!("<option value='{year}'>{year}</option>"))
        .collect::<String>();
    let response = Html(format!(r##"
        <!DOCTYPE html>
        <html><head><meta charset=utf-8><meta name=viewport content="width=device-width, initial-scale=1"><title>Movie Tracker</title>
        <link rel=stylesheet href=/style.css>
        <script src=/htmx.min.js></script>
        </head><body>
        <div class=container>
          <header class=app-header>
            <div class=brand>
              <span class=brand-mark>🎬</span>
              <div>
                <p class=eyebrow>Catalog</p>
                <h1>Movie Tracker</h1>
              </div>
            </div>
            <div class=header-meta>36-year archive</div>
          </header>

          <section class=hero>
            <p class=eyebrow>Collection</p>
            <h2 class=hero-copy>Your movie archive.</h2>
            <p class=hero-subtitle>A personal record of cinema from 1990–2025.</p>
          </section>

          <div class=stats id=stats>
            <div>
              <span class=title>Total Movies</span>
              <span class=value>{0}</span>
            </div>
            <div class=watched>
              <span class=title>Watched</span>
              <span class=value>{1}</span>
            </div>
          </div>

          <div class=actions>
                        <form class=seed-form hx-post=/seed hx-target=#seed-status hx-swap=innerHTML hx-indicator=#seed-loading>
                            <button id=loading class=seed-button type=submit>Import catalog</button>
                            <span class=seed-detail>Movies from 1990–2025</span>
                            <span id=seed-loading class="htmx-indicator async-indicator" role=status>Importing catalog…</span>
                            <span id=seed-status class=seed-status role=status aria-live=polite></span>
            </form>
            <div class=search-wrap>
                            <input id=search-query type=text placeholder="Search movies..." name=q hx-get=/search hx-trigger="keyup changed delay:300ms, search" hx-target=#list hx-indicator=#s hx-include="#active-filter,#sort-order,#year-filter">
                            <span id=s class="htmx-indicator async-indicator" role=status>Searching…</span>
                            <span id=search-error class=search-error hidden>Search failed. <button type=button class=retry-button onclick="htmx.trigger(document.getElementById('search-query'), 'search')">Retry</button></span>
            </div>
          </div>
                    <div class=browse-toolbar>
                        <div class=filter-toolbar role=group aria-label="Filter movies">
                            <span class=filter-label>Show</span>
                            <div class=filter-options>
                                <input id=active-filter type=hidden name=filter value=all>
                                <button type=button class="filter-button active" aria-pressed=true data-filter=all hx-get="/filter?filter=all" hx-include="#search-query,#sort-order,#year-filter" hx-target=#list hx-indicator=#s onclick="document.getElementById('active-filter').value=this.dataset.filter; document.querySelectorAll('.filter-button').forEach(button => button.classList.remove('active')); this.classList.add('active'); document.querySelectorAll('.filter-button').forEach(button => button.setAttribute('aria-pressed', false)); this.setAttribute('aria-pressed', true);">All</button>
                                <button type=button class=filter-button aria-pressed=false data-filter=unwatched hx-get="/filter?filter=unwatched" hx-include="#search-query,#sort-order,#year-filter" hx-target=#list hx-indicator=#s onclick="document.getElementById('active-filter').value=this.dataset.filter; document.querySelectorAll('.filter-button').forEach(button => button.classList.remove('active')); this.classList.add('active'); document.querySelectorAll('.filter-button').forEach(button => button.setAttribute('aria-pressed', false)); this.setAttribute('aria-pressed', true);">To Watch</button>
                                <button type=button class=filter-button aria-pressed=false data-filter=watched hx-get="/filter?filter=watched" hx-include="#search-query,#sort-order,#year-filter" hx-target=#list hx-indicator=#s onclick="document.getElementById('active-filter').value=this.dataset.filter; document.querySelectorAll('.filter-button').forEach(button => button.classList.remove('active')); this.classList.add('active'); document.querySelectorAll('.filter-button').forEach(button => button.setAttribute('aria-pressed', false)); this.setAttribute('aria-pressed', true);">Watched</button>
                            </div>
                        </div>
                        <div class=archive-controls>
                            <label class=sort-control for=year-filter>
                                <span class=filter-label>Year</span>
                                <select id=year-filter class=sort-select name=year hx-get=/filter hx-trigger=change hx-include="#search-query,#active-filter,#sort-order" hx-target=#list hx-indicator=#s>
                                    <option value="">All years</option>{3}
                                </select>
                            </label>
                            <label class=sort-control for=sort-order>
                                <span class=filter-label>Sort by</span>
                                <select id=sort-order class=sort-select name=sort hx-get=/filter hx-trigger=change hx-include="#search-query,#active-filter,#year-filter" hx-target=#list hx-indicator=#s>
                                    <option value=year_desc>Newest year</option>
                                    <option value=year_asc>Oldest year</option>
                                    <option value=title selected>Title A–Z</option>
                                    <option value=watched>Watched first</option>
                                </select>
                            </label>
                        </div>
                    </div>
          <div id=list>{2}</div>
        </div>
                <dialog id=movie-dialog class=movie-dialog aria-labelledby=movie-dialog-title>
                    <div class=movie-dialog-header>
                        <h2 id=movie-dialog-title>Movie details</h2>
                        <form method=dialog><button class=dialog-close type=submit aria-label="Close movie details">×</button></form>
                    </div>
                    <div id=movie-dialog-content class=movie-dialog-content aria-live=polite></div>
                </dialog>
                <script>
                    document.body.addEventListener('htmx:responseError', function(event) {{
                        const source = event.detail.elt;
                        if (source.closest('.seed-form')) {{
                            document.getElementById('seed-status').innerHTML = "Import failed. <button type='submit' class='retry-button'>Retry</button>";
                        }} else {{
                            document.getElementById('search-error').hidden = false;
                        }}
                    }});
                    document.body.addEventListener('htmx:sendError', function(event) {{
                        const source = event.detail.elt;
                        if (source.closest('.seed-form')) {{
                            document.getElementById('seed-status').innerHTML = "Import failed. <button type='submit' class='retry-button'>Retry</button>";
                        }} else {{
                            document.getElementById('search-error').hidden = false;
                        }}
                    }});
                    document.body.addEventListener('htmx:afterRequest', function(event) {{
                        if (event.detail.successful && !event.detail.elt.closest('.seed-form')) {{
                            document.getElementById('search-error').hidden = true;
                        }}
                    }});
                </script>
        </body></html>
        "##,
        t,w,for_render(m),year_options
    )).into_response();
    no_store(response)
}

async fn toggle(
    Path(id): Path<i64>,
    State(state): State<AppState>,
    Form(params): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    if toggle_watched(&state.db, id).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<p>Could not update watched status.</p>".to_string()),
        )
            .into_response();
    }

    let page = match search_movies_page(
        &state.db,
        params.get("q").map(String::as_str).unwrap_or_default(),
        params.get("filter").map(String::as_str).unwrap_or("all"),
        params.get("sort").map(String::as_str).unwrap_or("title"),
        params.get("year").and_then(|year| year.parse::<u16>().ok()),
        params
            .get("page")
            .and_then(|page| page.parse::<usize>().ok())
            .unwrap_or(1),
        PAGE_SIZE,
    )
    .await
    {
        Ok(page) => page,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<p>Could not refresh the movie page.</p>".to_string()),
            )
                .into_response()
        }
    };
    let (total, watched) = get_stats(&state.db).await.unwrap_or((0, 0));
    let response = format!(
        "{}<div id='stats' hx-swap-oob='innerHTML'><div><span class='title'>Total Movies</span><span class='value'>{total}</span></div><div class='watched'><span class='title'>Watched</span><span class='value'>{watched}</span></div></div>",
        for_render(page)
    );
    no_store(Html(response).into_response())
}

async fn search(
    Query(p): Query<HashMap<String, String>>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let movies = search_movies_page(
        &s.db,
        p.get("q").map(String::as_str).unwrap_or_default(),
        p.get("filter").map(String::as_str).unwrap_or("all"),
        p.get("sort").map(String::as_str).unwrap_or("title"),
        p.get("year").and_then(|year| year.parse::<u16>().ok()),
        p.get("page")
            .and_then(|page| page.parse::<usize>().ok())
            .unwrap_or(1),
        PAGE_SIZE,
    )
    .await
    .unwrap_or_else(|_| empty_movie_page());
    no_store(Html(for_render(movies)).into_response())
}

async fn filter(
    Query(p): Query<HashMap<String, String>>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let movies = search_movies_page(
        &s.db,
        p.get("q").map(String::as_str).unwrap_or_default(),
        p.get("filter").map(String::as_str).unwrap_or("all"),
        p.get("sort").map(String::as_str).unwrap_or("title"),
        p.get("year").and_then(|year| year.parse::<u16>().ok()),
        p.get("page")
            .and_then(|page| page.parse::<usize>().ok())
            .unwrap_or(1),
        PAGE_SIZE,
    )
    .await
    .unwrap_or_else(|_| empty_movie_page());
    no_store(Html(for_render(movies)).into_response())
}

async fn list(
    Query(p): Query<HashMap<String, String>>,
    State(s): State<AppState>,
) -> Json<ApiResponse<Vec<MovieResponse>>> {
    let movies = get_filtered_movies(
        &s.db,
        p.get("filter").map(String::as_str).unwrap_or("all"),
        p.get("sort").map(String::as_str).unwrap_or("year_desc"),
        p.get("year").and_then(|year| year.parse::<u16>().ok()),
    )
    .await
    .unwrap_or_default();
    Json(ApiResponse {
        success: true,
        data: Some(response_movies(movies)),
    })
}

async fn seed(State(s): State<AppState>) -> Html<String> {
    println!("Seeding...");
    match scraper::scrape_range(1990, 2025).await {
        Ok(movies) => match db::add_movies_batch(&s.db, &movies).await {
            Ok((_, added)) => {
                println!("Added {added} movies");
                let (total, watched) = get_stats(&s.db).await.unwrap_or((0, 0));
                let current_movies =
                    search_movies_page(&s.db, "", "all", "title", None, 1, PAGE_SIZE)
                        .await
                        .unwrap_or_else(|_| empty_movie_page());
                let status = if added == 0 {
                    "Catalog is up to date; no new titles were added.".to_string()
                } else {
                    format!("Added {added} new titles to your catalog.")
                };
                Html(format!(
                    "<div id='list' hx-swap-oob='innerHTML'>{}</div><div id='stats' hx-swap-oob='innerHTML'><div><span class='title'>Total Movies</span><span class='value'>{total}</span></div><div class='watched'><span class='title'>Watched</span><span class='value'>{watched}</span></div></div><span class='seed-feedback is-success'>{status}</span>",
                    for_render(current_movies)
                ))
            }
            Err(_) => Html(
                "<span class='seed-feedback is-error'>Movies were fetched but could not be saved. <button type='submit' class='retry-button'>Retry</button></span>"
                    .to_string(),
            ),
        }
        Err(_) => Html(
                    "<span class='seed-feedback is-error'>Could not load movie data. <button type='submit' class='retry-button'>Retry</button></span>"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::{wikipedia_summary_url, WikipediaSummary};

    #[test]
    fn wikipedia_summary_uses_article_slug() {
        let url = wikipedia_summary_url("https://en.wikipedia.org/wiki/Toy_Story").unwrap();
        assert_eq!(
            url.as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/Toy_Story"
        );
    }

    #[test]
    fn wikipedia_summary_parses_extract_and_thumbnail() {
        let summary: WikipediaSummary = serde_json::from_str(
            r#"{"extract":"A useful overview.","thumbnail":{"source":"https://upload.wikimedia.org/poster.jpg","width":250,"height":375}}"#,
        )
        .unwrap();
        assert_eq!(summary.extract.as_deref(), Some("A useful overview."));
        assert_eq!(
            summary
                .thumbnail
                .map(|thumbnail| thumbnail.source)
                .as_deref(),
            Some("https://upload.wikimedia.org/poster.jpg")
        );
    }
}
