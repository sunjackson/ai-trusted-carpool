# Official API Pricing Sources

Pricing catalog last verified: **2026-07-14**.

## Sources

- OpenAI API pricing: <https://developers.openai.com/api/docs/pricing>
- Anthropic Claude pricing: <https://platform.claude.com/docs/en/about-claude/pricing>

Only first-party, standard API list prices are used. The UI reports an estimate in USD; it is not
an invoice, a settlement amount, or a conversion of ChatGPT, Codex, or Claude subscription usage.

## Calculation Rules

- Keep every model separate under each passenger seat.
- Count uncached input, output, cache reads, 5-minute cache writes, and 1-hour cache writes as
  distinct categories when the provider reports them.
- Apply GPT-5.6 long-context rates when a request exceeds 272,000 input tokens.
- Apply the Claude Sonnet 5 introductory rate through 2026-08-31 and the published standard rate
  from 2026-09-01. Historical requests are not repriced after the cutover.
- Do not guess. Unknown model IDs or requests containing an unpriced token category are marked
  “暂无官价” and excluded from the official-price total.

Regional processing, priority service, batch/flex discounts, data-residency uplifts, paid tools,
taxes, and provider-side credits are outside the current estimate unless the request path can prove
that modifier. The default estimate therefore represents standard, global, first-party API usage.
