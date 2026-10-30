#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Draft,
    Active,
    Paused,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Submit,
    Pause,
    Resume,
    Close,
}

/// Advance the order lifecycle, or return `None` when the event does not
/// apply to the current state.
pub fn next(state: State, event: Event) -> Option<State> {
    match (state, event) {
        (State::Draft, Event::Submit) => Some(State::Active),
        (State::Active, Event::Pause) => Some(State::Paused),
        (State::Paused, Event::Resume) => Some(State::Active),
        _ => None,
    }
}
