//! Long polling lets a request handler with nothing new to tell the client
//! leave the request unanswered. The main loop runs the request again once the
//! server's state changes.
//!
//! `workspace/diagnostic` is the one long-polling request. VS Code sends it
//! again 2 s after every response, so answering "nothing changed" at once would
//! have the server check the whole workspace every 2 s while the user only reads
//! code. Instead:
//!
//! 1. The handler, registered with
//!    [`RequestDispatcher::on_long_poll`](crate::dispatch::RequestDispatcher::on_long_poll),
//!    returns [`LongPoll::Suspend`] rather than a result.
//! 2. The dispatcher hands the request back to the main loop as a
//!    [`Task::Suspend`](crate::main_loop::Task::Suspend), and the main loop parks
//!    it in a [`SuspendedRequest`] ([`SuspendedRequest::park`]). The request
//!    stays registered in the request queue, unanswered. If the server's state
//!    changed while the handler ran, the handler's answer may predate the
//!    change, so the main loop runs the request again at once instead.
//! 3. When the server's state changes, the main loop takes the parked request
//!    ([`SuspendedRequest::wake`]) and dispatches it again with a fresh snapshot.
//!    It may suspend again.
//!
//! A `$/cancelRequest` drops the parked request (the request queue answers it
//! with `RequestCancelled`). A shutdown, or a second request parking in its
//! place, answers it with its "nothing changed" result
//! ([`SuspendedRequest::give_up`], [`Park::Displaced`]).

use lsp_server::{Request, RequestId, Response};

/// What a long-polling handler wants done with its request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LongPoll<T> {
    /// Answer the request with this result.
    Respond(T),
    /// Nothing changed for the client: leave the request open, and run it again
    /// once the server's state changes.
    Suspend,
}

#[cfg(test)]
impl<T> LongPoll<T> {
    /// The result to answer with. Panics if the handler suspended the request.
    pub(crate) fn unwrap_respond(self) -> T {
        match self {
            Self::Respond(result) => result,
            Self::Suspend => panic!("expected a response, but the request was suspended"),
        }
    }
}

/// A request whose handler returned [`LongPoll::Suspend`], on its way from the
/// task pool to the main loop.
#[derive(Debug)]
pub(crate) struct Suspended {
    pub(crate) request: Request,
    /// The answer that tells the client nothing changed, for when the server
    /// gives up on the request instead of running it again.
    pub(crate) no_news: Response,
    /// The [`SuspendedRequest::changes`] count when the dispatcher took the
    /// handler's snapshot.
    pub(crate) changes: u64,
}

/// What [`SuspendedRequest::park`] did with a request.
#[derive(Debug)]
pub(crate) enum Park {
    /// The request waits for the next change.
    Parked,
    /// The request waits for the next change, in place of the request parked
    /// before it. The response is that earlier request's "nothing changed"
    /// answer, for the caller to send.
    Displaced(Response),
    /// The server's state changed while the handler ran, so its "nothing
    /// changed" may be stale. The caller runs the request again now instead.
    RunAgain(Request),
}

/// The one request long polling holds open, if any, and a count of the state
/// changes that wake it.
#[derive(Debug, Default)]
pub(crate) struct SuspendedRequest {
    parked: Option<Suspended>,
    /// How many times [`Self::wake`] has run. A handler that ran across a wake
    /// computed its answer from an old snapshot, and parking it would wait for
    /// the change after the one it missed.
    changes: u64,
}

impl SuspendedRequest {
    /// The number of state changes so far, for [`Suspended::changes`].
    pub(crate) const fn changes(&self) -> u64 {
        self.changes
    }

    /// Parks `suspended` until the next state change, unless a change came while
    /// its handler ran. At most one request is parked, so a second one displaces
    /// the first, and the caller answers the first with its "nothing changed"
    /// result.
    pub(crate) fn park(
        &mut self,
        suspended: Suspended,
    ) -> Park {
        if suspended.changes != self.changes {
            return Park::RunAgain(suspended.request);
        }
        match self.parked.replace(suspended) {
            Some(displaced) => Park::Displaced(displaced.no_news),
            None => Park::Parked,
        }
    }

    /// Records a change to the server's state, and takes the parked request so
    /// that the caller runs it again.
    pub(crate) fn wake(&mut self) -> Option<Request> {
        self.changes += 1;
        self.parked.take().map(|parked| parked.request)
    }

    /// Drops the parked request if it has this id. The caller answers it.
    pub(crate) fn cancel(
        &mut self,
        id: &RequestId,
    ) {
        drop(self.parked.take_if(|parked| parked.request.id == *id));
    }

    /// Takes the "nothing changed" answer for the parked request, to answer it
    /// without running it again.
    pub(crate) fn give_up(&mut self) -> Option<Response> {
        self.parked.take().map(|parked| parked.no_news)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with this id, suspended by a handler that started after
    /// `changes` state changes.
    fn suspended(
        id: i32,
        changes: u64,
    ) -> Suspended {
        Suspended {
            request: Request::new(id.into(), "workspace/diagnostic".to_owned(), ()),
            no_news: Response::new_ok(id.into(), ()),
            changes,
        }
    }

    #[test]
    fn a_parked_request_runs_again_on_the_next_change() {
        let mut slot = SuspendedRequest::default();
        assert!(matches!(slot.park(suspended(1, 0)), Park::Parked));
        assert_eq!(slot.wake().map(|request| request.id), Some(1.into()));
        assert!(slot.wake().is_none());
        assert_eq!(slot.changes(), 2);
    }

    /// The handler started before a change, so the answer it suspended on may
    /// predate the change, and nothing would wake the parked request for it.
    #[test]
    fn a_request_that_ran_across_a_change_runs_again_at_once() {
        let mut slot = SuspendedRequest::default();
        let started_at = slot.changes();
        assert!(slot.wake().is_none());
        let Park::RunAgain(request) = slot.park(suspended(1, started_at)) else {
            panic!("expected the request to run again");
        };
        assert_eq!(request.id, 1.into());
        assert!(slot.give_up().is_none());
    }

    #[test]
    fn a_second_request_displaces_the_first() {
        let mut slot = SuspendedRequest::default();
        assert!(matches!(slot.park(suspended(1, 0)), Park::Parked));
        let Park::Displaced(answer) = slot.park(suspended(2, 0)) else {
            panic!("expected the first request to be displaced");
        };
        assert_eq!(answer.id, 1.into());
        assert_eq!(slot.wake().map(|request| request.id), Some(2.into()));
    }

    #[test]
    fn cancel_drops_only_the_request_with_that_id() {
        let mut slot = SuspendedRequest::default();
        assert!(matches!(slot.park(suspended(1, 0)), Park::Parked));
        slot.cancel(&2.into());
        assert_eq!(slot.give_up().map(|answer| answer.id), Some(1.into()));

        assert!(matches!(slot.park(suspended(1, 0)), Park::Parked));
        slot.cancel(&1.into());
        assert!(slot.give_up().is_none());
    }

    #[test]
    fn give_up_hands_back_the_nothing_changed_answer() {
        let mut slot = SuspendedRequest::default();
        assert!(matches!(slot.park(suspended(1, 0)), Park::Parked));
        let answer = slot.give_up().unwrap();
        assert_eq!(answer.id, 1.into());
        assert!(slot.wake().is_none());
    }
}
