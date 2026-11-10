# Sign-in, the session screens and the session's connection.

## Start-up and offline

starting = Starting…
starting-connecting-to = Connecting to { $instance }…
offline-title = Offline: { $instance }
offline-retry-in = You are still signed in. Inlet tries again in { $seconds } s.
offline-retry-now = Retry now
about-link = About Inlet and licences

## Sign-in

sign-in-instance-label = Instance
sign-in-instance-placeholder = fluxer.opland.net
sign-in-token-label = Token
sign-in-token-placeholder = Paste your token
sign-in-mode-browser = Sign in with browser
sign-in-mode-token = Paste token
sign-in-browser-hint = Inlet opens the instance's sign-in page in your browser. Sign in there (passkeys, SSO and email codes all work) and enter the code Inlet shows.
sign-in-token-hint = The token is kept in your system keychain, never in a file. Inlet clears it from the clipboard after sign-in.
sign-in-open-page = Open the sign-in page
sign-in-submit = Sign in
signing-in = Signing in…
sign-in-enter-instance = Enter the address of a Fluxer instance.
sign-in-paste-first = Paste a token first.
sign-in-handoff-instructions = Your browser should now show the sign-in page. Sign in there, then enter this code:
sign-in-waiting = Waiting for the browser…
sign-in-reopen = Open the page again
sign-in-handoff-not-https = The instance gave a sign-in page that is not https. Inlet did not open it.
sign-in-denied = The sign-in was denied in the browser.
sign-in-code-expired = The code expired. Try again.

## Confirming a new sign-in

confirm-signed-in-as = You signed in as:
confirm-stored-after-continue = Inlet stores the session in your system keychain only after you continue.
confirm-cancel-pasted = Cancel only forgets the pasted token here. The app it came from stays signed in.
confirm-cancel-issued = Cancel ends this new session on the server.
confirm-continue = Continue

## The backend thread stopped

fatal-title = The network part of Inlet stopped.
fatal-hint = Restart it to reconnect. Your session is kept.
fatal-restart = Restart backend

## Notices after sign-out

notice-pasted-forgotten = The pasted token was only forgotten here. The app it came from stays signed in.
notice-not-deleted = The token could not be removed from the system keychain.
notice-sign-out-not-revoked = Signed out here, but the instance could not be told. End the session under Devices in Fluxer's settings.
notice-reject-not-revoked = The instance could not be told to end that session. End it under Devices in Fluxer's settings.

## Connection status (the line at the right of the channel header)

status-connecting = Connecting…
status-connecting-attempt = Connecting (attempt { $attempt })…
status-connected = Connected
status-disconnected = Disconnected
status-reconnecting = Reconnecting…
status-reconnecting-in = Disconnected, reconnecting in { $seconds } s…
status-gateway-reconnecting-in = Reconnecting in { $seconds } s…

## Why the gateway stopped

gateway-auth-failed = The instance ended this session. Sign in again.
gateway-api-version = The instance uses a newer gateway version than this Inlet build supports.
gateway-invalid-shard = The instance refused the gateway connection (invalid shard).
gateway-invalid-url = The instance's gateway address is not usable: { $url }
gateway-stopped = The gateway stopped: { $reason }

## Errors

error-timeout = The instance did not answer in time.
error-unauthorized = The instance did not accept the session. Sign in again.
error-global-rate-limit = Fluxer's global rate limit was hit, which ends the session. Sign in again.
error-api-refused = The instance refused: { $error }
error-unreachable = Could not reach the instance. Check the address and your connection.
error-endpoints-changed = This server's addresses changed. Sign in again.
error-bad-address = The instance address is not usable: { $reason }
error-too-large = The instance sent more data than Inlet accepts.
error-keychain-refused = Signed in, but the system keychain refused the token. You will have to sign in again next time.
error-task-panicked = Part of Inlet stopped unexpectedly ({ $what }). Try again.

## Several accounts (the account switcher)

accounts-title = Accounts
accounts-add = Add an account
accounts-full = At most { $max } accounts can be signed in.
accounts-active = Active
accounts-close = Close
accounts-handle-at = { $handle } on { $instance }
accounts-ended = { $account }: { $message }
sign-in-back = Back
sign-in-too-many-accounts = At most { $max } accounts can be signed in at once. Sign one out first.
error-account-id = The instance sent an account ID that Inlet cannot use.
