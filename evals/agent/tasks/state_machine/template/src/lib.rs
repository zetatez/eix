mod machine;

pub use machine::{next, Event, State};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_starts_the_order() {
        assert_eq!(next(State::Draft, Event::Submit), Some(State::Active));
    }

    #[test]
    fn pause_and_resume_round_trip() {
        assert_eq!(next(State::Active, Event::Pause), Some(State::Paused));
        assert_eq!(next(State::Paused, Event::Resume), Some(State::Active));
    }
}
