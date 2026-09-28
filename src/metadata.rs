use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use std::time::{Duration, SystemTime};

#[derive(Debug, Deserialize)]
pub struct WikipediaSummary {
    pub extract: Option<String>,
    pub thumbnail: Option<WikipediaThumbnail>,
}

#[derive(Debug, Deserialize)]
pub struct WikipediaThumbnail {
    pub source: String,
}

pub fn wikipedia_summary_url(wiki_url: &str) -> Option<url::Url> {
    let article_url = url::Url::parse(wiki_url).ok()?;
    if article_url.host_str() != Some("en.wikipedia.org") {
        return None;
    }
    let encoded_slug = article_url.path_segments()?.last()?;
    let slug = percent_encoding::percent_decode_str(encoded_slug)
        .decode_utf8()
        .ok()?;
    let mut summary_url =
        url::Url::parse("https://en.wikipedia.org/api/rest_v1/page/summary").ok()?;
    summary_url.path_segments_mut().ok()?.push(&slug);
    Some(summary_url)
}

pub fn validated_image_url(image_url: &str) -> Option<String> {
    let url = url::Url::parse(image_url).ok()?;
    let host = url.host_str()?;
    let trusted_host = host == "wikimedia.org"
        || host.ends_with(".wikimedia.org")
        || host == "wikipedia.org"
        || host.ends_with(".wikipedia.org");
    (url.scheme() == "https" && trusted_host).then(|| url.to_string())
}

async fn send_with_retry(client: &Client, url: url::Url) -> Result<Response, reqwest::Error> {
    const MAX_RETRIES: u32 = 5;

    for attempt in 0..=MAX_RETRIES {
        let response = client.get(url.clone()).send().await?;
        if !matches!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
        ) {
            return Ok(response);
        }
        if attempt == MAX_RETRIES {
            return Ok(response);
        }

        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(|value| retry_after_delay(value, attempt));
        let delay = retry_after.unwrap_or_else(|| retry_after_delay("", attempt));
        eprintln!(
            "Wikimedia returned HTTP {}; retrying in {} seconds (attempt {}/{})",
            response.status(),
            delay.as_secs(),
            attempt + 1,
            MAX_RETRIES
        );
        tokio::time::sleep(delay).await;
    }

    unreachable!("retry loop always returns or continues")
}

fn retry_after_delay(value: &str, attempt: u32) -> Duration {
    if let Ok(seconds) = value.parse::<u64>() {
        return Duration::from_secs(seconds);
    }
    if let Ok(retry_at) = httpdate::parse_http_date(value) {
        return retry_at
            .duration_since(SystemTime::now())
            .unwrap_or_default();
    }
    Duration::from_secs(5u64.saturating_mul(1u64 << attempt.min(6)))
}

pub async fn fetch_wikipedia_summary(
    client: &Client,
    wiki_url: &str,
) -> Result<Option<WikipediaSummary>, reqwest::Error> {
    let Some(url) = wikipedia_summary_url(wiki_url) else {
        return Ok(None);
    };
    let response = send_with_retry(client, url).await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(
        response
            .error_for_status()?
            .json::<WikipediaSummary>()
            .await?,
    ))
}

pub async fn download_image(
    client: &Client,
    image_url: &str,
) -> Result<Option<(Vec<u8>, String)>, reqwest::Error> {
    let Ok(url) = url::Url::parse(image_url) else {
        return Ok(None);
    };
    if url.scheme() != "https" {
        return Ok(None);
    }

    let response = send_with_retry(client, url).await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > 5_000_000)
    {
        return Ok(None);
    }
    let Some(content_type) = response.headers().get("content-type") else {
        return Ok(None);
    };
    let Ok(content_type) = content_type.to_str() else {
        return Ok(None);
    };
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/jpeg" | "image/png" | "image/webp" | "image/gif" | "image/avif"
    ) {
        return Ok(None);
    }

    let bytes = response.bytes().await?;
    if bytes.is_empty() || bytes.len() > 5_000_000 {
        return Ok(None);
    }
    Ok(Some((bytes.to_vec(), mime)))
}

#[cfg(test)]
mod tests {
    use super::{retry_after_delay, wikipedia_summary_url, WikipediaSummary};
    use std::time::{Duration, SystemTime};

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(retry_after_delay("17", 0), Duration::from_secs(17));
        assert_eq!(retry_after_delay("invalid", 0), Duration::from_secs(5));

        let retry_at = SystemTime::now() + Duration::from_secs(30);
        let header = httpdate::fmt_http_date(retry_at);
        let delay = retry_after_delay(&header, 0);
        assert!(delay <= Duration::from_secs(30));
    }

    #[test]
    fn summary_url_uses_the_article_slug() {
        assert_eq!(
            wikipedia_summary_url("https://en.wikipedia.org/wiki/Toy_Story")
                .unwrap()
                .as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/Toy_Story"
        );
    }

    #[test]
    fn summary_url_decodes_existing_percent_escapes_once() {
        assert_eq!(
            wikipedia_summary_url(
                "https://en.wikipedia.org/wiki/The_Naked_Gun_2%C2%BD:_The_Smell_of_Fear"
            )
            .unwrap()
            .as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/The_Naked_Gun_2%C2%BD:_The_Smell_of_Fear"
        );
    }

    #[test]
    fn summary_url_keeps_ampersand_inside_article_path() {
        assert_eq!(
            wikipedia_summary_url("https://en.wikipedia.org/wiki/Bodies,_Rest_&_Motion")
                .unwrap()
                .as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/Bodies,_Rest_&_Motion"
        );
    }

    #[test]
    fn summary_url_preserves_literal_plus_in_article_slug() {
        assert_eq!(
            wikipedia_summary_url("https://en.wikipedia.org/wiki/A+B")
                .unwrap()
                .as_str(),
            "https://en.wikipedia.org/api/rest_v1/page/summary/A+B"
        );
    }

    #[test]
    fn summary_payload_contains_overview_and_thumbnail() {
        let summary: WikipediaSummary = serde_json::from_str(
            r#"{"extract":"A useful overview.","thumbnail":{"source":"https://upload.wikimedia.org/poster.jpg"}}"#,
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
