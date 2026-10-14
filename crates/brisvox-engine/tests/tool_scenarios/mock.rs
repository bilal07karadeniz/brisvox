//! The scenarios against scripted providers: five shapes each (see `Shape`).

use crate::harness::{Provider, Shape};
use crate::scenarios as s;
use crate::wire::Turn;

async fn provider(shape: Shape, turns: Vec<Turn>) -> Provider {
    Provider::scripted(shape, &turns).await
}

/// One test per provider shape: the simulator's mock runs on a paused clock, the adapters on
/// loopback sockets in real time.
macro_rules! per_shape {
    ($scenario:ident, $script:expr, $run:expr) => {
        mod $scenario {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn plain() {
                $run(provider(Shape::Plain, $script).await).await;
            }

            #[tokio::test(start_paused = true)]
            async fn interleaved() {
                $run(provider(Shape::Interleaved, $script).await).await;
            }

            #[tokio::test]
            async fn openai_compatible() {
                $run(provider(Shape::OpenAi, $script).await).await;
            }

            #[tokio::test]
            async fn anthropic() {
                $run(provider(Shape::Anthropic, $script).await).await;
            }

            #[tokio::test]
            async fn gemini() {
                $run(provider(Shape::Gemini, $script).await).await;
            }
        }
    };
}

per_shape!(s1_parallel, s::s1_script(), s::s1);
per_shape!(s2_chain, s::s2_script(), s::s2);
per_shape!(s3_booking_repeated, s::s3_script(), s::s3);
per_shape!(s4_invalid_then_valid, s::s4_script(), s::s4);
per_shape!(s4b_invalid_forever, s::s4b_script(), s::s4b);
per_shape!(s5_slow_tool_filler, s::s5_script(), s::s5);
per_shape!(s5b_slow_parallel_one_filler, s::s5b_script(), s::s5b);
per_shape!(s6_barge_in, s::s6_script(), s::s6);

/// The live checks of the scenarios (loose assertions) run against the same scripts, so a bug
/// in them shows up here and not on the first run with keys.
macro_rules! loose {
    ($scenario:ident, $script:expr, $run:expr) => {
        mod $scenario {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn plain() {
                $run(provider(Shape::Plain, $script).await.loose()).await;
            }

            #[tokio::test]
            async fn openai_compatible() {
                $run(provider(Shape::OpenAi, $script).await.loose()).await;
            }

            #[tokio::test]
            async fn anthropic() {
                $run(provider(Shape::Anthropic, $script).await.loose()).await;
            }

            #[tokio::test]
            async fn gemini() {
                $run(provider(Shape::Gemini, $script).await.loose()).await;
            }
        }
    };
}

mod loose {
    use super::*;

    loose!(s1_parallel, s::s1_script(), s::s1);
    loose!(s2_chain, s::s2_script(), s::s2);
    loose!(s3_booking_repeated, s::s3_script(), s::s3);
    loose!(s4_invalid_then_valid, s::s4_script(), s::s4);
    loose!(s5_slow_tool_filler, s::s5_script(), s::s5);
    loose!(s6_barge_in, s::s6_script(), s::s6);
}

/// The speech-to-speech stage with a scripted provider (Gemini Live's events).
mod s2s_stage {
    use crate::s2s;

    #[tokio::test(start_paused = true)]
    async fn s1_three_lookups_in_one_response() {
        s2s::s1_scripted().await;
    }

    #[tokio::test(start_paused = true)]
    async fn s2_three_round_chain() {
        s2s::s2_scripted().await;
    }

    #[tokio::test(start_paused = true)]
    async fn s3_repeated_booking_runs_once() {
        s2s::s3_scripted().await;
    }
}

per_shape!(s4c_broken_calls_among_good_ones, s::s4c_script(), s::s4c);
per_shape!(s5c_tool_timeout, s::s5c_script(), s::s5c);
