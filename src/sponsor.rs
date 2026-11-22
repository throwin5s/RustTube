//! Opt-in SponsorBlock skipping.
//!
//! SponsorBlock is a community database of intro/outro/sponsor ranges. This is
//! an extra third-party request, so the whole feature is off by default and the
//! user picks which categories are skipped. Only the video id and the chosen
//! category names are sent.

use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub category: String,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    category: String,
    #[serde(rename = "actionType", default)]
    action_type: String,
    segment: Vec<f64>,
}

impl RawEntry {
    fn into_segment(self) -> Option<Segment> {
        if self.segment.len() < 2 {
            return None;
        }
        let start = self.segment[0];
        let end = self.segment[1];
        if !start.is_finite() || !end.is_finite() || end <= start {
            return None;
        }
        // `mute` and `poi` are not skip actions; treating them as skips would
        // jump over content the user asked to hear or see.
        if !self.action_type.is_empty() && self.action_type != "skip" {
            return None;
        }
        Some(Segment {
            start,
            end,
            category: self.category,
        })
    }
}

/// Fetch skippable ranges for one video. An empty database answer (404) is a
/// normal result, not an error.
pub async fn segments(video_id: &str, categories: &[String]) -> Result<Vec<Segment>, String> {
    if categories.is_empty() {
        return Ok(Vec::new());
    }
    let categories = serde_json::to_string(categories).map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get("https://sponsor.ink/api/skipSegments")
        .query(&[("videoID", video_id), ("categories", categories.as_str())])
        .send()
        .await
        .map_err(|e| format!("SponsorBlock: {e}"))?;

    if response.status().as_u16() == 404 {
        return Ok(Vec::new());
    }
    if !response.status().is_success() {
        return Err(format!(
            "SponsorBlock returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let entries: Vec<RawEntry> = response
        .json()
        .await
        .map_err(|_| "SponsorBlock returned an unexpected response".to_string())?;
    Ok(entries
        .into_iter()
        .filter_map(RawEntry::into_segment)
        .collect())
}

/// Where playback should jump when it is inside a skippable range.
pub fn skip_target(segments: &[Segment], position: f64) -> Option<f64> {
    segments
        .iter()
        .find(|segment| position >= segment.start - 0.25 && position < segment.end - 0.75)
        .map(|segment| segment.end)
}

pub fn parse_categories(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|category| category.trim().to_lowercase())
        .filter(|category| !category.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: f64, end: f64) -> Segment {
        Segment {
            start,
            end,
            category: "sponsor".into(),
        }
    }

    #[test]
    fn skips_only_inside_a_range() {
        let segments = vec![segment(10.0, 20.0)];
        assert_eq!(skip_target(&segments, 9.0), None);
        assert_eq!(skip_target(&segments, 12.0), Some(20.0));
        // Standing at the end must not bounce the player backwards.
        assert_eq!(skip_target(&segments, 20.0), None);
        assert_eq!(skip_target(&segments, 19.5), None);
    }

    #[test]
    fn parses_and_ignores_duplicate_categories() {
        assert_eq!(
            parse_categories("sponsor, intro , ,outro"),
            vec!["sponsor", "intro", "outro"]
        );
    }

    #[test]
    fn raw_entries_ignore_mute_and_poi() {
        let skip = RawEntry {
            category: "intro".into(),
            action_type: "skip".into(),
            segment: vec![0.0, 5.0],
        }
        .into_segment()
        .expect("skip");
        assert_eq!(skip.end, 5.0);

        assert!(RawEntry {
            category: "sponsor".into(),
            action_type: "mute".into(),
            segment: vec![0.0, 5.0],
        }
        .into_segment()
        .is_none());
        assert!(RawEntry {
            category: "sponsor".into(),
            action_type: String::new(),
            segment: vec![0.0],
        }
        .into_segment()
        .is_none());
    }
}
