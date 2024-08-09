use nostr_sdk::Timestamp;

pub struct TimeDelta {
    pub delta_abs_seconds: u64,
    pub negative: bool,
}

impl TimeDelta {
    /// Safely calculate the difference between two timestamps in seconds
    /// This function is safer against overflows than subtracting the timestamps directly
    pub fn subtracting(t1: Timestamp, t2: Timestamp) -> TimeDelta {
        if t1 > t2 {
            TimeDelta {
                delta_abs_seconds: (t1 - t2).as_u64(),
                negative: false,
            }
        } else {
            TimeDelta {
                delta_abs_seconds: (t2 - t1).as_u64(),
                negative: true,
            }
        }
    }
}

impl std::fmt::Display for TimeDelta {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.negative {
            write!(f, "-{}", self.delta_abs_seconds)
        } else {
            write!(f, "{}", self.delta_abs_seconds)
        }
    }
}
