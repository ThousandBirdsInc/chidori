/// Rough cost estimation for LLM calls.
///
/// Prices are USD per 1M tokens and matched by model-name prefix. They are
/// deliberately approximate — the goal is a ballpark figure for the trace
/// output and the `stats` command, not a billing-grade number. Update the
/// table as new models ship.

#[derive(Debug, Clone, Copy)]
struct Pricing {
    prefix: &'static str,
    input_per_mtok: f64,
    output_per_mtok: f64,
}

const PRICING: &[Pricing] = &[
    // Anthropic
    Pricing {
        prefix: "claude-opus-4",
        input_per_mtok: 15.00,
        output_per_mtok: 75.00,
    },
    Pricing {
        prefix: "claude-sonnet-4",
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
    },
    Pricing {
        prefix: "claude-haiku-4",
        input_per_mtok: 0.80,
        output_per_mtok: 4.00,
    },
    Pricing {
        prefix: "claude-3-5-sonnet",
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
    },
    Pricing {
        prefix: "claude-3-5-haiku",
        input_per_mtok: 0.80,
        output_per_mtok: 4.00,
    },
    Pricing {
        prefix: "claude-3-opus",
        input_per_mtok: 15.00,
        output_per_mtok: 75.00,
    },
    Pricing {
        prefix: "claude",
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
    },
    // OpenAI
    Pricing {
        prefix: "gpt-4o-mini",
        input_per_mtok: 0.15,
        output_per_mtok: 0.60,
    },
    Pricing {
        prefix: "gpt-4o",
        input_per_mtok: 2.50,
        output_per_mtok: 10.00,
    },
    Pricing {
        prefix: "gpt-4.1",
        input_per_mtok: 2.00,
        output_per_mtok: 8.00,
    },
    Pricing {
        prefix: "gpt-4",
        input_per_mtok: 30.00,
        output_per_mtok: 60.00,
    },
    Pricing {
        prefix: "o3-mini",
        input_per_mtok: 1.10,
        output_per_mtok: 4.40,
    },
    Pricing {
        prefix: "o3",
        input_per_mtok: 15.00,
        output_per_mtok: 60.00,
    },
    Pricing {
        prefix: "o1-mini",
        input_per_mtok: 3.00,
        output_per_mtok: 12.00,
    },
    Pricing {
        prefix: "o1",
        input_per_mtok: 15.00,
        output_per_mtok: 60.00,
    },
    Pricing {
        prefix: "gpt",
        input_per_mtok: 2.50,
        output_per_mtok: 10.00,
    },
];

/// Prompt-cache price multipliers relative to the base input rate. Anthropic
/// bills cache writes at 1.25x and reads at 0.1x base input; OpenAI has no
/// separate write charge and bills cached reads at 0.5x. Matched by model
/// prefix like the base table.
fn cache_multipliers(model: &str) -> (f64, f64) {
    if model.starts_with("claude") {
        (1.25, 0.10)
    } else {
        (1.0, 0.50)
    }
}

/// Estimate USD cost for a single LLM call. Returns 0.0 for unknown models.
pub fn estimate_cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    estimate_cost_usd_with_cache(model, input_tokens, output_tokens, 0, 0)
}

/// Estimate USD cost for a single LLM call, pricing prompt-cache writes and
/// reads at their own rates. `input_tokens` is the fresh (non-cached) share.
pub fn estimate_cost_usd_with_cache(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> f64 {
    let Some(p) = PRICING.iter().find(|p| model.starts_with(p.prefix)) else {
        return 0.0;
    };
    let (write_mult, read_mult) = cache_multipliers(model);
    let input = (input_tokens as f64 / 1_000_000.0) * p.input_per_mtok;
    let cache_write = (cache_creation_tokens as f64 / 1_000_000.0) * p.input_per_mtok * write_mult;
    let cache_read = (cache_read_tokens as f64 / 1_000_000.0) * p.input_per_mtok * read_mult;
    let output = (output_tokens as f64 / 1_000_000.0) * p.output_per_mtok;
    input + cache_write + cache_read + output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sonnet_cost() {
        // 1M input + 1M output on claude-sonnet-4-6 ≈ $3 + $15 = $18
        let cost = estimate_cost_usd("claude-sonnet-4-6", 1_000_000, 1_000_000);
        assert!((cost - 18.0).abs() < 0.001);
    }

    #[test]
    fn test_unknown_model_zero() {
        assert_eq!(estimate_cost_usd("unknown-model", 1000, 1000), 0.0);
    }

    #[test]
    fn test_cache_tokens_price_at_documented_multiples() {
        // claude-sonnet base input is $3/MTok: 1M cache-write ≈ $3.75 (1.25x),
        // 1M cache-read ≈ $0.30 (0.1x).
        let write = estimate_cost_usd_with_cache("claude-sonnet-4-6", 0, 0, 1_000_000, 0);
        assert!((write - 3.75).abs() < 0.001, "write cost was {write}");
        let read = estimate_cost_usd_with_cache("claude-sonnet-4-6", 0, 0, 0, 1_000_000);
        assert!((read - 0.30).abs() < 0.001, "read cost was {read}");
        // gpt-4o input is $2.50/MTok; cached reads bill at 0.5x ≈ $1.25.
        let openai_read = estimate_cost_usd_with_cache("gpt-4o", 0, 0, 0, 1_000_000);
        assert!(
            (openai_read - 1.25).abs() < 0.001,
            "openai read cost was {openai_read}"
        );
    }
}
