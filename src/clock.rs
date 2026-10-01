//! Time, injectable — because the state machine is tested and `SystemTime::now()` inside it
//! would make every test a race. Milliseconds since the epoch everywhere; the registry is
//! JSON and a u64 of milliseconds reads the same in every language that might ever look at
//! it.

use std::time::{SystemTime, UNIX_EPOCH};

pub type Millis = u64;

pub fn now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        // A clock before 1970 is not a case worth a Result all the way up the call stack.
        .unwrap_or(0)
}
