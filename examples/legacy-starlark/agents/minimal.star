# minimal.star — the five-minute onboarding fixture.
#
# Small enough to read in 60 seconds and see how every Starlark
# construct maps onto the canvas: a helper def, a branch, a host
# call (prompt), a return dict. If kitchen_sink is "every feature,"
# this is "the shape of a real agent."

config(
    model = "claude-sonnet-4-6",
    temperature = 0.3,
)

GREETING = "Hello"

def classify(score):
    if score >= 90:
        return "excellent"
    elif score >= 70:
        return "good"
    else:
        return "needs_work"

def agent(name = "world", score = 85):
    salutation = GREETING + ", " + name
    rating = classify(score)

    summary = prompt(
        "Write a one-sentence encouragement for someone rated " + rating,
    )

    log("minimal agent ran", name = name, rating = rating)

    return {
        "salutation": salutation,
        "rating": rating,
        "summary": summary,
    }
