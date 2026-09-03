use crate::UpstreamErrorKind;

/// Upper bound on the latency a single attempt may contribute to a deployment's
/// EWMA.
///
/// A timeout's wall-clock cost is bounded only by the caller's request timeout,
/// which an operator may set to hundreds of seconds for a slow provider. Without
/// a cap, one hang would dominate the average and flatten the axis for every
/// other sample. Two minutes sits above any realistic per-attempt deadline, so a
/// normal timeout is counted in full and only genuine outliers are clipped.
pub const LATENCY_SAMPLE_CAP_MILLIS: u64 = 120_000;

/// The EWMA sample an attempt earns, or `None` when its outcome says nothing
/// about how fast the deployment is.
///
/// Only two outcomes are evidence of speed: a success, and a timeout that
/// consumed its whole budget. Every other failure is evidence of *brokenness*
/// and its latency is an artifact of how quickly the upstream gave up — a 401
/// that returns in 8 ms, or a load balancer's 502 in 5 ms, would otherwise pull
/// the average down and make the most broken deployment look like the fastest
/// one to [`crate::DeploymentPicker::LowestLatency`].
///
/// `cap_millis == 0` disables capping.
#[must_use]
pub const fn latency_sample_millis(
    outcome: Result<(), UpstreamErrorKind>,
    latency_millis: u64,
    cap_millis: u64,
) -> Option<u64> {
    match outcome {
        Ok(()) | Err(UpstreamErrorKind::Timeout) => {
            if cap_millis == 0 || latency_millis < cap_millis {
                Some(latency_millis)
            } else {
                Some(cap_millis)
            }
        }
        Err(
            UpstreamErrorKind::Transport
            | UpstreamErrorKind::RateLimited
            | UpstreamErrorKind::ServerError
            | UpstreamErrorKind::ProviderUnavailable
            | UpstreamErrorKind::Unauthorized
            | UpstreamErrorKind::NotFound
            | UpstreamErrorKind::BadRequest,
        ) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind, at three points around the cap. The matrix is exhaustive on
    /// purpose: adding an `UpstreamErrorKind` variant must force a decision
    /// about whether it is a speed signal, not silently inherit one.
    #[test]
    fn only_success_and_timeout_are_speed_samples() {
        const CAP: u64 = 120_000;
        let excluded = [
            UpstreamErrorKind::Transport,
            UpstreamErrorKind::RateLimited,
            UpstreamErrorKind::ServerError,
            UpstreamErrorKind::ProviderUnavailable,
            UpstreamErrorKind::Unauthorized,
            UpstreamErrorKind::NotFound,
            UpstreamErrorKind::BadRequest,
        ];
        for kind in excluded {
            for latency in [5, CAP - 1, CAP, CAP + 1] {
                assert_eq!(
                    latency_sample_millis(Err(kind), latency, CAP),
                    None,
                    "{kind:?} at {latency}ms must not be a speed sample"
                );
            }
        }
        assert_eq!(latency_sample_millis(Ok(()), 50, CAP), Some(50));
        assert_eq!(
            latency_sample_millis(Err(UpstreamErrorKind::Timeout), 50, CAP),
            Some(50)
        );
    }

    #[test]
    fn timeout_latency_is_capped_not_dropped() {
        const CAP: u64 = 120_000;
        assert_eq!(
            latency_sample_millis(Err(UpstreamErrorKind::Timeout), CAP - 1, CAP),
            Some(CAP - 1)
        );
        assert_eq!(
            latency_sample_millis(Err(UpstreamErrorKind::Timeout), CAP, CAP),
            Some(CAP)
        );
        // A 20-minute hang contributes exactly the cap, not 20 minutes.
        assert_eq!(
            latency_sample_millis(Err(UpstreamErrorKind::Timeout), 1_200_000, CAP),
            Some(CAP)
        );
        assert_eq!(latency_sample_millis(Ok(()), u64::MAX, CAP), Some(CAP));
    }

    #[test]
    fn zero_cap_disables_capping() {
        assert_eq!(
            latency_sample_millis(Err(UpstreamErrorKind::Timeout), 1_200_000, 0),
            Some(1_200_000)
        );
        assert_eq!(latency_sample_millis(Ok(()), 1_200_000, 0), Some(1_200_000));
    }
}
