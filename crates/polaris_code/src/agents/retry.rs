//! When and how long to wait before sending a failed model call again, and how
//! long to wait for one at all.
//!
//! [`RetryPolicy`] holds the numbers; this module holds the rules that read
//! them. The agent graph's retry stage asks [`RetryPolicy::next_step`] after
//! each failed attempt and routes on exactly what it answers, so the graph
//! itself carries no constants.
//!
//! A call can also hang: the Anthropic client sets no HTTP timeout, so a
//! connection that goes quiet would wait for ever. [`within`] and
//! [`guard_idle`] bound the wait for the response and the gaps between its
//! stream events, and report an expiry as the same transient
//! [`FailureKind::Timeout`](super::failure::FailureKind::Timeout) the retry
//! rules already handle.

use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use polaris::models::llm::{GenerationError, LlmStream, StreamEvent};

use super::failure::{classify, requested_wait, timed_out};
use super::policy::RetryPolicy;

mod model_call;

pub(crate) use model_call::{
    AttemptLedger, AttemptOutcome, abandon, accept_response, act, begin_model_call, decide_retry,
    report_failure, wait_to_retry,
};

/// Whether a failed attempt had already left something behind that a second
/// attempt would repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Commitment {
    /// Nothing reached the user and nothing was billed, so the call can be
    /// sent again as if the attempt had not happened.
    Uncommitted,
    /// Output was shown or tokens were billed. Neither can be rolled back, so
    /// a second attempt would show or bill the same thing twice.
    Committed,
}

impl Commitment {
    /// Whether the attempt was [`Committed`](Self::Committed).
    #[must_use]
    pub fn is_committed(self) -> bool {
        self == Self::Committed
    }
}

impl From<bool> for Commitment {
    /// `true` is [`Committed`](Self::Committed).
    fn from(committed: bool) -> Self {
        if committed {
            Self::Committed
        } else {
            Self::Uncommitted
        }
    }
}

/// What to do after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetryStep {
    /// Send the call again once `after` has passed.
    Retry {
        /// How long to wait first.
        after: Duration,
    },
    /// Stop and report the failure.
    GiveUp,
}

impl RetryPolicy {
    /// Decide what follows attempt number `attempt` (counting from 1), which
    /// failed with `error`.
    ///
    /// The call is sent again only if all of these hold:
    ///
    /// - the failure is transient: not an authentication or invalid-request
    ///   error, which a second try cannot fix;
    /// - the attempt had not shown anything to the user (`commitment` is
    ///   [`Uncommitted`](Commitment::Uncommitted)), because shown output and
    ///   billed tokens cannot be rolled back;
    /// - attempts remain under [`max_attempts`](Self::max_attempts);
    /// - the wait the provider asked for, if any, is no longer than
    ///   [`max_delay`](Self::max_delay).
    ///
    /// The wait is the provider's own request when it made one
    /// (the `retry_after` of [`GenerationError::RateLimited`]), and otherwise
    /// [`base_delay`](Self::base_delay) doubled for each earlier retry, scaled
    /// by a factor in `0.75..1.25` taken from `jitter` (a value in `0..1`) so
    /// that clients that failed together do not retry together, and capped at
    /// [`max_delay`](Self::max_delay).
    ///
    /// An `attempt` of 0 is read as the first attempt, and a zero
    /// [`base_delay`](Self::base_delay) retries at once.
    #[must_use]
    pub fn next_step(
        &self,
        attempt: u32,
        error: &GenerationError,
        commitment: Commitment,
        jitter: f64,
    ) -> RetryStep {
        let attempt = attempt.max(1);
        if commitment.is_committed()
            || attempt >= self.max_attempts
            || !classify(error).is_transient()
        {
            return RetryStep::GiveUp;
        }
        match requested_wait(error) {
            Some(requested) if requested > self.max_delay => RetryStep::GiveUp,
            Some(requested) => RetryStep::Retry { after: requested },
            None => RetryStep::Retry {
                after: self.backoff(attempt, jitter),
            },
        }
    }

    /// The longest a call can go on before this policy gives up: every
    /// attempt silent for [`attempt_timeout`](Self::attempt_timeout), and every
    /// wait between attempts as long as [`max_delay`](Self::max_delay).
    ///
    /// A `max_attempts` of 0 is read as 1, as in [`next_step`](Self::next_step).
    #[must_use]
    pub fn worst_case_wait(&self) -> Duration {
        let attempts = self.max_attempts.max(1);
        self.attempt_timeout
            .saturating_mul(attempts)
            .saturating_add(self.max_delay.saturating_mul(attempts - 1))
    }

    /// The computed wait after attempt number `attempt` failed.
    fn backoff(&self, attempt: u32, jitter: f64) -> Duration {
        let doublings = attempt.saturating_sub(1);
        let factor = 1u32.checked_shl(doublings).unwrap_or(u32::MAX);
        let exponential = self.base_delay.saturating_mul(factor).min(self.max_delay);
        exponential
            .mul_f64(0.75 + 0.5 * jitter.clamp(0.0, 1.0))
            .min(self.max_delay)
    }
}

/// A value in `0..1` that differs from call to call.
///
/// Used only to spread out retries, so it needs to be unpredictable rather
/// than strong. The standard library seeds every [`RandomState`] from the
/// operating system, which saves a dependency on a random-number crate.
#[must_use]
pub fn jitter() -> f64 {
    let bits = RandomState::new().hash_one(0u8) >> 11;
    // 53 bits fill an `f64` mantissa exactly.
    bits as f64 / (1u64 << 53) as f64
}

/// Await `call`, giving up with a timeout error if it takes longer than
/// `limit`.
///
/// Wraps the part of a request before any stream event exists: opening the
/// connection and receiving the response status.
///
/// # Errors
///
/// Returns the call's own error, or a timeout error once `limit` passes.
pub async fn within<T>(
    limit: Duration,
    call: impl Future<Output = Result<T, GenerationError>>,
) -> Result<T, GenerationError> {
    tokio::time::timeout(limit, call)
        .await
        .unwrap_or_else(|_| Err(timed_out(limit)))
}

/// Pass `stream` through, but end it with a timeout error if `limit` passes
/// without its next event.
///
/// The limit applies to every gap, so a long response that keeps producing
/// events never trips it, while one that goes silent does. The stream ends
/// after the error.
pub fn guard_idle(
    stream: LlmStream,
    limit: Duration,
) -> impl Stream<Item = Result<StreamEvent, GenerationError>> + Send {
    futures_util::stream::unfold(Some(stream), move |state| async move {
        let mut stream = state?;
        match tokio::time::timeout(limit, stream.next()).await {
            Ok(Some(event)) => Some((event, Some(stream))),
            Ok(None) => None,
            Err(_) => Some((Err(timed_out(limit)), None)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::failure::FailureKind;
    use polaris::models::llm::StopReason;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            attempt_timeout: Duration::from_secs(300),
        }
    }

    fn provider(status: u16) -> GenerationError {
        GenerationError::Provider {
            status: Some(status),
            message: "{}".into(),
            source: None,
        }
    }

    fn rate_limited(seconds: Option<u64>) -> GenerationError {
        GenerationError::RateLimited {
            retry_after: seconds.map(Duration::from_secs),
        }
    }

    #[test]
    fn an_auth_failure_is_never_retried() {
        for error in [GenerationError::Auth("bad key".into()), provider(401)] {
            assert_eq!(
                policy().next_step(1, &error, Commitment::Uncommitted, 0.5),
                RetryStep::GiveUp,
                "{error}"
            );
        }
    }

    #[test]
    fn a_rejected_request_is_never_retried() {
        for error in [
            GenerationError::InvalidRequest("too long".into()),
            provider(400),
            provider(404),
            provider(422),
        ] {
            assert_eq!(
                policy().next_step(1, &error, Commitment::Uncommitted, 0.5),
                RetryStep::GiveUp,
                "{error}"
            );
        }
        assert_eq!(
            policy().next_step(
                1,
                &GenerationError::Refusal("no".into()),
                Commitment::Uncommitted,
                0.5
            ),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn transient_failures_are_retried() {
        for error in [
            GenerationError::Http("connection reset".into()),
            rate_limited(None),
            provider(429),
            provider(500),
            provider(502),
            provider(503),
            provider(529),
            timed_out(Duration::from_secs(1)),
        ] {
            assert!(
                matches!(
                    policy().next_step(1, &error, Commitment::Uncommitted, 0.5),
                    RetryStep::Retry { .. }
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn the_wait_the_provider_asked_for_is_the_wait() {
        let step = policy().next_step(1, &rate_limited(Some(7)), Commitment::Uncommitted, 0.0);
        assert_eq!(
            step,
            RetryStep::Retry {
                after: Duration::from_secs(7)
            }
        );
        // Jitter does not stretch or shrink a requested wait, and neither does
        // how many attempts came before.
        assert_eq!(
            policy().next_step(4, &rate_limited(Some(7)), Commitment::Uncommitted, 1.0),
            step
        );
    }

    #[test]
    fn a_wait_longer_than_the_cap_ends_the_retrying() {
        assert_eq!(
            policy().next_step(1, &rate_limited(Some(61)), Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
        assert!(matches!(
            policy().next_step(1, &rate_limited(Some(60)), Commitment::Uncommitted, 0.5),
            RetryStep::Retry { .. }
        ));
    }

    #[test]
    fn the_last_attempt_gives_up() {
        let error = provider(529);
        assert!(matches!(
            policy().next_step(4, &error, Commitment::Uncommitted, 0.5),
            RetryStep::Retry { .. }
        ));
        assert_eq!(
            policy().next_step(5, &error, Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
        assert_eq!(
            policy().next_step(9, &error, Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn one_attempt_means_no_retrying() {
        let policy = RetryPolicy {
            max_attempts: 1,
            ..policy()
        };
        assert_eq!(
            policy.next_step(1, &provider(529), Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
        // A zero is read as one rather than as "retry for ever".
        let zero = RetryPolicy {
            max_attempts: 0,
            ..policy
        };
        assert_eq!(
            zero.next_step(1, &provider(529), Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn an_attempt_that_showed_output_is_not_retried() {
        assert_eq!(
            policy().next_step(1, &provider(529), Commitment::Committed, 0.5),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn the_computed_wait_doubles_and_stays_within_the_jitter_band() {
        let wait = |attempt, jitter| match policy().next_step(
            attempt,
            &provider(529),
            Commitment::Uncommitted,
            jitter,
        ) {
            RetryStep::Retry { after } => after,
            RetryStep::GiveUp => panic!("attempt {attempt} should retry"),
        };
        // Mid-point jitter is the plain exponential.
        assert_eq!(wait(1, 0.5), Duration::from_secs(1));
        assert_eq!(wait(2, 0.5), Duration::from_secs(2));
        assert_eq!(wait(3, 0.5), Duration::from_secs(4));
        assert_eq!(wait(4, 0.5), Duration::from_secs(8));
        // The band is plus or minus a quarter.
        assert_eq!(wait(3, 0.0), Duration::from_secs(3));
        assert_eq!(wait(3, 1.0), Duration::from_secs(5));
        // Out-of-range jitter is clamped, not extrapolated.
        assert_eq!(wait(3, -4.0), wait(3, 0.0));
        assert_eq!(wait(3, 9.0), wait(3, 1.0));
    }

    #[test]
    fn the_computed_wait_never_exceeds_the_cap() {
        let policy = RetryPolicy {
            max_attempts: 100,
            ..policy()
        };
        for attempt in [7, 30, 31, 32, 33, 99] {
            let RetryStep::Retry { after } =
                policy.next_step(attempt, &provider(529), Commitment::Uncommitted, 1.0)
            else {
                panic!("attempt {attempt} should retry");
            };
            assert_eq!(after, policy.max_delay, "attempt {attempt}");
        }
    }

    #[test]
    fn attempt_zero_is_read_as_the_first_attempt() {
        let error = provider(529);
        for jitter in [0.0, 0.5, 1.0] {
            assert_eq!(
                policy().next_step(0, &error, Commitment::Uncommitted, jitter),
                policy().next_step(1, &error, Commitment::Uncommitted, jitter),
                "jitter {jitter}"
            );
        }
        // And it is not a way past the attempt budget.
        let once = RetryPolicy {
            max_attempts: 1,
            ..policy()
        };
        assert_eq!(
            once.next_step(0, &error, Commitment::Uncommitted, 0.5),
            RetryStep::GiveUp
        );
    }

    #[test]
    fn a_zero_base_delay_retries_at_once_at_every_attempt() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..policy()
        };
        for attempt in 1..policy.max_attempts {
            assert_eq!(
                policy.next_step(attempt, &provider(529), Commitment::Uncommitted, 0.5),
                RetryStep::Retry {
                    after: Duration::ZERO
                },
                "attempt {attempt}"
            );
        }
        // A wait the provider asked for is still honoured.
        assert_eq!(
            policy.next_step(1, &rate_limited(Some(3)), Commitment::Uncommitted, 0.5),
            RetryStep::Retry {
                after: Duration::from_secs(3)
            }
        );
    }

    #[test]
    fn the_default_policy_is_five_attempts_and_bounded_at_twenty_nine_minutes() {
        let defaults = RetryPolicy::default();
        assert_eq!(defaults.max_attempts, 5);
        assert_eq!(defaults.base_delay, Duration::from_secs(1));
        assert_eq!(defaults.max_delay, Duration::from_secs(60));
        assert_eq!(defaults.attempt_timeout, Duration::from_secs(300));
        // Five silent attempts of 300 s and four waits of 60 s.
        assert_eq!(defaults.worst_case_wait(), Duration::from_secs(1_740));
        // A single attempt has no waits, and a zero budget is one attempt.
        let single = RetryPolicy {
            max_attempts: 0,
            ..defaults
        };
        assert_eq!(single.worst_case_wait(), Duration::from_secs(300));
    }

    #[test]
    fn a_commitment_reads_from_a_flag() {
        assert_eq!(Commitment::from(true), Commitment::Committed);
        assert_eq!(Commitment::from(false), Commitment::Uncommitted);
        assert!(Commitment::Committed.is_committed());
        assert!(!Commitment::Uncommitted.is_committed());
    }

    #[test]
    fn jitter_stays_in_the_unit_interval_and_varies() {
        let samples: Vec<f64> = (0..64).map(|_| jitter()).collect();
        assert!(samples.iter().all(|value| (0.0..1.0).contains(value)));
        assert!(
            samples.windows(2).any(|pair| pair[0] != pair[1]),
            "64 identical samples: {samples:?}"
        );
    }

    fn message_stop() -> StreamEvent {
        StreamEvent::MessageStop {
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            id: None,
            model: None,
        }
    }

    #[tokio::test]
    async fn a_prompt_stream_passes_through_untouched() {
        let inner: LlmStream = futures_util::stream::iter(vec![Ok(message_stop())]).boxed();
        let events: Vec<_> = guard_idle(inner, Duration::from_secs(5)).collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Ok(StreamEvent::MessageStop { .. })));
    }

    #[tokio::test]
    async fn a_stream_that_goes_quiet_ends_with_a_timeout_error() {
        // One event, then silence for ever.
        let inner: LlmStream = futures_util::stream::iter(vec![Ok(message_stop())])
            .chain(futures_util::stream::pending())
            .boxed();
        let mut guarded = std::pin::pin!(guard_idle(inner, Duration::from_millis(40)));

        assert!(matches!(
            guarded.next().await,
            Some(Ok(StreamEvent::MessageStop { .. }))
        ));
        let started = std::time::Instant::now();
        let error = guarded
            .next()
            .await
            .expect("the silence is reported")
            .expect_err("as an error");
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert_eq!(classify(&error), FailureKind::Timeout);
        assert!(guarded.next().await.is_none(), "the stream ends after it");
    }

    #[tokio::test]
    async fn the_idle_limit_restarts_with_every_event() {
        // Six events 100 ms apart take 600 ms in all: longer than the 500 ms
        // limit, but no single gap is, and each gap leaves four fifths of the
        // limit to spare for a slow host.
        let inner: LlmStream = futures_util::stream::unfold(0u8, |sent| async move {
            if sent == 6 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Some((Ok(message_stop()), sent + 1))
        })
        .boxed();
        let events: Vec<_> = guard_idle(inner, Duration::from_millis(500))
            .collect()
            .await;
        assert_eq!(events.len(), 6);
        assert!(events.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn a_call_that_never_answers_times_out() {
        let error = within(
            Duration::from_millis(30),
            std::future::pending::<Result<(), GenerationError>>(),
        )
        .await
        .expect_err("it never answers");
        assert_eq!(classify(&error), FailureKind::Timeout);
    }

    #[tokio::test]
    async fn a_call_that_answers_in_time_keeps_its_own_result() {
        assert_eq!(
            within(Duration::from_secs(5), async {
                Ok::<_, GenerationError>(7)
            })
            .await
            .unwrap(),
            7
        );
        let error = within(Duration::from_secs(5), async {
            Err::<(), _>(GenerationError::Auth("bad".into()))
        })
        .await
        .expect_err("its own failure");
        assert_eq!(classify(&error), FailureKind::Auth);
    }
}
