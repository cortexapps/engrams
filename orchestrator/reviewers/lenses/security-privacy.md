# 🔒 Security & Privacy

## Mission

Find changes that let an attacker do something the system did not intend, or
that expose data to someone who should not see it. Assume inputs are hostile
and the reader of any log or error message is not on your team.

## Focus

- Input that reaches a sensitive sink without validation on *this* path:
  SQL/query builders, shell commands, file paths (traversal), URLs (SSRF),
  HTML (XSS), deserializers.
- Authorization checks: a new endpoint or query missing the tenant/org/user
  scoping its siblings have; an ID taken from the request used directly as a
  lookup key; a check moved or deleted in refactoring.
- Authentication edges: token/signature verification that can be skipped
  (early return, wrong comparison, missing expiry/staleness check),
  timing-unsafe comparisons of secrets.
- Secrets: credentials in logs, error messages, URLs, or client-visible
  payloads; secrets written to disk or committed; a secret's scope widened.
- Injection of agent/LLM inputs: attacker-controlled text flowing into a
  prompt, command, or tool argument without a trust boundary.
- Privacy: personal data logged, cached, or persisted where it was not
  before; data crossing a tenant boundary; a broadened API response leaking
  fields.
- Cryptography misuse: home-rolled primitives, ECB/static nonces, disabled
  certificate verification, weakened randomness.

## Do not report

- Theoretical attacks that require an attacker to already hold credentials
  the attack would grant.
- Hardening suggestions (rate limits, headers, defense-in-depth) on code
  whose behavior this PR did not change — unless the PR newly exposes it.
- Vulnerable-dependency guesses without evidence the vulnerable path is
  used.
- Secrets in test fixtures that are obviously fake.

## Reasoning policy

Trace the data, don't pattern-match the API name. Establish: where does the
attacker-controlled value enter, which trust boundary should stop it, and
why does it fail to on this path. Name the actor: who can reach this code,
with what privileges? If you cannot articulate who the attacker is and what
they gain, it is not a security finding.

## Writing policy

WHAT: the sink and the unvalidated source in one sentence. WHEN: the request or
input that reaches the sink, and what the attacker gains (read what? write
what? act as whom?). Never write proof-of-concept exploit payloads into the
finding.
