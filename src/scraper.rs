use reqwest::redirect;
use scraper::{Html, Selector};
use std::collections::HashSet;

/// Movie structures for seeding data
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Movie {
    pub title: String,
    pub year: u16,
    pub wiki_url: String,
}

/// Cleaner for Wikipedia movie titles
fn clean_movie_title(raw_title: &str) -> String {
    if raw_title.trim().is_empty() {
        return String::new();
    }

    // Remove Wikipedia reference footnotes like [1], [a], etc.
    let cleaned = regex::Regex::new(r"\[[^\]]+\]")
        .unwrap()
        .replace(raw_title, "");

    // Remove quotes surrounding title
    let cleaned = cleaned.trim_matches([' ', '"', '\'', '“', '”', '«', '»']);

    // Strip trailing disambiguations like "(film)", "(2000 film)"
    let cleaned = regex::Regex::new(r"\s*\((?:film|\d{4}\s+film|\d{4})\)")
        .unwrap()
        .replace(&cleaned, "");

    // Replace weird whitespace with single space
    let cleaned = regex::Regex::new(r"\s+").unwrap().replace(&cleaned, " ");

    cleaned.trim().to_string()
}

/// Scrape the American and British movie lists for a specific year.
pub async fn scrape_year(
    year: u16,
) -> Result<Vec<Movie>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .user_agent("MovieTracker/0.1 (movie catalog importer)")
        .redirect(redirect::Policy::limited(10))
        .build()?;

    let urls = [
        format!("https://en.wikipedia.org/wiki/List_of_American_films_of_{year}"),
        format!("https://en.wikipedia.org/wiki/List_of_British_films_of_{year}"),
    ];
    let mut movies = Vec::new();
    let mut seen = HashSet::new();
    let mut successful_sources = 0;
    let mut source_errors = Vec::new();

    for (index, url) in urls.iter().enumerate() {
        match scrape_list(&client, url, year).await {
            Ok(listed_movies) => {
                successful_sources += 1;
                for movie in listed_movies {
                    if seen.insert((movie.title.to_lowercase(), movie.year)) {
                        movies.push(movie);
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "Failed to scrape {} list for {year}: {error}",
                    if index == 0 { "American" } else { "British" }
                );
                source_errors.push(error.to_string());
            }
        }
        if index + 1 < urls.len() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    if successful_sources == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "all movie lists failed for {year}: {}",
                source_errors.join("; ")
            ),
        )
        .into());
    }

    Ok(movies)
}

async fn scrape_list(
    client: &reqwest::Client,
    url: &str,
    year: u16,
) -> Result<Vec<Movie>, Box<dyn std::error::Error + Send + Sync>> {
    let response = client.get(url).send().await?.error_for_status()?;
    let html = response.text().await?;
    let document = Html::parse_document(&html);
    let table_selectors = vec![
        Selector::parse(".wikitable tbody tr").unwrap(),
        Selector::parse("table tbody tr").unwrap(),
    ];
    let mut movies = Vec::new();
    let base_url = url::Url::parse(url)?;

    for selector in &table_selectors {
        for element in document.select(selector) {
            if let Some(title_cell) = element.select(&Selector::parse("a").unwrap()).next() {
                let title = clean_movie_title(title_cell.text().collect::<String>().as_str());

                if !title.is_empty() {
                    let wiki_url = title_cell
                        .value()
                        .attr("href")
                        .and_then(|href| base_url.join(href).ok())
                        .map(|href| href.to_string())
                        .unwrap_or_else(|| url.to_string());

                    movies.push(Movie {
                        title,
                        year,
                        wiki_url,
                    });
                }
            }
        }
        if !movies.is_empty() {
            break;
        }
    }

    Ok(movies)
}

/// Recursive scraper that fetches data for a year range
pub async fn scrape_range(
    start_year: u16,
    end_year: u16,
) -> Result<Vec<Movie>, Box<dyn std::error::Error + Send + Sync>> {
    let mut all_movies = Vec::new();

    for year in start_year..=end_year {
        println!("Scraping {}...", year);
        match scrape_year(year).await {
            Ok(year_movies) => {
                let count = year_movies.len();
                all_movies.extend(year_movies);
                println!("Found {} movies for {}", count, year);
            }
            Err(e) => {
                eprintln!("Failed to scrape {}: {}", year, e);
            }
        }

        // Small delay to be polite
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    Ok(all_movies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scrape_2022_includes_british_films() {
        let movies = scrape_year(2022).await.unwrap();
        assert!(movies.iter().any(|movie| {
            movie.year == 2022 && movie.title == "What's Love Got to Do with It?"
        }));
    }

    #[tokio::test]
    async fn test_scrape_2020() {
        let movies = scrape_year(2020).await.unwrap();
        println!("Found {} movies", movies.len());
        assert!(!movies.is_empty());
    }

    #[test]
    fn test_clean_title() {
        assert_eq!(clean_movie_title("Toy Story (1995 film)"), "Toy Story");
        assert_eq!(
            clean_movie_title("The Shawshank Redemption [1]"),
            "The Shawshank Redemption"
        );
    }
}
