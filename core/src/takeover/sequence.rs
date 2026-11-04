#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotASocket,
    OtherUid,
    Off,
    TooOld(String),
    NotOneTui,
    AlreadyTaken,
    NoDisplay,
    Blocking,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    Refused(Refusal),
    RoomStart(String),
    Install(String),
    MoveError(String),
    MoveUnanswered,
    TuiStayed,
    OtherUi,
}

impl StepError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Refused(_) => 64,
            Self::RoomStart(_)
            | Self::Install(_)
            | Self::MoveError(_)
            | Self::MoveUnanswered
            | Self::TuiStayed
            | Self::OtherUi => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Refusal, StepError};

    #[test]
    fn exit_codes_are_64_for_a_refusal_and_1_otherwise() {
        for refusal in [
            Refusal::NotASocket,
            Refusal::OtherUid,
            Refusal::Off,
            Refusal::TooOld("0.11.2".into()),
            Refusal::NotOneTui,
            Refusal::AlreadyTaken,
            Refusal::NoDisplay,
            Refusal::Blocking,
        ] {
            assert_eq!(StepError::Refused(refusal).exit_code(), 64);
        }

        for error in [
            StepError::RoomStart("not yet".into()),
            StepError::Install("not yet".into()),
            StepError::MoveError("connection closed".into()),
            StepError::MoveUnanswered,
            StepError::TuiStayed,
            StepError::OtherUi,
        ] {
            assert_eq!(error.exit_code(), 1);
        }
    }
}
