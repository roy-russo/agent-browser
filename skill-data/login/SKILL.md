---
name: login
description: Log into websites using agent-browser, with or without saved passwords. Use when the user needs to sign in to a site, authenticate, automate a login flow, or drive Chrome's autofill picker to commit a saved credential. Triggers include "log in to X", "sign in to my account", "autofill the password", "log into this site", or any task requiring authenticated access to a website.
allowed-tools: Bash(agent-browser:*), Bash(npx agent-browser:*)
---

# Website Login Automation

Drive a website's login flow via agent-browser attached to a real Chrome over CDP. Two paths depending on whether the target Chrome profile already has the credential saved.

## Cloned-profile prerequisite (read this first)

If your Chrome was launched on a **cloned** user-data-dir (e.g. `cp -Rp ~/Library/Application\ Support/Google/Chrome/Profile\ 1 /tmp/chrome-test-userdir/` then launched with `--user-data-dir=/tmp/chrome-test-userdir`), Chrome refuses to invoke the autofill picker on CDP-dispatched clicks against any tab that existed at startup. The field will silently auto-fill instead and the picker never shows.

Fix: open one fresh tab right after `connect`. The `Target.createTarget`'d tab accepts CDP clicks normally and unsticks the rest of the browser session.

```bash
agent-browser connect 9222
agent-browser tab new        # ← unsticks autofill picker for cloned profiles
```

This is a no-op on profiles set up fresh in the dev Chrome itself. Always safe to call. The unstucked state is **not always durable** across cross-domain navigations on the same tab — for long-running flows, run `tab new` again before the first click on each new domain.

## Which path

| State | Path |
|---|---|
| Profile has a saved credential for this site | **Autofill path** — driver never reads or types the password; Chrome's password manager commits it on submit. Lower bot-detection footprint. |
| No saved credential, or you must inject a new one | **Type path** — `agent-browser keyboard type` for per-char trusted keystrokes. |

## Autofill path (saved credential)

Validated on argos.co.uk and account.bbc.com:

```bash
agent-browser connect 9222
agent-browser tab new
agent-browser open https://www.argos.co.uk/login

# Dismiss cookie consent (selector varies — see Cookie banners below)
agent-browser eval 'document.querySelector("#onetrust-accept-btn-handler")?.click()'

# Click the username/email field. The picker should appear in ax.popups[0].
agent-browser click 'input[type=email]'

# Select first row + commit. --raw is required: Chrome's password UI
# filters non-rawKeyDown events. Plain `press Enter` flows to the page
# (page-scroll proves it) but the picker handler ignores it.
agent-browser press ArrowDown --raw
agent-browser press Enter --raw

# Submit. The submit click is what commits Chrome's autofill into the
# DOM values — until trusted user-activation, the autofill is held in
# preview state and `eval` reads value === ''.
agent-browser click 'button[type=submit]'
```

### Verification

After the field click:

```bash
agent-browser ax-snapshot
# popups: [{ desc: "Autofill", items: [{ text: "Password for ..." }, ...] }]
# popups: []  → picker did not appear (likely cloned-profile issue or no saved cred)
```

After Enter (commit):

```bash
agent-browser eval '(() => {
  const e = document.querySelector("input[type=email]");
  return JSON.stringify({
    valLen: e.value.length,
    selected: e.matches(":-internal-autofill-selected")
  });
})()'
# selected: true and valLen > 0  →  credential committed
```

After submit:

```bash
agent-browser get url     # should change away from /login
agent-browser get title   # should change away from "Sign in"
```

## Type path (no saved credential)

```bash
agent-browser connect 9222
agent-browser open https://example.com/login

agent-browser click 'input[type=email]'
agent-browser keyboard type 'user@example.com'

agent-browser click 'input[type=password]'
agent-browser keyboard type 'PASSWORD_HERE'

agent-browser click 'button[type=submit]'
```

**Avoid `agent-browser fill`.** It dispatches a single `input` event without per-char keys and measures `isTrusted: false`. It will not pass Akamai-class behavioural scoring and may not register on React-controlled inputs. Use `keyboard type` for credential injection.

**Avoid `keyboard inserttext`** for credentials too — it inserts text without key events at all.

## Cookie banners

Most EU sites gate page interaction behind a consent banner. Common selectors:

| Vendor | Accept selector |
|---|---|
| OneTrust | `#onetrust-accept-btn-handler` |
| Cookiebot | `#CybotCookiebotDialogBodyButtonAccept` |
| Quantcast Choice | `.qc-cmp2-summary-buttons [mode="primary"]` |
| Didomi | `#didomi-notice-agree-button` |
| TrustArc | `.truste-button1` |
| Generic | `agent-browser find text "Accept" click` |

If the banner is rendered in an iframe, click via `eval` against the outer page first; if that fails, switch frames with the snapshot tooling.

## MFA / two-step flows

After submit, check for an OTP step:

```bash
agent-browser snapshot -i
agent-browser get text 'main, form, body' | head -c 500
# Look for: input[autocomplete="one-time-code"], "verification code", "2FA",
# 6-digit OTP-shaped fields
```

If an OTP code is required and you don't have the secret, this is human-in-the-loop only — there's no programmatic path through TOTP/SMS without the seed.

For TOTP where the agent does have the seed, generate the code outside agent-browser, then:

```bash
agent-browser click 'input[autocomplete="one-time-code"]'
agent-browser keyboard type '123456'
agent-browser click 'button[type=submit]'
```

## CAPTCHA / WAF challenges

If the site is behind Akamai / Cloudflare Turnstile / PerimeterX / hCaptcha, expect a challenge before submit succeeds. No current local-MCP browser passes Akamai cold. Two paths:

1. **Warm-cookie**: have the human pass the challenge once in their own browser pane, then attach AB to that profile or copy the validated cookies via `agent-browser cookies set --curl path/to/curl.txt`.
2. **Human-in-the-loop**: open the page, let the human click the challenge in the visible Chrome window, then resume AB.

Don't burn cycles trying to fake `isTrusted` events — Akamai's behavioural scorer is engine-enforced. CDP clicks are already `isTrusted: true`; that alone isn't enough.

## Common failure modes

| Symptom | Likely cause | Fix |
|---|---|---|
| Field click returns `popups: []` and field silent-fills | Cloned profile not unstucked | `agent-browser tab new`, retry click on new tab |
| Picker appears but `Enter` does nothing | Used `press Enter` instead of `press Enter --raw` | Add `--raw` |
| Picker appears but ArrowDown does nothing | Same — needs `--raw` | Add `--raw` |
| `value === ''` after Enter, picker dismissed | Chrome holds autofill in preview until submit | Don't read pre-submit; click submit then read |
| `fill` set value but submit rejected with WAF challenge | `isTrusted: false` event flagged | Switch to `keyboard type` per-char |
| Cookie banner blocks form interaction | Banner overlay capturing clicks | Dismiss banner first; verify with `agent-browser is visible '#consent-overlay'` |
| Submit lands back on `/login` with no error | Wrong field selectors, or React state wasn't updated | Re-snapshot, verify the submit handler ran (check console) |

## Sites with awkward login flows

Some patterns need light extra handling:

- **Username-then-password two-step** (Google, BBC, Microsoft): the form has only a username field initially; password field appears after submit. Click submit between the two `keyboard type` calls and `agent-browser wait 'input[type=password]'` for the second field.
- **Modal login** (sites where `/login` opens an overlay rather than navigating): selectors live inside the modal — prefix with `[role=dialog]` or use snapshot refs.
- **OAuth redirects** (sign in with Google, etc.): the flow exits to a different origin and back. Check final URL after redirects settle.

## Related skills

- `core` — agent-browser fundamentals (snapshot, refs, find)
- `forgot-password` — trigger a password-reset flow when the user can't sign in
