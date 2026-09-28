use crate::scraper::Movie;
use sqlx::{FromRow, Result as SqlxResult, Row, SqlitePool};

pub async fn init_db(db_path: &str) -> SqlxResult<SqlitePool> {
    let pool = SqlitePool::connect(db_path).await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS movies (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, year INTEGER NOT NULL, is_watched INTEGER DEFAULT 0, wiki_url TEXT)"
    ).execute(&pool).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_movies_year ON movies(year)")
        .execute(&pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_movies_watched ON movies(is_watched)")
        .execute(&pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_movies_title ON movies(title)")
        .execute(&pool)
        .await?;

    let movie_columns = sqlx::query("PRAGMA table_info(movies)")
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect::<Vec<_>>();
    for (column, definition) in [
        ("imdb_id", "TEXT"),
        ("poster_url", "TEXT"),
        ("poster_data", "BLOB"),
        ("poster_mime", "TEXT"),
        ("plot", "TEXT"),
        ("metadata_loaded", "INTEGER NOT NULL DEFAULT 0"),
        ("metadata_fetch_complete", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !movie_columns.iter().any(|existing| existing == column) {
            sqlx::query(&format!(
                "ALTER TABLE movies ADD COLUMN {column} {definition}"
            ))
            .execute(&pool)
            .await?;
        }
    }

    sqlx::query(
        "UPDATE movies SET metadata_fetch_complete = 1 WHERE metadata_fetch_complete = 0 AND plot IS NOT NULL AND (poster_url IS NULL OR poster_data IS NOT NULL)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "UPDATE movies SET metadata_loaded = 0 WHERE metadata_loaded = 1 AND ((imdb_id IS NULL AND poster_url IS NULL AND plot IS NULL) OR (poster_url IS NOT NULL AND poster_data IS NULL))",
    )
    .execute(&pool)
    .await?;

    let unique_index_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_movies_title_year_unique'",
    )
    .fetch_one(&pool)
    .await?;

    if unique_index_exists == 0 {
        let mut transaction = pool.begin().await?;
        sqlx::query(
            "UPDATE movies SET is_watched = (SELECT MAX(duplicate.is_watched) FROM movies AS duplicate WHERE duplicate.title = movies.title COLLATE NOCASE AND duplicate.year = movies.year), wiki_url = COALESCE(NULLIF(movies.wiki_url, ''), (SELECT duplicate.wiki_url FROM movies AS duplicate WHERE duplicate.title = movies.title COLLATE NOCASE AND duplicate.year = movies.year AND duplicate.wiki_url IS NOT NULL AND duplicate.wiki_url <> '' ORDER BY duplicate.id LIMIT 1)) WHERE id IN (SELECT MIN(id) FROM movies GROUP BY title COLLATE NOCASE, year)",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "DELETE FROM movies WHERE id NOT IN (SELECT MIN(id) FROM movies GROUP BY title COLLATE NOCASE, year)",
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX idx_movies_title_year_unique ON movies(title COLLATE NOCASE, year)",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }

    Ok(pool)
}

pub async fn add_movie(
    pool: &SqlitePool,
    movie: &Movie,
) -> SqlxResult<sqlx::sqlite::SqliteQueryResult> {
    sqlx::query("INSERT OR IGNORE INTO movies (title, year, wiki_url) VALUES (?, ?, ?)")
        .bind(&movie.title)
        .bind(movie.year as i64)
        .bind(&movie.wiki_url)
        .execute(pool)
        .await
}

pub async fn add_movies_batch(pool: &SqlitePool, movies: &[Movie]) -> SqlxResult<(u64, u64)> {
    let mut count = 0u64;
    for movie in movies {
        match add_movie(pool, movie).await {
            Ok(result) if result.rows_affected() > 0 => count += 1,
            _ => {}
        }
    }
    Ok((movies.len() as u64, count))
}

#[derive(Debug, FromRow)]
pub struct MovieRow {
    pub id: i64,
    pub title: String,
    pub year: u16,
    pub is_watched: bool,
    pub wiki_url: String,
}

#[derive(Debug, FromRow)]
pub struct MovieMetadata {
    pub id: i64,
    pub title: String,
    pub year: u16,
    pub wiki_url: String,
    pub imdb_id: Option<String>,
    pub poster_url: Option<String>,
    pub poster_data: Option<Vec<u8>>,
    pub plot: Option<String>,
    pub metadata_fetch_complete: bool,
}

#[derive(Debug, FromRow)]
pub struct PendingMetadata {
    pub id: i64,
    pub title: String,
    pub year: u16,
    pub wiki_url: String,
}

pub async fn get_movie_metadata(pool: &SqlitePool, id: i64) -> SqlxResult<MovieMetadata> {
    sqlx::query_as::<_, MovieMetadata>(
        "SELECT id, title, year, COALESCE(wiki_url, '') AS wiki_url, imdb_id, poster_url, poster_data, plot, metadata_fetch_complete FROM movies WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
}

pub async fn save_movie_metadata(
    pool: &SqlitePool,
    id: i64,
    imdb_id: Option<&str>,
    poster_url: Option<&str>,
    poster_data: Option<&[u8]>,
    poster_mime: Option<&str>,
    plot: Option<&str>,
) -> SqlxResult<()> {
    sqlx::query(
        "UPDATE movies SET imdb_id = COALESCE(?, imdb_id), poster_url = COALESCE(?, poster_url), poster_data = COALESCE(?, poster_data), poster_mime = COALESCE(?, poster_mime), plot = COALESCE(?, plot), metadata_loaded = 1, metadata_fetch_complete = 1 WHERE id = ?",
    )
    .bind(imdb_id)
    .bind(poster_url)
    .bind(poster_data)
    .bind(poster_mime)
    .bind(plot)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_pending_metadata(
    pool: &SqlitePool,
    limit: usize,
) -> SqlxResult<Vec<PendingMetadata>> {
    sqlx::query_as::<_, PendingMetadata>(
        "SELECT id, title, year, COALESCE(wiki_url, '') AS wiki_url FROM movies WHERE metadata_fetch_complete = 0 ORDER BY id ASC LIMIT ?",
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await
}

pub async fn count_pending_metadata(pool: &SqlitePool) -> SqlxResult<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM movies WHERE metadata_fetch_complete = 0")
        .fetch_one(pool)
        .await
}

pub async fn save_backfilled_metadata(
    pool: &SqlitePool,
    id: i64,
    poster_url: Option<&str>,
    poster_data: Option<&[u8]>,
    poster_mime: Option<&str>,
    plot: Option<&str>,
    fetch_complete: bool,
) -> SqlxResult<()> {
    sqlx::query(
        "UPDATE movies SET poster_url = COALESCE(?, poster_url), poster_data = COALESCE(?, poster_data), poster_mime = COALESCE(?, poster_mime), plot = COALESCE(?, plot), metadata_loaded = 1, metadata_fetch_complete = ? WHERE id = ?",
    )
    .bind(poster_url)
    .bind(poster_data)
    .bind(poster_mime)
    .bind(plot)
    .bind(fetch_complete)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_movie_poster(pool: &SqlitePool, id: i64) -> SqlxResult<Option<(Vec<u8>, String)>> {
    let row = sqlx::query(
        "SELECT poster_data, COALESCE(poster_mime, 'image/jpeg') AS poster_mime FROM movies WHERE id = ? AND poster_data IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| Ok((row.try_get("poster_data")?, row.try_get("poster_mime")?)))
        .transpose()
}

impl From<MovieRow> for (i64, String, u16, bool, String) {
    fn from(movie: MovieRow) -> Self {
        (
            movie.id,
            movie.title,
            movie.year,
            movie.is_watched,
            movie.wiki_url,
        )
    }
}

pub type MovieItem = (i64, String, u16, bool, String);

pub struct MoviePage {
    pub movies: Vec<MovieItem>,
    pub total: usize,
    pub page: usize,
    pub page_count: usize,
}

pub async fn search_movies_page(
    pool: &SqlitePool,
    query: &str,
    filter: &str,
    sort: &str,
    year: Option<u16>,
    requested_page: usize,
    requested_page_size: usize,
) -> SqlxResult<MoviePage> {
    let terms = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| {
            term.to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        })
        .collect::<Vec<_>>();
    let mut predicate = String::from(" FROM movies WHERE 1 = 1");
    for _ in &terms {
        predicate.push_str(" AND lower(title) LIKE ? ESCAPE '\\'");
    }
    match filter.to_lowercase().as_str() {
        "watched" => predicate.push_str(" AND is_watched = 1"),
        "unwatched" => predicate.push_str(" AND is_watched = 0"),
        _ => {}
    }
    if year.is_some() {
        predicate.push_str(" AND year = ?");
    }

    let count_statement = format!("SELECT COUNT(*){predicate}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_statement);
    for term in &terms {
        count_query = count_query.bind(format!("%{term}%"));
    }
    if let Some(year) = year {
        count_query = count_query.bind(year);
    }
    let total = count_query.fetch_one(pool).await?.max(0) as usize;
    let page_size = requested_page_size.clamp(1, 100);
    let page_count = total.div_ceil(page_size).max(1);
    let page = requested_page.clamp(1, page_count);
    let offset = (page - 1) * page_size;

    let statement = format!(
        "SELECT id, title, year, is_watched, COALESCE(wiki_url, '') AS wiki_url{predicate} ORDER BY {} LIMIT ? OFFSET ?",
        movie_order(sort)
    );
    let mut movies_query = sqlx::query_as::<_, MovieRow>(&statement);
    for term in &terms {
        movies_query = movies_query.bind(format!("%{term}%"));
    }
    if let Some(year) = year {
        movies_query = movies_query.bind(year);
    }
    let movies = movies_query
        .bind(page_size as i64)
        .bind(offset as i64)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();

    Ok(MoviePage {
        movies,
        total,
        page,
        page_count,
    })
}

pub async fn toggle_watched(pool: &SqlitePool, id: i64) -> SqlxResult<bool> {
    let mut transaction = pool.begin().await?;
    let row = sqlx::query("SELECT is_watched FROM movies WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    let is_watched: i32 = row.get("is_watched");
    let new_status = if is_watched == 0 { 1 } else { 0 };
    sqlx::query("UPDATE movies SET is_watched = ? WHERE id = ?")
        .bind(new_status)
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(new_status == 1)
}

pub async fn get_stats(pool: &SqlitePool) -> SqlxResult<(u64, u64)> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
        .fetch_one(pool)
        .await?;
    let watched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies WHERE is_watched = 1")
        .fetch_one(pool)
        .await?;
    Ok((total as u64, watched as u64))
}

pub async fn get_filtered_movies(
    pool: &SqlitePool,
    filter: &str,
    sort: &str,
    year: Option<u16>,
) -> SqlxResult<Vec<(i64, String, u16, bool, String)>> {
    let filter_clause = match filter.to_lowercase().as_str() {
        "watched" => " WHERE is_watched = 1",
        "unwatched" => " WHERE is_watched = 0",
        _ => "",
    };
    let year_clause = if year.is_some() {
        if filter_clause.is_empty() {
            " WHERE year = ?"
        } else {
            " AND year = ?"
        }
    } else {
        ""
    };
    let statement = format!("SELECT id, title, year, is_watched, COALESCE(wiki_url, '') AS wiki_url FROM movies{}{} ORDER BY {}", filter_clause, year_clause, movie_order(sort));
    let mut query = sqlx::query_as::<_, MovieRow>(&statement);
    if let Some(year) = year {
        query = query.bind(year);
    }
    query
        .fetch_all(pool)
        .await
        .map(|movies| movies.into_iter().map(Into::into).collect())
}

fn movie_order(sort: &str) -> &'static str {
    match sort {
        "year_asc" => "year ASC, title ASC",
        "title" => "title COLLATE NOCASE ASC, year DESC",
        "watched" => "is_watched DESC, year DESC, title ASC",
        _ => "year DESC, title ASC",
    }
}

#[cfg(test)]
mod pagination_tests {
    use super::search_movies_page;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn database_pages_are_bounded_and_keep_filters() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE movies (id INTEGER PRIMARY KEY, title TEXT NOT NULL, year INTEGER NOT NULL, is_watched INTEGER NOT NULL, wiki_url TEXT)",
        )
        .execute(&pool)
        .await
        .unwrap();

        for id in 1..=101 {
            sqlx::query(
                "INSERT INTO movies (id, title, year, is_watched, wiki_url) VALUES (?, ?, ?, ?, '')",
            )
            .bind(id)
            .bind(format!("Movie {id:03}"))
            .bind(if id % 2 == 0 { 2000 } else { 1999 })
            .bind(i64::from(id % 3 == 0))
            .execute(&pool)
            .await
            .unwrap();
        }

        let first = search_movies_page(&pool, "", "all", "title", None, 1, 50)
            .await
            .unwrap();
        assert_eq!((first.total, first.page, first.page_count), (101, 1, 3));
        assert_eq!(first.movies.len(), 50);

        let second = search_movies_page(&pool, "", "all", "title", None, 2, 50)
            .await
            .unwrap();
        assert_eq!(second.movies.len(), 50);
        assert_eq!(second.movies[0].1, "Movie 051");

        let last = search_movies_page(&pool, "", "all", "title", None, 99, 50)
            .await
            .unwrap();
        assert_eq!((last.page, last.movies.len()), (3, 1));

        let watched = search_movies_page(&pool, "", "watched", "title", None, 1, 50)
            .await
            .unwrap();
        assert_eq!(watched.total, 33);
        assert!(watched.movies.iter().all(|movie| movie.3));

        let year = search_movies_page(&pool, "", "all", "title", Some(2000), 1, 50)
            .await
            .unwrap();
        assert_eq!(year.total, 50);
        assert!(year.movies.iter().all(|movie| movie.2 == 2000));
    }
}
