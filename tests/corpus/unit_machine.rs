// Corpus unit: state machine. Enums moved by value through `self`, a `match` on a tuple of
// two enums with bindings and a catch-all, `loop` with `break value`, reassignment of a moved
// local. `Q0` is the per-unit tag; see `unit_geometry.rs`.

pub enum StateQ0 {
    Idle,
    Running(u32),
    Paused { at: u32, reason: u32 },
    Done(u32),
}

pub enum EventQ0 {
    Start,
    Tick,
    Pause(u32),
    Resume,
    Stop,
}

impl StateQ0 {
    pub fn step(self, event: EventQ0) -> StateQ0 {
        match (self, event) {
            (StateQ0::Idle, EventQ0::Start) => StateQ0::Running(0),
            (StateQ0::Running(n), EventQ0::Tick) => StateQ0::Running(n + 1),
            (StateQ0::Running(n), EventQ0::Pause(reason)) => StateQ0::Paused { at: n, reason },
            (StateQ0::Paused { at, reason: _ }, EventQ0::Resume) => StateQ0::Running(at),
            (StateQ0::Running(n), EventQ0::Stop) => StateQ0::Done(n),
            (StateQ0::Paused { at, reason }, EventQ0::Stop) => StateQ0::Done(at + reason),
            (state, _) => state,
        }
    }

    pub fn value(&self) -> u32 {
        match self {
            StateQ0::Idle => 0,
            StateQ0::Running(n) => *n,
            StateQ0::Paused { at, reason } => *at + *reason,
            StateQ0::Done(n) => *n * 2,
        }
    }

    pub fn is_done(&self) -> bool {
        match self {
            StateQ0::Done(_) => true,
            _ => false,
        }
    }
}

pub fn event_Q0(i: u32) -> EventQ0 {
    if i == 0 {
        EventQ0::Start
    } else if i == 5 {
        EventQ0::Pause(i)
    } else if i == 7 {
        EventQ0::Resume
    } else if i > 20 {
        EventQ0::Stop
    } else {
        EventQ0::Tick
    }
}

pub fn run_Q0(limit: u32) -> u32 {
    let mut state = StateQ0::Idle;
    let mut i: u32 = 0;
    let result = loop {
        if i > limit {
            break state.value();
        }
        state = state.step(event_Q0(i));
        if state.is_done() {
            break state.value() + 1;
        }
        i = i + 1;
    };
    result
}
