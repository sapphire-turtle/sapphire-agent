//! The speech bubble: what it shows and for how long (spec §5).
//!
//! Pure. Times are the time since the app started, as the system calling
//! in measures it.

use std::time::Duration;

const BASE: Duration = Duration::from_secs(3);
const PER_CHAR: Duration = Duration::from_millis(60);
const LONGEST: Duration = Duration::from_secs(20);

/// How long a reply of `text` stays in the bubble: 3 s plus 60 ms a
/// character, within 3–20 s. Characters, not bytes, so Japanese reads as
/// long as English.
pub fn bubble_duration(text: &str) -> Duration {
    let chars = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    BASE.saturating_add(PER_CHAR.saturating_mul(chars))
        .clamp(BASE, LONGEST)
}

/// A reply in the bubble, shown until `until`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bubble {
    pub text: String,
    pub until: Duration,
}

/// What the bubble says.
#[derive(Clone, Debug, PartialEq)]
pub enum BubbleContent {
    /// A turn is in flight.
    Thinking,
    Reply(String),
}

/// The bubble at `now`: thinking while a turn is `sending`, else the
/// latest reply until its time is up.
pub fn content(sending: bool, bubble: Option<&Bubble>, now: Duration) -> Option<BubbleContent> {
    if sending {
        return Some(BubbleContent::Thinking);
    }
    bubble
        .filter(|b| now < b.until)
        .map(|b| BubbleContent::Reply(b.text.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_grows_with_text_and_is_clamped() {
        assert_eq!(bubble_duration(""), Duration::from_secs(3));
        assert_eq!(bubble_duration(&"a".repeat(50)), Duration::from_secs(6));
        assert_eq!(bubble_duration(&"a".repeat(1000)), Duration::from_secs(20));
        // Characters, not bytes.
        assert_eq!(bubble_duration(&"あ".repeat(50)), Duration::from_secs(6));
    }

    #[test]
    fn thinking_outranks_the_reply_which_shows_until_its_time() {
        let s = Duration::from_secs;
        let bubble = Bubble {
            text: "hi".into(),
            until: s(10),
        };
        assert_eq!(
            content(true, Some(&bubble), s(5)),
            Some(BubbleContent::Thinking)
        );
        assert_eq!(
            content(false, Some(&bubble), s(5)),
            Some(BubbleContent::Reply("hi".into()))
        );
        assert_eq!(content(false, Some(&bubble), s(10)), None);
        assert_eq!(content(false, None, s(5)), None);
        assert_eq!(content(true, None, s(5)), Some(BubbleContent::Thinking));
    }
}
