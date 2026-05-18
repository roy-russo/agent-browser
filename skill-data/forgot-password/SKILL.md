---
name: forgot-password
description: Trigger a password-reset flow on a website using agent-browser. Use when the user needs to reset a forgotten password, request a reset email, click "forgot password", or initiate account recovery on a site. Triggers include "reset my password on X", "I forgot my password to Y", "send a reset email", "trigger password recovery", or any task requiring a password-reset request flow.
allowed-tools: Bash(agent-browser:*), Bash(npx agent-browser:*)
---

# Forgot Password Automation

Trigger a password-reset email on a website. The driver handles the request side only — the email arrives in the user's inbox and is read separately (or by a different driver).

## Prerequisites

Same cloned-profile caveat as the `login` skill: if attached to a Chrome on a cloned user-data-dir, run `agent-browser tab new` after `connect` before the first click. See the `login` skill for the full explanation.

```bash
agent-browser connect 9222
agent-browser tab new   # safe to call; no-op on non-cloned profiles
```

## Recipe

```bash
agent-browser open https://example.com/login

# Dismiss cookie consent (see login skill for vendor-specific selectors)
agent-browser eval 'document.querySelector("#onetrust-accept-btn-handler")?.click()'

# Find and click the "Forgot password" link. Wording varies — try the most
# likely phrasings in order. agent-browser find returns first match.
agent-browser find text "Forgot password" click \
  || agent-browser find text "Forgot your password" click \
  || agent-browser find text "I've forgotten my password" click \
  || agent-browser find text "Reset password" click \
  || agent-browser find text "Trouble signing in" click

# Some sites navigate to a new page; some open a modal; some inline
# expand the form. Re-snapshot to find the email input.
agent-browser snapshot -i

# Enter email and submit
agent-browser click 'input[type=email]'
agent-browser keyboard type 'user@example.com'
agent-browser click 'button[type=submit]'
```

## Verifying the reset was triggered

Most sites confirm with text like *"if that email is in our system, we'll send a reset link"*. **This response is the same whether the email exists or not** — it's account-enumeration prevention. Treat the on-page confirmation as "the request was accepted by the form," not "an email was sent to a real account."

```bash
agent-browser get text 'main, .reset-confirmation, body' | head -c 400

# Common confirmation phrases:
#   "Check your email"
#   "We've sent a link"
#   "If this email is in our system"
#   "Email sent"
#   "Reset instructions on the way"
```

The authoritative signal is the email arriving in the inbox, not the page text.

## Variations

### Multi-step (email → CAPTCHA → submit)

Some sites add a CAPTCHA between entering the email and submitting:

```bash
agent-browser snapshot -i | head -c 1500
# If output mentions "google.com/recaptcha" or "hcaptcha.com" — needs human assist
```

For Akamai/Cloudflare-protected reset flows, the same warm-cookie or human-in-the-loop pattern from the `login` skill applies.

### Modal vs. dedicated page

Some sites open the reset form in an overlay rather than navigating:

```bash
# Dedicated page: form selectors at root level
agent-browser click 'input[type=email]'

# Modal: form is inside a dialog/overlay element
agent-browser click '[role=dialog] input[type=email]'
```

When in doubt, `agent-browser get url` after clicking the link tells you whether the page navigated.

### Username-instead-of-email

Some sites accept a username (not just an email) for reset. The selector may be `input[name=username]` instead of `input[type=email]`. Snapshot to confirm.

### Magic-link / passwordless sites

Increasingly common: the "reset" flow is identical to the regular sign-in flow because the site sends a link instead of using a stored password. Driver-side flow shape is the same; only the email content differs.

### Multi-account flows

If the user has multiple accounts on the site, the reset form may show a chooser before the email field. Look for an account selector and pick by email or display name with `find text`.

## Failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| "Forgot password" link not found | Wording varies per locale or A/B variant | Try multiple phrasings via `find text "..." click`, or `snapshot -i` and click by ref |
| Submit returns 429 / rate-limit error | Too many reset requests for this email recently | Wait or use a different account; some sites cool off after 24h |
| Submit returns "email not recognised" explicitly | Site does NOT use enumeration-prevention; the email genuinely isn't on file | Verify the email with the user |
| Form rejects email format | MX-record validation or disposable-email blocklist | Use a real, primary email |
| CAPTCHA appears between email and submit | Site is behind Akamai/PerimeterX/hCaptcha | Human-in-the-loop or warm-cookie path |
| Page navigates back to login on submit | The "submit" button was a different button (e.g. "Sign in" still); selector clash | Re-snapshot, click the submit *inside the reset form* by ref |

## Combining with the email-reading flow

The full account-recovery loop is:

1. (this skill) trigger the reset email
2. read the email (separate flow — Gmail, IMAP, mail-client driver)
3. extract the reset link from the email body
4. `agent-browser open <reset-link>` and follow the new-password form
5. `keyboard type` the new password into both fields, submit

This skill covers step 1 only. Steps 2-5 are out of scope; combine with whatever inbox-driving tool the user has available.

## Related skills

- `login` — sign in once you have the (new) credential
- `core` — agent-browser fundamentals
