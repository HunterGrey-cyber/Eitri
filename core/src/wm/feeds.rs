//! The feed pump's timer and listener-watch policy, independent of source registration.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpWake {
    Timer,
    Watches,
    Polling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    TimerFired,
    ListenerReady,
    ReaderAdded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Before {
    pub remove_watches: bool,
    pub tick: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    KeepTimer,
    ArmTimer,
    AddWatches,
    Nothing,
}

pub fn before(now: PumpWake, event: PumpEvent) -> Before {
    use PumpEvent::*;
    use PumpWake::*;
    match (now, event) {
        (Timer, TimerFired) => Before {
            remove_watches: false,
            tick: true,
        },
        (Watches, ListenerReady) => Before {
            remove_watches: true,
            tick: true,
        },
        (Watches, ReaderAdded) => Before {
            remove_watches: true,
            tick: false,
        },
        (Timer | Polling, ReaderAdded) => Before {
            remove_watches: false,
            tick: false,
        },
        _ => {
            debug_assert!(false, "no source dispatches {event:?} while {now:?}");
            Before {
                remove_watches: false,
                tick: false,
            }
        }
    }
}

pub fn after(now: PumpWake, event: PumpEvent, quiet: bool) -> (PumpWake, Arm) {
    use PumpEvent::*;
    use PumpWake::*;
    match (now, event) {
        (Timer, TimerFired) if !quiet => (Timer, Arm::KeepTimer),
        (Timer, TimerFired) | (Watches, ListenerReady | ReaderAdded) => {
            if quiet {
                (Watches, Arm::AddWatches)
            } else {
                (Timer, Arm::ArmTimer)
            }
        }
        (Timer | Polling, ReaderAdded) => (now, Arm::Nothing),
        _ => {
            debug_assert!(false, "no source dispatches {event:?} while {now:?}");
            (now, Arm::Nothing)
        }
    }
}

pub fn watch_refused() -> (PumpWake, Arm) {
    (PumpWake::Timer, Arm::ArmTimer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pump_policy_follows_the_transition_table() {
        use Arm::*;
        use PumpEvent::*;
        use PumpWake::*;
        for (now, event, quiet, remove_watches, tick, next, arm) in [
            (Timer, TimerFired, true, false, true, Watches, AddWatches),
            (Timer, TimerFired, false, false, true, Timer, KeepTimer),
            (Watches, ListenerReady, true, true, true, Watches, AddWatches),
            (Watches, ListenerReady, false, true, true, Timer, ArmTimer),
            (Watches, ReaderAdded, true, true, false, Watches, AddWatches),
            (Watches, ReaderAdded, false, true, false, Timer, ArmTimer),
            (Timer, ReaderAdded, true, false, false, Timer, Nothing),
            (Timer, ReaderAdded, false, false, false, Timer, Nothing),
            (Polling, ReaderAdded, true, false, false, Polling, Nothing),
            (Polling, ReaderAdded, false, false, false, Polling, Nothing),
        ] {
            assert_eq!(
                (before(now, event), after(now, event, quiet)),
                (Before { remove_watches, tick }, (next, arm)),
                "{now:?}, {event:?}, quiet={quiet}"
            );
        }
        assert_eq!(watch_refused(), (Timer, ArmTimer));
    }

    macro_rules! impossible {
        ($before:ident, $after:ident, $now:ident, $event:ident) => {
            #[test]
            #[cfg_attr(debug_assertions, should_panic)]
            fn $before() {
                let result = before(PumpWake::$now, PumpEvent::$event);
                #[cfg(not(debug_assertions))]
                assert_eq!(
                    result,
                    Before {
                        remove_watches: false,
                        tick: false
                    }
                );
                #[cfg(debug_assertions)]
                let _ = result;
            }

            #[test]
            #[cfg_attr(debug_assertions, should_panic)]
            fn $after() {
                for quiet in [true, false] {
                    let result = after(PumpWake::$now, PumpEvent::$event, quiet);
                    #[cfg(not(debug_assertions))]
                    assert_eq!(result, (PumpWake::$now, Arm::Nothing));
                    #[cfg(debug_assertions)]
                    let _ = result;
                }
            }
        };
    }

    impossible!(timer_listener_before, timer_listener_after, Timer, ListenerReady);
    impossible!(watches_timer_before, watches_timer_after, Watches, TimerFired);
    impossible!(polling_timer_before, polling_timer_after, Polling, TimerFired);
    impossible!(polling_listener_before, polling_listener_after, Polling, ListenerReady);
}
