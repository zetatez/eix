use orders::{next, Event, State};

#[test]
fn any_open_state_can_close() {
    assert_eq!(next(State::Draft, Event::Close), Some(State::Closed));
    assert_eq!(next(State::Active, Event::Close), Some(State::Closed));
    assert_eq!(next(State::Paused, Event::Close), Some(State::Closed));
}

#[test]
fn closed_is_terminal() {
    for event in [Event::Submit, Event::Pause, Event::Resume, Event::Close] {
        assert_eq!(next(State::Closed, event), None);
    }
}

#[test]
fn invalid_events_are_rejected() {
    assert_eq!(next(State::Draft, Event::Pause), None);
    assert_eq!(next(State::Draft, Event::Resume), None);
    assert_eq!(next(State::Active, Event::Submit), None);
    assert_eq!(next(State::Active, Event::Resume), None);
    assert_eq!(next(State::Paused, Event::Submit), None);
    assert_eq!(next(State::Paused, Event::Pause), None);
}

#[test]
fn valid_transitions_still_work() {
    assert_eq!(next(State::Draft, Event::Submit), Some(State::Active));
    assert_eq!(next(State::Active, Event::Pause), Some(State::Paused));
    assert_eq!(next(State::Paused, Event::Resume), Some(State::Active));
}
